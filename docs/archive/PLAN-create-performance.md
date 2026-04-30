# Plan: Sandbox Creation Performance Instrumentation

## The Problem

`bhatti create` takes **~1.5 seconds** when it works, but frequently
times out at **30 seconds** with `agent not ready: context deadline
exceeded`. The failure pattern is bimodal — no middle ground.

On a clean server (no orphaned resources), both `minimal` and `docker`
tiers create in ~1.5s server-side. The ~9.5s timing seen earlier was
a mix of one successful create and the WaitReady 30s timeout averaged
across multiple attempts.

The real problem is **reliability, not raw speed.**

### What we measured

**Individual steps in isolation (on the btrfs server, quiescent):**

| Step | Time |
|------|------|
| Rootfs reflink copy (2GB, same btrfs) | ~43ms |
| e2fsck on reflinked rootfs | ~39ms |
| truncate to 8192MB | ~1ms |
| resize2fs | ~39ms |
| Lohar injection (mount + cp + umount) | ~34ms |
| Config drive creation | ~2ms |
| Jail hardlinks + chown | ~5ms |
| FC process start + socket ready | ~3ms |
| FC API call (each PUT) | ~10ms |
| **Sum of isolated measurements** | **~210ms** |

**Actual end-to-end (3 runs each, sequential, destroy between):**

| Scenario | Run 1 | Run 2 | Run 3 |
|----------|-------|-------|-------|
| minimal no-resize | 26.7s ❌ | 30.5s ❌ | 1.5s ✅ |
| docker no-resize | 31.4s ❌ | 1.5s ✅ | 30.7s ❌ |

**Pattern: bimodal — 1.5s or 30s timeout. No middle ground.**

**When it works (clean server, no orphaned resources):**

| Scenario | Server time |
|----------|-------------|
| minimal no-resize | 1.51s |
| docker no-resize | 1.49s |
| docker + disk-size 8192 | 2.18s |

The ~1.5s is kernel boot + lohar init + WaitReady. The extra ~0.7s
with disk-size is e2fsck + truncate + resize2fs.

### The real problem: orphaned resources

During testing we discovered **44 orphaned TAP devices** and **9
orphaned FC processes** on a server with only 2 running sandboxes.
These were left behind by creates that failed during WaitReady (the
30s timeout path) — the cleanup in Create's defer doesn't fully
cover the failure path, or failed creates that never got stored in
the DB don't get their TAP/FC cleaned up on destroy.

The orphaned TAPs pollute the bridge. New VMs get IPs that can't
route through the contaminated bridge, causing WaitReady to timeout,
which creates MORE orphaned TAPs. It's a cascading failure.

**This is why creates are bimodal**: if the new VM gets an IP that
happens to work around the orphaned TAPs, it succeeds in 1.5s. If
not, it times out at 30s and leaves another orphan.

### What we DON'T know

1. Whether the orphaned TAP/FC cleanup is missing from the error path
   in Create(), or if it's a destroy-path bug for sandboxes that
   failed to fully initialize
2. Exact kernel boot time (need lohar boot timing instrumentation)
3. How many WaitReady probe attempts happen on a SUCCESSFUL create
   (is 1.5s = 1 attempt or 2 attempts?)
4. Whether the orphaned resources also leak IP pool allocations,
   causing address exhaustion
5. Whether a bhatti server restart cleans up orphaned TAPs (the
   `cleanupOrphanedTapDevices` function exists but may not cover
   all cases)

## Critical Bugs Found During Investigation

### Bug 1: Failed creates leak TAP devices and FC processes

When `WaitReady` times out (30s), the create path returns an error.
The defer cleanup in `Create()` runs, but either:
- It doesn't destroy the TAP device
- It doesn't kill the FC/jailer process
- The cleanup itself fails silently

Evidence: 44 TAP devices and 9 FC processes on a server with 2
running sandboxes. The excess came from our benchmark creates that
timed out.

This causes cascading failures: orphaned TAPs pollute the bridge,
new VMs can't route, they timeout too, creating more orphans.

**Severity: Critical.** A burst of failed creates can permanently
break sandbox creation until the server is restarted.

**Fix:** Audit the defer chain in `Create()`. Ensure that on ANY
error after TAP creation, the TAP is destroyed. Same for the FC
process — if WaitReady fails, the FC and jailer MUST be killed.
The cleanup function at the top of Create() needs to handle:

```go
defer func() {
    if err != nil {
        if tapName != "" {
            destroyTapDevice(tapName)
        }
        if fcCmd != nil {
            killFC(fcCmd, 1*time.Second)
        }
        // remove jail directory
        // release IP back to pool
    }
}()
```

Also add a startup recovery path: on `bhatti serve` start, compare
running FC processes and existing TAPs against the DB/in-memory VM
map. Kill/remove anything orphaned. The `cleanupOrphanedTapDevices`
function exists but may not be called on the right path.

### Bug 2: Orphaned TAPs survive sandbox destroy

When a sandbox that failed to fully initialize is destroyed, the
destroy path may not find the TAP (because the sandbox was never
stored in the VM map with its TAP name). The TAP persists.

**Fix:** `Destroy()` should also attempt to clean up `tap{id[:8]}`
by name convention, even if the VM isn't in the map.

### Bug 3: IP pool leaks on failed creates

If `Create()` allocates an IP but fails during WaitReady, does the
IP get released back to the pool? If not, the /24 subnet (253 IPs)
exhausts after 253 failed creates. Need to verify the defer chain
releases the IP on error.

## The Instrumentation Plan

Add `slog.Debug` timing to every phase of `Create()`. Zero behavioral
changes — just logging. Each log line includes a `phase` field and
`elapsed_ms` since the start of the create call.

### Phase 1: Engine Create path (`pkg/engine/firecracker/create.go`)

Add a timing helper at the top of `Create()`:

```go
func (e *Engine) Create(ctx context.Context, spec engine.SandboxSpec) (info engine.SandboxInfo, err error) {
    t0 := time.Now()
    phase := func(name string) {
        slog.Debug("create.phase", "sandbox", spec.Name, "phase", name,
            "elapsed_ms", time.Since(t0).Milliseconds())
    }
```

Then instrument every step:

```go
    // 1. Copy rootfs
    phase("rootfs_copy_start")
    if err = copyRootfs(baseImage, rootfsPath); err != nil { ... }
    phase("rootfs_copy_done")

    // 1b. Inject lohar
    phase("lohar_inject_start")
    if err = injectLoharIntoRootfs(rootfsPath, e.cfg.DataDir); err != nil { ... }
    phase("lohar_inject_done")

    // 1c. Resize
    if spec.DiskSizeMB > 0 {
        phase("resize_start")
        exec.Command("e2fsck", "-f", "-y", rootfsPath).Run()
        phase("e2fsck_done")
        exec.Command("truncate", "-s", fmt.Sprintf("%dM", spec.DiskSizeMB), rootfsPath).Run()
        phase("truncate_done")
        exec.Command("resize2fs", rootfsPath).Run()
        phase("resize2fs_done")
    }

    // 2. Allocate CID + paths
    phase("alloc_cid")

    // 3. Network: ensure bridge, allocate IP, create TAP
    phase("network_start")
    if err = ensureUserBridge(userNet); err != nil { ... }
    phase("bridge_done")
    guestIP, err := userNet.Pool.Allocate()
    phase("ip_alloc_done")
    tapName, err = createTapDevice(id, userNet.BridgeName)
    phase("tap_done")

    // 4. Config drive
    phase("config_drive_start")
    if err = createConfigDrive(configDrivePath, SandboxConfig{...}); err != nil { ... }
    phase("config_drive_done")

    // 5. Resolve paths for jailer
    phase("resolve_paths")

    // 6. Start FC (jailed)
    phase("fc_start_begin")
    fcProc, err := e.startFC(socketPath, startFCOpts{...})
    phase("fc_start_done")  // includes jailer setup + socket ready wait

    // 7. FC API configuration
    phase("fc_api_start")
    fcPut(ctx, client, "/logger", ...)
    phase("fc_api_logger")
    fcPut(ctx, client, "/metrics", ...)
    phase("fc_api_metrics")
    fcPut(ctx, client, "/boot-source", ...)
    phase("fc_api_boot_source")
    fcPut(ctx, client, "/drives/rootfs", ...)
    phase("fc_api_drive_rootfs")
    fcPut(ctx, client, "/machine-config", ...)
    phase("fc_api_machine_config")
    fcPut(ctx, client, "/entropy", ...)
    phase("fc_api_entropy")
    fcPut(ctx, client, "/balloon", ...)
    phase("fc_api_balloon")
    fcPut(ctx, client, "/vsock", ...)
    phase("fc_api_vsock")
    fcPut(ctx, client, "/network-interfaces/eth0", ...)
    phase("fc_api_network")
    fcPut(ctx, client, "/drives/config", ...)
    phase("fc_api_drive_config")
    // + volume drives
    phase("fc_api_volumes_done")

    // 8. Boot
    phase("instance_start")
    fcPut(ctx, client, "/actions", `{"action_type":"InstanceStart"}`)
    phase("instance_started")

    // 9. Wait for agent
    phase("wait_ready_start")
    agentClient.WaitReady(ctx, 30*time.Second)
    phase("wait_ready_done")

    // 10. Store VM
    phase("store_vm")
    e.mu.Lock()
    e.vms[id] = vm
    e.mu.Unlock()
    phase("create_complete")
```

### Phase 2: WaitReady detail (`pkg/agent/client.go`)

The WaitReady loop is the prime suspect. Log every attempt:

```go
func (c *AgentClient) WaitReady(ctx context.Context, timeout time.Duration) error {
    t0 := time.Now()
    attempt := 0

    for {
        attempt++
        attemptStart := time.Now()

        execCtx, execCancel := context.WithTimeout(ctx, 2*time.Second)
        result, err := c.Exec(execCtx, []string{"true"}, nil, "")
        execCancel()

        attemptDur := time.Since(attemptStart)

        if err == nil && result.ExitCode == 0 {
            slog.Debug("wait_ready.success",
                "attempt", attempt,
                "attempt_ms", attemptDur.Milliseconds(),
                "total_ms", time.Since(t0).Milliseconds(),
                "addr", c.tcpAddr)
            return nil
        }

        slog.Debug("wait_ready.attempt_failed",
            "attempt", attempt,
            "attempt_ms", attemptDur.Milliseconds(),
            "total_ms", time.Since(t0).Milliseconds(),
            "error", err,
            "addr", c.tcpAddr)

        select {
        case <-ctx.Done():
            return fmt.Errorf("agent not ready after %d attempts (%dms): %w",
                attempt, time.Since(t0).Milliseconds(), ctx.Err())
        case <-time.After(100 * time.Millisecond):
        }
    }
}
```

This will show:
- How many attempts before success
- Whether each attempt burns the full 2s timeout or fails faster
- The exact error on each failure (connection refused vs timeout vs
  protocol error)

### Phase 3: Jailed FC startup detail (`pkg/engine/firecracker/fc.go`)

The `startFCJailed` function has a socket polling loop. Instrument it:

```go
func (e *Engine) startFCJailed(socketPath string, opts startFCOpts) (*fcProcess, error) {
    t0 := time.Now()
    phase := func(name string) {
        slog.Debug("fc_jail.phase", "sandbox", opts.id, "phase", name,
            "elapsed_ms", time.Since(t0).Milliseconds())
    }

    phase("mkdir_jail")
    // ... create jail dir ...

    phase("hardlink_files")
    // ... hardlink loop ...

    phase("chown_walk")
    // ... filepath.Walk chown ...

    phase("jailer_cmd_start")
    cmd.Start()
    phase("jailer_started")

    // Socket polling loop:
    for i := 0; i < 100; i++ {
        if _, err := os.Stat(hostSock); err == nil {
            phase(fmt.Sprintf("socket_ready_iter_%d", i))
            break
        }
        time.Sleep(20 * time.Millisecond)
    }
    phase("socket_ready")
```

### Phase 4: Lohar boot timing (`cmd/lohar/main.go`)

Lohar runs inside the VM. Its stderr goes to FC's stderr buffer.
Add timestamps to the boot sequence so we can see kernel→userspace
and each init phase:

```go
func main() {
    bootStart := time.Now()
    bp := func(name string) {
        fmt.Fprintf(os.Stderr, "lohar: boot %s +%dms\n",
            name, time.Since(bootStart).Milliseconds())
    }

    bp("start")                    // first line of main()

    // Mounts
    mustMount("proc", ...)
    mustMount("sysfs", ...)
    // ...
    bp("mounts_done")

    bringUpInterface("lo")
    bp("lo_up")

    cfg := loadConfigDrive()
    bp("config_loaded")

    // hostname, hosts, dns, env, files, volumes
    bp("config_applied")

    setupNetworking()
    bp("network_done")

    // vsock + TCP listeners
    lnControl, _ := listenVsock(...)
    bp("vsock_listen")

    tcpControl, _ := net.Listen("tcp", ...)
    tcpForward, _ := net.Listen("tcp", ...)
    bp("tcp_listen")              // ← WaitReady can succeed after this

    fmt.Fprintln(os.Stderr, "lohar: ready")

    // Boot profile
    if _, err := os.Stat("/etc/bhatti/init.sh"); err == nil {
        bp("boot_profile_start")
        cmd.Run()
        bp("boot_profile_done")
    }
```

To read these after boot:
```bash
bhatti exec <sandbox> -- sudo cat /dev/null
# FC stderr is captured in the stderrBuf ring buffer.
# Alternatively, read the FC log file.
```

Actually, the FC log goes to a file (`logRef` in the jail). The lohar
stderr goes to FC's captured stderr. We need to surface it.

**Better approach:** Write boot timing to a file inside the VM that
can be read via `bhatti exec` or `bhatti file read`:

```go
func main() {
    bootStart := time.Now()
    var bootLog strings.Builder
    bp := func(name string) {
        line := fmt.Sprintf("+%dms %s\n", time.Since(bootStart).Milliseconds(), name)
        fmt.Fprint(os.Stderr, "lohar: boot "+line)
        bootLog.WriteString(line)
    }

    // ... all the boot phases ...

    bp("tcp_listen")
    // Write boot timing to a file accessible via bhatti file read
    os.WriteFile("/tmp/boot-timing.txt", []byte(bootLog.String()), 0644)
```

Then after create:
```bash
bhatti file read <sandbox> /tmp/boot-timing.txt
```

### Phase 5: Server-side handler (`pkg/server/sandbox_handlers.go`)

The handler does work before and after calling `engine.Create()`:

```go
func (s *Server) handleSandboxes(w http.ResponseWriter, r *http.Request) {
    // ... POST handler ...
    t0 := time.Now()

    // Template resolution, volume resolution, secret decryption
    slog.Debug("create.handler", "phase", "request_parsed",
        "elapsed_ms", time.Since(t0).Milliseconds())

    // Volume resolution
    slog.Debug("create.handler", "phase", "volumes_resolved",
        "elapsed_ms", time.Since(t0).Milliseconds(),
        "volume_count", len(resolvedVols))

    // Engine create
    slog.Debug("create.handler", "phase", "engine_create_start",
        "elapsed_ms", time.Since(t0).Milliseconds())

    info, err := s.engine.Create(ctx, spec)

    slog.Debug("create.handler", "phase", "engine_create_done",
        "elapsed_ms", time.Since(t0).Milliseconds())

    // Store in DB
    slog.Debug("create.handler", "phase", "db_store_done",
        "elapsed_ms", time.Since(t0).Milliseconds())
```

This will catch any time spent in:
- JSON parsing / request validation
- Template lookup + secret decryption
- Volume resolution (finding volume files on disk)
- DB insert after engine.Create returns
- Any middleware overhead

### Phase 6: Mutex contention check

The engine has a global mutex. If the thermal manager or another
request holds it during create, we'd see a gap:

```go
// In Create():
phase("mu_lock_wait")
e.mu.Lock()
e.vms[id] = vm
e.mu.Unlock()
phase("mu_lock_done")
```

Also check if `getOrCreateUserNetwork` holds a lock:

```go
func (e *Engine) getOrCreateUserNetwork(userID string, subnetIndex int) *UserNetwork {
    e.mu.Lock()         // ← this could block if thermal manager holds it
    defer e.mu.Unlock()
    // ...
}
```

The thermal manager runs every 10s and iterates all VMs under the
same lock:

```go
// In thermal loop:
e.mu.RLock()
for _, vm := range e.vms { ... }
e.mu.RUnlock()
```

This is an RLock so it shouldn't block Create's write lock unless
a write is already pending. But worth verifying.

## Expected output

After all instrumentation, a create would log something like:

```
DEBUG create.handler phase=request_parsed elapsed_ms=0
DEBUG create.handler phase=volumes_resolved elapsed_ms=2 volume_count=1
DEBUG create.handler phase=engine_create_start elapsed_ms=2
DEBUG create.phase sandbox=k3s-s1 phase=rootfs_copy_start elapsed_ms=0
DEBUG create.phase sandbox=k3s-s1 phase=rootfs_copy_done elapsed_ms=45
DEBUG create.phase sandbox=k3s-s1 phase=lohar_inject_start elapsed_ms=45
DEBUG create.phase sandbox=k3s-s1 phase=lohar_inject_done elapsed_ms=80
DEBUG create.phase sandbox=k3s-s1 phase=resize_start elapsed_ms=80
DEBUG create.phase sandbox=k3s-s1 phase=e2fsck_done elapsed_ms=120
DEBUG create.phase sandbox=k3s-s1 phase=truncate_done elapsed_ms=121
DEBUG create.phase sandbox=k3s-s1 phase=resize2fs_done elapsed_ms=160
DEBUG create.phase sandbox=k3s-s1 phase=network_start elapsed_ms=160
DEBUG create.phase sandbox=k3s-s1 phase=tap_done elapsed_ms=165
DEBUG create.phase sandbox=k3s-s1 phase=config_drive_done elapsed_ms=170
DEBUG create.phase sandbox=k3s-s1 phase=fc_start_begin elapsed_ms=175
DEBUG fc_jail.phase sandbox=k3s-s1 phase=jailer_started elapsed_ms=5
DEBUG fc_jail.phase sandbox=k3s-s1 phase=socket_ready elapsed_ms=25
DEBUG create.phase sandbox=k3s-s1 phase=fc_start_done elapsed_ms=200
DEBUG create.phase sandbox=k3s-s1 phase=fc_api_start elapsed_ms=200
DEBUG create.phase sandbox=k3s-s1 phase=fc_api_boot_source elapsed_ms=220
... (8 more API calls, ~10ms each)
DEBUG create.phase sandbox=k3s-s1 phase=instance_start elapsed_ms=300
DEBUG create.phase sandbox=k3s-s1 phase=instance_started elapsed_ms=310
DEBUG create.phase sandbox=k3s-s1 phase=wait_ready_start elapsed_ms=310
DEBUG wait_ready.attempt_failed attempt=1 attempt_ms=2003 error="dial tcp 10.0.1.4:1024: i/o timeout" total_ms=2313
DEBUG wait_ready.attempt_failed attempt=2 attempt_ms=2001 error="dial tcp 10.0.1.4:1024: i/o timeout" total_ms=4414
DEBUG wait_ready.attempt_failed attempt=3 attempt_ms=2002 error="..." total_ms=6516
DEBUG wait_ready.success attempt=4 attempt_ms=150 total_ms=6766
DEBUG create.phase sandbox=k3s-s1 phase=wait_ready_done elapsed_ms=7076
DEBUG create.phase sandbox=k3s-s1 phase=create_complete elapsed_ms=7080
DEBUG create.handler phase=engine_create_done elapsed_ms=7082
DEBUG create.handler phase=db_store_done elapsed_ms=7085
```

This would prove (or disprove) that WaitReady is the bottleneck and
show exactly how many 2-second TCP timeout cycles are wasted.

The lohar boot timing (from inside the VM) would show:

```
+0ms start
+5ms mounts_done
+6ms lo_up
+15ms config_loaded
+20ms config_applied
+25ms network_done
+26ms vsock_listen
+28ms tcp_listen          ← VM is reachable here
+30ms boot_profile_start
+8500ms boot_profile_done ← dockerd took 8.5s to start
```

If the VM is reachable at +28ms but WaitReady burns 2 seconds per
attempt, the fix is obvious: **shorter initial TCP connect timeout
with exponential backoff** (50ms → 100ms → 200ms → 400ms → 1s → 2s).
The first probe would catch the VM at ~50ms after boot.

If the VM itself takes seconds to reach `tcp_listen`, the fix is in
lohar (lazy-load config drive, defer heavy mounts, etc).

## Implementation notes

- All instrumentation uses `slog.Debug` — invisible at default log
  level (`INFO`). Enable with `--log-level debug` or env var.
- The lohar boot timing writes to `/tmp/boot-timing.txt` — readable
  after create via `bhatti file read`, no protocol changes needed.
- Zero behavioral changes. Every log line is fire-and-forget.
- The `phase()` helper is 3 lines of code, inlined per function.
  No new packages, no interfaces, no config.

## What to do after

### Priority 1: Fix the resource leak (critical)

The orphaned TAP/FC leak is the #1 issue. A burst of failed creates
(e.g., from the k3s setup script creating 5 sandboxes) can cascade
into permanent creation failures until server restart.

1. Audit and fix the defer chain in `Create()` for TAP + FC + IP cleanup
2. Add startup recovery: clean orphaned TAPs/FCs on `bhatti serve` start
3. Add `Destroy()` fallback cleanup by TAP name convention
4. Test: create 10 sandboxes, kill bhatti mid-create, restart, verify
   no orphans remain

### Priority 2: Reduce WaitReady overhead

Even when creates succeed, the WaitReady polling pattern wastes time:

| Current | Proposed |
|---------|----------|
| 2s TCP connect timeout per attempt | Exponential backoff: 50ms, 100ms, 200ms, 400ms, 1s, 2s |
| 100ms fixed sleep between retries | Included in the backoff |
| ~1.5s total on success | Target: ~200-500ms |

If the VM is reachable at ~200ms after InstanceStart (plausible for
a stripped kernel + fast lohar init), the first 50ms probe would fail,
the 100ms probe would catch it. Total WaitReady: ~150ms.

Alternative: have lohar write a readiness sentinel to a shared
file (via virtio-9p or a second virtio-blk device) instead of
polling TCP. The host watches for the file. Eliminates TCP connect
overhead entirely.

### Priority 3: Eliminate runtime resize

The `--disk-size` flag adds e2fsck + truncate + resize2fs to the
create path. For k3s nodes that always use 8GB, pre-bake the image
at that size:

```bash
# Build docker tier at 8GB instead of 2GB
SIZE_MB=8192 sudo ./scripts/build-tier.sh docker amd64 ./lohar
```

Or save a pre-resized golden image:
```bash
bhatti create --name base --image docker --disk-size 8192
bhatti image save base --name docker-8g
bhatti destroy base
# Now: bhatti create --image docker-8g (no resize needed)
```

This saves ~700ms per create.

### Priority 4: Speed targets

Once the leak is fixed and WaitReady is optimized:

| Scenario | Current | Target |
|----------|---------|--------|
| minimal no-resize | ~1.5s | <500ms |
| docker no-resize | ~1.5s | <500ms |
| docker + disk-size 8192 | ~2.2s | <800ms |
| k3s 5-node cluster | ~90s | <30s |

The 90s → 30s k3s improvement comes from:
- Pre-baked k3s image (saves 30s of downloads)
- Faster creates with WaitReady fix (saves ~5s)
- Parallel server join where possible (saves ~15s)
- No `--disk-size` resize (saves ~3.5s)

The most likely fix (WaitReady backoff) is a 10-line change. But
we should prove it before shipping it.

## Files to modify

| File | Changes |
|------|---------|
| `pkg/engine/firecracker/create.go` | `phase()` helper + 25 log lines |
| `pkg/engine/firecracker/fc.go` | 6 log lines in `startFCJailed` |
| `pkg/agent/client.go` | 2 log lines in `WaitReady` loop |
| `pkg/server/sandbox_handlers.go` | 5 log lines in POST handler |
| `cmd/lohar/main.go` | `bp()` helper + 12 log lines + write `/tmp/boot-timing.txt` |

Total: ~50 lines of logging. No new dependencies. No behavioral
changes. Ship it, run one create with `--log-level debug`, read the
output, fix the actual bottleneck.

---

# Addendum: Measured Results (April 25, 2026)

*Benchmarked on production agni-01 via curl to localhost:8080. No sandbox creation/destruction of other users' VMs.*

```
Host: agni-01 | AMD Ryzen 9 3900 24-core | 125GB RAM
Disk: btrfs+zstd on NVMe (via loop device)
Jailer: enabled | 4 users | 8 running VMs (all keep_hot) | 37 stopped
```

## Server-Side Latencies

| Operation | p50 | Range | n |
|---|---|---|---|
| Exec `true` (hot) | **12.5ms** | 10.7–13.7ms | 20 |
| File read 1KB (hot) | **12.6ms** | 11.4–14.4ms | 10 |
| Warm resume + exec | **17ms** | 14–20ms | 2 |
| Destroy | **80ms** | 70–96ms | 5 |
| Cold resume (`start`) | **331ms** | 330–334ms | 5 |
| Stop (snapshot, 512MB VM) | **481ms** | 424–550ms | 5 |
| Create (1vCPU/512MB) | **~1,200ms** | when healthy | 4 |

## Root Cause of Bimodal Creates: Stale ARP

The bimodal 1.5s/30s pattern from the original investigation above has
a confirmed root cause: **stale ARP cache entries from IP reuse.**

When a sandbox is destroyed and a new one created immediately, the IP
pool recycles the freed IP (e.g. `10.0.1.7`). The new VM gets a fresh
random MAC. But the host's ARP cache still maps `10.0.1.7 → old MAC`
as `STALE`:

```
$ ip neigh show 10.0.1.7 dev brbhatti-1
10.0.1.7 lladdr 02:66:65:ec:55:b4 STALE
```

The kernel sends the TCP SYN to the stale MAC — which no longer exists
on any TAP device. The packet is silently dropped. ARP re-resolution
doesn't happen until the stale entry expires (`gc_stale_time=60s`).
WaitReady times out at 30s.

The create that succeeds is the one where the IP is fresh (no stale ARP
entry). The create that fails reuses an IP with a stale ARP→MAC mapping.

**This is NOT the orphaned-TAP cascading failure** described above — it's
a separate, simpler bug. The orphaned TAP/FC leak (from failed creates
not cleaning up) compounds the problem but is not the primary trigger.

**Fix** — one line in `create.go`, after `createTapDevice()` and before
`startFC()`:

```go
exec.Command("ip", "neigh", "del", guestIP, "dev", userNet.BridgeName).Run()
```

The snapshot resume path already does `bridge fdb del` for the same
reason. The create path just doesn't have it.

**Verification**: `forward_delay` was investigated and ruled out — with
`stp_state=0`, the kernel sets new bridge ports to forwarding immediately
regardless of the `forward_delay` value. Confirmed by creating a test TAP
and reading `/sys/class/net/tapbenchtest/brport/state` (state=0/disabled,
transitions to 3/forwarding when FC connects virtio-net).

## Memory Footprint

**Bhatti server**: **26 MB RSS** (12.5 MB heap, 13.5 MB binary/libs).
22 threads, 30 FDs, 5 sockets.

**Firecracker VMs** (all 8 are `keep_hot=1`, across 2 users):

| Sandbox | User | Allocated | RSS | Used% |
|---|---|---|---|---|
| rory | kowshik | 4,096 MB | 2,514 MB | 61% |
| alice | kowshik | 4,096 MB | 428 MB | 10% |
| karkhana-ME-29 | admin | 2,048 MB | 394 MB | 19% |
| eric | kowshik | 4,096 MB | 218 MB | 5% |
| finn-agent | kowshik | 4,096 MB | 168 MB | 4% |
| uyir | kowshik | 4,096 MB | 160 MB | 3% |
| karkhana-orchestrator | admin | 2,048 MB | 118 MB | 5% |
| test-agent-123 | kowshik | 4,096 MB | 80 MB | 1% |
| **Total** | | **30,720 MB** | **4,085 MB** | **13%** |

KVM demand-paging: only 13% of allocated memory is resident. Healthy.

**TAP devices**: 45 on host = 45 non-destroyed sandboxes in DB. **No
leaks.** 8 for running VMs, 37 intentionally kept for cold resume.
(Initial analysis incorrectly flagged kowshik's VMs as orphans because
the `/sandboxes` API is user-scoped — all 8 FC processes map to real
sandboxes.)

**Disk**: 360 GB used of 1 TB btrfs. Jails dir (171 GB) is mostly
hard-links sharing inodes with sandboxes dir (237 GB).

## Kernel Boot Timeline (from dmesg inside guest)

Created a sandbox and ran `dmesg` inside it. Kernel 6.1.155.

```
0.000s  Kernel start (Linux 6.1.155, SMP PREEMPT_DYNAMIC)
0.007s  ACPI/APIC/SMP init
0.022s  TCP/UDP/UNIX networking stack initialized
0.038s  Core kernel init done (virtio-blk, loop, i8042, bridge, vsock)
        ──── 520ms gap: kernel IP-Config carrier wait ────
0.557s  AT keyboard input registered (i8042 probe completed)
0.577s  IP-Config: eth0 configured (10.0.1.7)
0.579s  EXT4: root filesystem (vda) mounted
0.585s  "Run /usr/local/bin/lohar as init process"
0.592s  Config drive (vdb) mounted
0.596s  Config drive unmounted
        ──── lohar: mounts, networking, listeners ────
~0.7s   TCP listeners ready ("lohar: ready")
~0.8s   First successful WaitReady poll
```

**The 520ms gap (0.038s → 0.557s) is the kernel's IP-Config carrier
wait.** The kernel source (`net/ipv4/ipconfig.c`) polls
`netif_carrier_ok()` every 1ms until the virtio-net device reports
carrier. Carrier arrives when the host-side TAP is UP and the FC
process connects its virtio-net backend.

This is not the kernel being slow — it's the kernel waiting for the
host to be ready. The TAP is created late in Create() (after rootfs
copy, lohar injection, config drive creation). The guest boots faster
than the host finishes prep.

### Why removing `ip=` is NOT the answer

`decisions.md` #11 explains: the host polls the agent via TCP to
detect readiness. If the agent configures networking, the host can't
reach the agent until networking is up. `ip=` breaks this chicken-and-
egg by configuring the network before init runs.

### The real lever: make carrier available earlier

Currently:
```
copyRootfs (5ms) → injectLohar (34ms) → configDrive (2ms) → allocIP+createTAP (30ms) → startFC (200ms)
                                                                                        ↑ carrier arrives
                                                              guest kernel boots ────────┘ waits ~350ms
```

Reordered:
```
allocIP+createTAP (30ms) → copyRootfs (5ms) → injectLohar (34ms) → configDrive (2ms) → startFC (200ms)
↑ TAP already up                                                                       ↑ carrier ~immediately
                                                              guest kernel boots ────────┘ IP-Config in ~5ms
```

With a TAP pool (pre-created at startup), TAP grab is ~1ms.

## Optimized Create Target: ~450ms

Using the step timings from the original instrumentation above (which
are more precise than my earlier estimates):

| Step | Current | Optimized | How |
|---|---|---|---|
| TAP + IP alloc | 30ms | ~1ms | Pre-created TAP pool |
| copyRootfs | 43ms | 43ms | Already fast (btrfs reflink) |
| injectLohar | 34ms | 0-10ms | Checksum skip for base images; e2cp fallback |
| createConfigDrive | 2ms | 2ms | Already fast |
| Jail hardlinks + chown | 5ms | 5ms | Already fast |
| FC process + socket | 3ms | 3ms | Already fast |
| FC API config (10 PUTs) | ~100ms | ~50ms | Keep-alive |
| InstanceStart → carrier | ~520ms | ~80ms | TAP pre-ready; carrier = FC virtio-net init only |
| Kernel IP-Config + root mount | ~60ms | ~50ms | Minimal; fast once carrier up |
| Lohar init | ~80ms | ~65ms | Minor |
| WaitReady poll alignment | ~50ms avg | ~5ms avg | 10ms initial poll interval |
| **Total** | **~1,200ms** | **~450ms** | |

With a stripped kernel (disable i8042, DHCP/BOOTP/RARP, unused FS,
`mitigations=off`): **~420ms**.

For reference: cold resume from snapshot = **331ms** (±3ms). A fully
optimized cold boot at ~450ms is only ~120ms slower than snapshot
resume.

## Stop Optimization: Balloon Before Snapshot

The thermal manager inflates the balloon to 50% on warm→cold transitions
but **not** on explicit `Stop()`. Adding `BalloonSet(memMiB/2)` before
`Pause` in `lifecycle.go Stop()` would:

1. Guest releases clean pages → RSS shrinks
2. Full snapshot writes fewer non-zero pages
3. btrfs+zstd compresses zeroed pages to nearly nothing

For a 512MB VM using ~100MB: snapshot drops from ~512MB to ~200MB write.
Estimated stop time: **~300ms** (down from ~480ms). For idle 4GB VMs:
~300ms (down from ~3-4s).

## Safety Audit: Cross-Reference Against Existing Decisions

Every suggestion checked against `decisions.md`, `PLAN-reliability.md`,
`kernel.md`, `networking.md`, and `thermal-management.md`.

| Suggestion | Safe? | Notes |
|---|---|---|
| Stale ARP flush on create | ✅ | Already done in snapshot resume; networking doc recommends it |
| Balloon before Stop() | ✅ | Already done in thermal warm→cold; same code |
| Tighter WaitReady polling | ✅ | This doc already proposes exponential backoff |
| Reorder Create() (TAP first) | ✅ | No dependency ordering issues; cleanup defer covers it |
| TAP pool | ✅ | 37 NO-CARRIER TAPs already on bridges without issue |
| HTTP keep-alive on FC client | ✅ | Only within single Create() call |
| Kernel stripping (i8042, DHCP, XFS) | ✅ | kernel.md confirms unused; FC base config includes them |
| Checksum-skip lohar injection | ⚠️ | Only safe for default base images, NOT OCI/saved images |
| e2cp for lohar injection | ⚠️ | e2cp may not be installed; actual savings ~24ms (34→10ms) |
| Raw config drive (no ext4) | ❌ | Protocol change; old lohars in snapshots would break |
| Remove kernel `ip=` | ❌ | Breaks chicken-and-egg per decisions.md #11 |
| Snapshot-based fast create | ✅ | Reliability plan Phase 8 already considers this |

## Raw Benchmark Data

### Exec `true` (hot, 20 runs, ms)
```
13.52 12.77 12.50 13.66 13.13 12.56 12.16 12.08 12.13 12.38
12.89 12.28 10.66 12.33 13.39 12.93 13.19 12.86 12.77 12.98
```

### Stop (5 runs, ms)
```
549.85  424.85  520.49  481.02  436.69
```

### Cold resume (5 runs, ms)
```
331.83  333.70  330.90  330.65  330.64
```

### Warm resume + exec (2 runs, ms)
```
20.39  14.44
```

### Create 1vCPU/512MB (successful, ms)
```
1248.98  1164.93  1161.72  1240.50
```
