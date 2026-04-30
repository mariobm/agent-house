# Bhatti v0.7 - Reliability & Production Hardening

v0.1 shipped multi-tenant security. v0.2 shipped CLI improvements. v0.3
shipped images, persistent volumes, named snapshots, OCI support. v0.4
shipped custom kernel, rootfs tiers, 30-second install. v0.5 shipped
public preview URLs with domain mode. v0.6 shipped CLI polish, local
image import, thermal fixes.

v0.7 makes bhatti dependable. The `rory` snapshot corruption incident
(April 1, 2026) exposed three compounding failures - silent thermal
skip, Diff snapshot corruption, and no snapshot verification - that
destroyed a persistent sandbox. The reliability audit and Firecracker
production hardening review together identified 30+ issues across seven
subsystems. This plan fixes all of them, in order of blast radius.

Two source documents:
- `RELIABILITY-AUDIT.md` - 17 bugs found via the rory incident
- `PLAN-production-hardening.md` - 10-part gap analysis against
  Firecracker's production deployment docs

---

## Current State

Bhatti works well on the happy path. A sandbox boots in <1s, resumes
from snapshot in <3ms, and serves exec/shell/file operations reliably.
But the system has no defense-in-depth:

**Snapshot correctness.** Diff snapshots can produce silently corrupt
artifacts. `vm.snap` is fully rewritten but `mem.snap` only has dirty
pages - if KVM's dirty page tracking misses host-side virtio ring writes,
the snapshot is corrupt. Discovered only on restore, possibly days later.
`SnapshotAll` (daemon shutdown) uses Diff for hot VMs, maximizing this
risk. No verification after snapshot creation.

**Thermal manager.** Activity query timeouts are silent - no log, no
counter, no escalation. A stuck guest agent keeps a VM hot forever,
burning host RAM and CPU. Sessions blocking pause are also silent.
Warm→cold transitions were fixed in v0.6 but the hot→warm silent-skip
path remains.

**Restore resilience.** FC stderr goes to daemon stderr, not captured
per-VM. Restore failures produce "agent not ready" with no root cause.
No circuit breaker - every API request retries a broken restore,
spawning zombie FC processes. No process reaping on failure paths.

**Firecracker hardening.** FC runs as root with full host filesystem
access, no chroot, no namespace, no cgroup limits. No rate limiters on
network or disk. No entropy device. Serial console enabled (guest can
flood host stderr). No FC-native logging or metrics.

**Operational gaps.** Name resolution requires two API calls. Volume
detachment can leak on partial destroy failures. `sync` errors before
snapshot are discarded.

---

## Design Principle: Fix What Burned Us First

The priority order follows blast radius, not implementation complexity.
A one-line change that prevents silent data corruption ships before a
multi-hour jailer integration that adds defense-in-depth. Every phase
is independently deployable and testable.

---

## Phase 1 - Stop the Bleeding

_Fixes the three failures that caused the rory incident. No new
features, no rearchitecture - just plugging the holes._

### 1.1 SnapshotAll Uses Full Snapshots

**Audit ref:** RELIABILITY-AUDIT #6, #4

Diff snapshots on hot VMs are the direct cause of the rory panic.
`SnapshotAll` runs on daemon shutdown - correctness matters infinitely
more than speed.

The Diff/Full decision lives in `Stop()`, not `SnapshotAll`. Since Diff
snapshots remain valid for thermal warm→cold transitions (VM is already
paused, dirty set is small, ring state is quiescent), `Stop()` needs
a way to distinguish the two callers.

**File:** `engine.go` (Stop signature), `server.go` (SnapshotAll)

Add a `StopOpts` parameter to `Stop()`:

```go
type StopOpts struct {
    ForceFullSnapshot bool // true for SnapshotAll (daemon shutdown)
}

func (e *Engine) Stop(ctx context.Context, id string, opts StopOpts) error {
    // ...
    snapshotType := "Full"
    if vm.hasBaseSnapshot && !opts.ForceFullSnapshot {
        // Diff only for thermal warm→cold (caller did not force Full)
        if _, err := os.Stat(vm.SnapMemPath); err != nil {
            slog.Warn("base snapshot missing, falling back to full",
                "id", id, "path", vm.SnapMemPath, "error", err)
            vm.hasBaseSnapshot = false
        } else {
            snapshotType = "Diff"
        }
    }
    // ...
}
```

In `SnapshotAll`, pass `ForceFullSnapshot: true`:

```go
if err := s.engine.Stop(ctx, sb.EngineID, engine.StopOpts{
    ForceFullSnapshot: true,
}); err != nil {
    // ...
}
```

**SnapshotAll must not swallow failures.** If a Full snapshot fails for
a sandbox, that sandbox is now unrecoverable after daemon restart. The
current code logs the error and continues shutdown — this is wrong.
Instead, SnapshotAll should retry once with a fresh context, and if
that also fails, the sandbox should remain running (skip the
`Process.Kill`) so it survives as a live process. On next daemon start,
recovery can detect the still-running FC process via its PID file and
reclaim it.

```go
if err := s.engine.Stop(ctx, sb.EngineID, engine.StopOpts{
    ForceFullSnapshot: true,
}); err != nil {
    slog.Error("snapshot-all: first attempt failed, retrying",
        "sandbox", sb.Name, "error", err)
    retryCtx, retryCancel := context.WithTimeout(context.Background(), 60*time.Second)
    defer retryCancel()
    if err := s.engine.Stop(retryCtx, sb.EngineID, engine.StopOpts{
        ForceFullSnapshot: true,
    }); err != nil {
        failed.Add(1)
        slog.Error("snapshot-all: retry failed, leaving VM running",
            "sandbox", sb.Name, "id", sb.ID, "error", err)
        // Do NOT kill the FC process — an unsnapshotted live VM is
        // better than an unrecoverable dead sandbox.
        return
    }
}
```

**Backward compatibility:** ✅ TRANSPARENT. Existing snapshots are
unaffected. New shutdown snapshots are Full instead of Diff - slightly
larger `mem.snap` but always correct.

**Test:**
- `TestSnapshotAllUsesFullSnapshot` - trigger SnapshotAll, verify
  `mem.snap` size equals VM memory (Full) not a fraction (Diff)
- `TestSnapshotAllRetryOnFailure` - mock first Stop() failure, verify
  retry is attempted
- `TestSnapshotAllLeavesVMRunningOnDoubleFailure` - mock both attempts
  failing, verify FC process is still alive

### 1.2 Log and Escalate Activity Query Failures

**Audit ref:** RELIABILITY-AUDIT #1, #2, #3

The thermal manager silently skips VMs when `Activity()` times out or
sessions block pause. After N consecutive failures, force-pause.

**File:** `server.go:396` (thermal cycle hot→warm path)

Add per-sandbox failure counter:

```go
type thermalFailures struct {
    mu       sync.Mutex
    counters map[string]int // engineID → consecutive failure count
}

const maxThermalFailures = 10 // force-pause after 10 consecutive failures
```

In the thermal cycle:

```go
activity, err := te.Activity(actCtx, sb.EngineID)
if err != nil {
    count := s.thermalFails.increment(sb.EngineID)
    slog.Warn("thermal activity query failed",
        "sandbox", sb.Name, "error", err,
        "consecutive_failures", count)
    if count >= maxThermalFailures {
        slog.Error("thermal force-pause: agent unresponsive",
            "sandbox", sb.Name, "failures", count)
        // Pause is a Firecracker API call - doesn't need the agent
        te.Pause(context.Background(), sb.EngineID)
        s.thermalFails.reset(sb.EngineID)
    }
    continue
}
s.thermalFails.reset(sb.EngineID) // success - reset counter

// Also log when sessions block pause:
if activity.AttachedSessions > 0 {
    slog.Info("thermal skip: active sessions",
        "sandbox", sb.Name, "sessions", activity.AttachedSessions)
    continue
}
```

**`keep_hot` sandboxes are exempt from force-pause.** The store already
has a `keep_hot` column. If `sb.KeepHot` is true, skip the escalation
counter entirely — these sandboxes are explicitly opted out of thermal
management by the user.

**Trade-off: force-pausing during heavy CPU-bound exec.** At 10-second
thermal intervals, `maxThermalFailures = 10` means 100 seconds of agent
silence before force-pause. A legitimately busy agent (heavy Exec
blocking the Activity goroutine) would be paused. This is acceptable —
100 seconds of unresponsive agent is indistinguishable from a stuck
agent, and the user can immediately `bhatti exec` to resume it.

**Backward compatibility:** ✅ TRANSPARENT. Only adds logging and a
safety net. No behavior change for healthy VMs.

**Tests:**
- `TestThermalActivityTimeoutLogged` - mock agent timeout, verify log
  output includes sandbox name and failure count
- `TestThermalForcePauseAfterNFailures` - mock 10 consecutive timeouts,
  verify Pause() is called
- `TestThermalSessionsBlockingLogged` - mock sessions > 0, verify log
- `TestThermalCounterResetsOnSuccess` - fail 5 times, succeed, fail 5
  more → no force-pause (counter reset at success)
- `TestThermalKeepHotExemptFromForcePause` - set keep_hot, mock 20
  consecutive timeouts, verify Pause() is NOT called

### 1.3 Circuit Breaker on Restore Failures

**Audit ref:** RELIABILITY-AUDIT #10, #9

When a snapshot is corrupt, every API request triggers `ensureHot` →
starts FC → loads snapshot → FC panics → 30s timeout → zombie. Repeat
on every request. Add a restore-failed flag with an explicit reset path.

**File:** `engine.go:735` (in `ensureHot`)

```go
type VM struct {
    // ... existing fields ...
    restoreFailed bool   // set on restore failure, blocks retries
    restoreError  string // the error message for user display
}

func (e *Engine) ensureHot(ctx context.Context, id string) error {
    vm, err := e.getVM(id)
    if err != nil {
        return err
    }
    vm.stateMu.Lock()
    defer vm.stateMu.Unlock()

    if vm.restoreFailed {
        return fmt.Errorf("sandbox %q snapshot is corrupt: %s - "+
            "use 'bhatti start --force' to retry or "+
            "destroy and recreate (volume data is safe)", id, vm.restoreError)
    }

    if vm.Thermal == "hot" {
        return nil
    }

    // ... existing restore logic ...
    if err := e.restoreFromSnapshot(ctx, vm); err != nil {
        vm.restoreFailed = true
        vm.restoreError = err.Error()
        // Reap the zombie
        if vm.cmd != nil && vm.cmd.Process != nil {
            vm.cmd.Process.Kill()
            vm.cmd.Wait()
        }
        return fmt.Errorf("restore failed: %w", err)
    }
    return nil
}
```

**Reset path:** `bhatti start --force` clears the circuit breaker and
retries. This handles transient host issues (OOM killed FC, disk full
that was since resolved) without forcing the user to destroy.

```go
// In Start(), when --force is set:
func (e *Engine) Start(ctx context.Context, id string, force bool) error {
    vm, err := e.getVM(id)
    if err != nil {
        return err
    }
    vm.stateMu.Lock()
    defer vm.stateMu.Unlock()

    if force {
        vm.restoreFailed = false
        vm.restoreError = ""
    }
    // ... proceed with normal restore ...
}
```

**Backward compatibility:** ✅ TRANSPARENT. Only changes error behavior
for already-broken sandboxes. Healthy sandboxes are unaffected.

**Tests:**
- `TestRestoreFailureSetsFlag` - corrupt snapshot, attempt exec,
  verify error message mentions "corrupt"
- `TestRestoreFailureBlocksRetry` - after first failure, second exec
  returns immediately with the same error (no new FC process)
- `TestRestoreFailureReapsZombie` - verify no zombie FC processes
  after a failed restore
- `TestStartForceResetsCircuitBreaker` - fail restore, fix snapshot,
  `start --force`, verify restore succeeds

### 1.4 Capture FC Stderr Per-VM

**Audit ref:** RELIABILITY-AUDIT #8. Also: PROD-HARDENING Part 3.

FC stderr is the only diagnostic channel when a snapshot restore fails.
Currently it goes to `os.Stderr` - invisible in structured logs, mixed
across all VMs.

**File:** `engine.go:419` (FC process launch), new `ringbuffer.go`

```go
// ringBuffer is a fixed-size circular buffer implementing io.Writer.
type ringBuffer struct {
    mu   sync.Mutex
    buf  []byte
    max  int
    pos  int
    full bool
}

func (r *ringBuffer) Write(p []byte) (int, error) { /* ... */ }
func (r *ringBuffer) String() string { /* ... */ }
```

Store on VM struct, use as FC stderr:

```go
type VM struct {
    // ... existing fields ...
    stderrBuf *ringBuffer // last 64KB of FC stderr
}

// In startFC / Create / Start / ResumeSnapshot:
stderrBuf := &ringBuffer{max: 64 * 1024}
fcCmd.Stderr = stderrBuf
vm.stderrBuf = stderrBuf
```

On restore failure, include FC stderr in error:

```go
if err != nil {
    fcStderr := ""
    if vm.stderrBuf != nil {
        fcStderr = vm.stderrBuf.String()
    }
    return fmt.Errorf("snapshot load failed: %w\nFC stderr: %s", err, fcStderr)
}
```

Note: this bounds host-side memory from FC stderr output at 64KB, but
does NOT address the serial console CPU overhead from guest writes —
that requires disabling the serial device entirely (Phase 3.1).

**Backward compatibility:** ✅ TRANSPARENT. No user-visible change
except better error messages.

**Tests:**
- `TestFCStderrCaptured` - corrupt snapshot, attempt restore, verify
  error message includes FC's panic text
- `TestRingBufferOverflow` - write >64KB, verify only last 64KB retained
- `TestRingBufferConcurrent` - concurrent writes don't panic

---

## Phase 2 - Snapshot Integrity

_Hardens the snapshot pipeline with lightweight sanity checks and
correctness fixes._

### 2.1 Snapshot Sanity Checks

**Audit ref:** RELIABILITY-AUDIT #5

Spawning a throwaway Firecracker process to verify snapshots is
impractical: FC v1.6+ requires `/snapshot/load` as the only pre-boot
API call (no drive pre-configuration), the rory bug manifested on
`resume_vm` not on load, and the overhead (~500ms per snapshot including
process spawn and socket wait) is too high for thermal snapshots
happening every 10 seconds across many VMs.

Instead, do lightweight sanity checks that catch the most common
corruption modes at near-zero cost:

**File:** `engine.go` (new function)

```go
func verifySnapshotArtifacts(vmSnapPath, memSnapPath string, memSizeMib int64, snapshotType string) error {
    // 1. vm.snap must be valid JSON (FC stores device state as JSON)
    vmData, err := os.ReadFile(vmSnapPath)
    if err != nil {
        return fmt.Errorf("read vm.snap: %w", err)
    }
    if !json.Valid(vmData) {
        return fmt.Errorf("vm.snap is not valid JSON (truncated or corrupt)")
    }

    // 2. mem.snap size sanity
    memInfo, err := os.Stat(memSnapPath)
    if err != nil {
        return fmt.Errorf("stat mem.snap: %w", err)
    }
    expectedFull := memSizeMib * 1024 * 1024
    if snapshotType == "Full" && memInfo.Size() != expectedFull {
        return fmt.Errorf("Full snapshot mem.snap size %d != expected %d (VM memory)",
            memInfo.Size(), expectedFull)
    }
    if snapshotType == "Diff" && (memInfo.Size() == 0 || memInfo.Size() > expectedFull) {
        return fmt.Errorf("Diff snapshot mem.snap size %d out of range (0, %d]",
            memInfo.Size(), expectedFull)
    }

    return nil
}
```

Called after every `/snapshot/create`:

```go
if err := verifySnapshotArtifacts(vm.SnapVMPath, vm.SnapMemPath, vm.MemSizeMib, snapshotType); err != nil {
    slog.Error("snapshot sanity check failed", "sandbox", id, "error", err)
    // For SnapshotAll: log and retry as Full
    // For Checkpoint: return error so user can retry
}
```

This catches truncated files, zero-byte files, and corrupt vm.snap —
the most common failure modes. The real fix for snapshot correctness
is Phase 1.1 (Full snapshots for SnapshotAll).

**Backward compatibility:** ✅ TRANSPARENT. Near-zero overhead
(stat + JSON parse). No external behavior change.

**Tests:**
- `TestSnapshotSanityCheckPasses` - normal snapshot, verify succeeds
- `TestSnapshotSanityDetectsTruncatedVMSnap` - truncate vm.snap,
  verify error
- `TestSnapshotSanityDetectsWrongMemSize` - Full snapshot with wrong
  mem.snap size, verify error
- `TestSnapshotSanityDetectsZeroMemSnap` - zero-byte mem.snap for
  Diff, verify error

### 2.2 Sync Before Snapshot of Hot VMs

**Audit ref:** RELIABILITY-AUDIT #4 (mitigation). PROD-HARDENING Part 7.

Run `sync` in the guest before pausing hot VMs. Check the error - don't
silently discard it. Log and continue in all cases — a stale snapshot
is better than no snapshot.

**File:** `engine.go:547` (in `Stop()`, before Pause)

```go
syncCtx, syncCancel := context.WithTimeout(context.Background(), 10*time.Second)
defer syncCancel()
if _, err := vm.Agent.Exec(syncCtx, []string{"sync"}, nil, ""); err != nil {
    slog.Warn("guest sync failed before snapshot",
        "sandbox", id, "error", err)
    // Continue — stale snapshot > no snapshot.
    // Checkpoint already does sync (snapshot.go ~line 85).
}
```

**Backward compatibility:** ✅ TRANSPARENT. Adds sync to `Stop()` path.
`Checkpoint()` already has sync — no change there.

**Tests:**
- `TestSyncBeforeSnapshot` - exec a write, stop, verify file persists
  on resume
- `TestSyncFailureLogged` - kill agent, stop, verify warning in logs
  and snapshot still succeeds

### 2.3 Reset `has_base_snapshot` on Recovery

**Audit ref:** RELIABILITY-AUDIT #7, #12

After daemon restart, `has_base_snapshot=true` persists but the base
it refers to may have been overwritten. Reset to false so the first
snapshot after restart is always Full.

**File:** `engine.go:953` (in `RestoreVM` / `recoverVMs`)

```go
// In RestoreVM, always reset:
vm.hasBaseSnapshot = false
// First snapshot after recovery will be Full, establishing a clean base
```

The reset must also be persisted to the store via `saveVMState()` after
`RestoreVM` completes. Otherwise the stale `has_base_snapshot=true` is
read again on the next daemon restart. Verify that the recovery path
in `server.go` calls `saveVMState()` after `RestoreVM` — if not, add
it.

**Backward compatibility:** ✅ TRANSPARENT. First post-recovery snapshot
is slightly larger (Full vs Diff). Subsequent snapshots resume normal
Diff behavior.

**Test:**
- `TestRecoveryResetsBaseSnapshot` - create sandbox, take snapshot
  (sets has_base_snapshot), restart daemon, verify has_base_snapshot=false
  both in-memory and in the store

---

## Phase 3 - Firecracker Hardening + Network Override

_Low-risk changes from the production hardening audit plus adoption of
Firecracker's `network_overrides` to eliminate fragile TAP name matching.
Each is independently deployable._

### 3.1 Disable Serial Console

**Audit ref:** PROD-HARDENING Part 3

Add `8250.nr_uarts=0` to boot args, remove `console=ttyS0`:

**File:** `engine.go` (boot args construction)

```go
bootArgs := fmt.Sprintf(
    "reboot=k panic=1 pci=off 8250.nr_uarts=0 init=/usr/local/bin/lohar "+
    "quiet loglevel=0 ip=%s::%s:255.255.255.0::eth0:off:1.1.1.1:8.8.8.8:",
    guestIP, userNet.GatewayIP)
```

This eliminates the CPU overhead from serial device emulation when the
guest writes to the console. Combined with Phase 1.4's ring buffer,
FC stderr is bounded from both FC's own output and any residual serial
output on older snapshots.

**Backward compatibility:** ✅ TRANSPARENT. Boot args are only used on
fresh boot, not snapshot resume. Existing cold snapshots have old args
baked in but they're not re-applied on resume.

**Test:**
- `TestSerialConsoleDisabled` - boot VM, verify
  `cat /proc/cmdline | grep 8250.nr_uarts=0`

### 3.2 Network Rate Limiters

**Audit ref:** PROD-HARDENING Part 4

Add rate limiters to network interfaces so a single VM can't saturate
the host NIC. Defaults are configurable via `config.yaml`.

**File:** `engine.go` (network interface configuration), `config.go`

```go
// config.go
type RateLimitConfig struct {
    NetBandwidthBytes int64 `yaml:"net_bandwidth_bytes"` // default: 12_500_000 (100 Mbps)
    NetBurstBytes     int64 `yaml:"net_burst_bytes"`     // default: 10_000_000 (10 MB burst)
    DiskBandwidthBytes int64 `yaml:"disk_bandwidth_bytes"` // default: 104_857_600 (100 MB/s)
    DiskIOPS          int64 `yaml:"disk_iops"`            // default: 10_000
}
```

```go
netConfig := fmt.Sprintf(`{
    "iface_id": "eth0",
    "guest_mac": %q,
    "host_dev_name": %q,
    "rx_rate_limiter": {
        "bandwidth": {"size": %d, "one_time_burst": %d, "refill_time": 1000}
    },
    "tx_rate_limiter": {
        "bandwidth": {"size": %d, "one_time_burst": %d, "refill_time": 1000}
    }
}`, mac, tapName,
    e.cfg.RateLimits.NetBandwidthBytes, e.cfg.RateLimits.NetBurstBytes,
    e.cfg.RateLimits.NetBandwidthBytes, e.cfg.RateLimits.NetBurstBytes)
```

**Backward compatibility:** 🔄 ROLLING. Only new VMs get rate limiters.
Existing running VMs unaffected.

**Test:**
- `TestNetworkRateLimiterApplied` - boot VM, run iperf3, verify
  throughput capped near 100 Mbps (not unbounded)

### 3.3 Disk Rate Limiters

**Audit ref:** PROD-HARDENING Part 4

Same treatment for block devices, using configurable values from
`RateLimitConfig`:

```go
driveConfig := fmt.Sprintf(`{
    "drive_id": "rootfs",
    "path_on_host": %q,
    "is_root_device": true,
    "is_read_only": false,
    "rate_limiter": {
        "bandwidth": {"size": %d, "refill_time": 1000},
        "ops": {"size": %d, "refill_time": 1000}
    }
}`, rootfsPath,
    e.cfg.RateLimits.DiskBandwidthBytes,
    e.cfg.RateLimits.DiskIOPS)
```

**Backward compatibility:** 🔄 ROLLING. New VMs only.

### 3.4 Entropy Device

**Audit ref:** PROD-HARDENING Part 6

Add virtio-rng so guests don't block on `getrandom()`:

**File:** `engine.go` (after machine-config, before boot)

```go
if err = fcPut(ctx, client, "/entropy", `{
    "rate_limiter": {
        "bandwidth": {"size": 1024, "one_time_burst": 8192, "refill_time": 100}
    }
}`); err != nil {
    slog.Warn("entropy device setup failed", "error", err) // non-fatal
}
```

Rate limiter: 10 KB/s sustained, 8 KB initial burst. The burst allows
fast startup operations (TLS handshakes, key generation) without
blocking, while the sustained rate prevents a guest from draining the
host entropy pool.

**Backward compatibility:** 🔄 ROLLING. New VMs only. Existing VMs
continue without the device.

**Test:**
- `TestEntropyDevicePresent` - boot VM, verify `/dev/hwrng` exists
- `TestGetrandomFast` - time `dd if=/dev/urandom bs=1k count=1`,
  verify <100ms (vs seconds without virtio-rng on a fresh VM)

### 3.5 Network Override for Snapshot Resume

**Audit ref:** PROD-HARDENING Part 5

The current `ResumeSnapshot` recreates a TAP with the exact name from
the original sandbox, sets up symlinks at the old sandbox paths, loads
the snapshot, then cleans up. This is fragile: concurrent resumes from
the same origin race on the TAP name, and the `FCPathOrigin` tracking
adds complexity.

Firecracker's `network_overrides` in `/snapshot/load` remaps the
`host_dev_name` at load time, eliminating all of this:

```go
tapName, err = createTapDevice(id, userNet.BridgeName)

loadPayload := fmt.Sprintf(`{
    "snapshot_path": %q,
    "mem_backend": {"backend_path": %q, "backend_type": "File"},
    "resume_vm": true,
    "enable_diff_snapshots": true,
    "network_overrides": [{"iface_id": "eth0", "host_dev_name": %q}]
}`, vmSnapPath, memSnapPath, tapName)
```

This eliminates:
- `FCPathOrigin` tracking
- `origTapName` recreation hack
- `destroyTapDevice(origTapName)` race
- TAP name collision on concurrent resume

Drive path symlinks remain necessary until jailer adoption makes all
paths chroot-relative (Phase 6).

### 3.6 Network Override for Start() (Cold→Hot)

Same `network_overrides` in `Start()`. TAP can now be destroyed in
`Stop()`, freeing host resources while the VM is cold.

**Backward compatibility:** 🔄 ROLLING. Old snapshots work - the
override just remaps the TAP name. Symlink fallback for drives remains.

**Tests:**
- `TestResumeWithNetworkOverride` - stop sandbox, start, verify
  networking works with a fresh TAP name
- `TestConcurrentResume` - two snapshots from same base, resume
  simultaneously, verify no TAP collision

### 3.7 Sync Error Handling

**Audit ref:** PROD-HARDENING Part 7

Already covered in Phase 2.2 above. Listed here for traceability.

---

## Phase 4 - Operational Improvements

_Quality-of-life fixes that reduce operational friction and debugging
time. No security or correctness impact._

### 4.1 Server-Side Name Resolution

**Audit ref:** RELIABILITY-AUDIT #11

`GetSandbox` only matches by `id`. Every name-based request hits 404,
then the CLI falls back to listing all sandboxes. Doubles API calls
and produces confusing logs.

A single `WHERE (id = ? OR name = ?)` query has a collision risk: if a
sandbox name happens to match another sandbox's ID, the result is
ambiguous. Use a two-query approach with deterministic precedence
(ID wins):

**File:** `store.go:562`

```go
func (s *Store) GetSandbox(userID, idOrName string) (*Sandbox, error) {
    // Try exact ID match first (deterministic, always unique)
    sb, err := s.getSandboxByIDAndUser(userID, idOrName)
    if err == nil {
        return sb, nil
    }
    // Fall back to name match
    return s.getSandboxByNameAndUser(userID, idOrName)
}

func (s *Store) getSandboxByIDAndUser(userID, id string) (*Sandbox, error) {
    row := s.db.QueryRow(
        `SELECT `+sandboxCols+` FROM sandboxes WHERE id = ? AND created_by = ?`,
        id, userID)
    return scanSandbox(row)
}

func (s *Store) getSandboxByNameAndUser(userID, name string) (*Sandbox, error) {
    row := s.db.QueryRow(
        `SELECT `+sandboxCols+` FROM sandboxes WHERE name = ? AND created_by = ?`,
        name, userID)
    return scanSandbox(row)
}
```

**Backward compatibility:** ✅ TRANSPARENT. ID lookups work exactly as
before. Name lookups now also work on the first try.

**Tests:**
- `TestGetSandboxByID` - existing test, verify still works
- `TestGetSandboxByName` - create sandbox with name, look up by name
- `TestGetSandboxNameCollision` - create sandbox where another sandbox's
  name equals the first's ID → verify ID lookup wins

### 4.2 Robust Volume Detachment on Destroy Failure

**Audit ref:** RELIABILITY-AUDIT #16

If sandbox destroy fails partway, the volume attachment record in the DB
is not released. Requires manual SQL to detach before reuse.

**File:** `server.go` (in destroy handler) or `store.go`

Volume detachment must happen AFTER the VM is confirmed dead, not before.
If the VM is still running and writing to a volume, releasing the DB
lock would allow another sandbox to attach the same volume, causing ext4
corruption from two concurrent writers.

```go
// In handleSandboxDestroy:

// 1. Kill the VM first
if err := s.engine.Destroy(ctx, sb.EngineID); err != nil {
    slog.Warn("VM destroy failed, force-detaching volumes anyway",
        "sandbox", sandboxID, "error", err)
    // If Destroy failed, the FC process may already be dead (crashed)
    // or truly stuck. Either way, after Destroy attempted Kill+Wait,
    // the process is gone — safe to release volumes.
}

// 2. Now release volume attachments — VM is dead (or was already dead)
if err := s.store.DetachAllVolumes(sandboxID); err != nil {
    slog.Warn("failed to detach volumes on destroy",
        "sandbox", sandboxID, "error", err)
}

// 3. Clean up files...
```

**Backward compatibility:** ✅ TRANSPARENT.

**Test:**
- `TestDestroyReleasesVolumeOnPartialFailure` - create sandbox with
  volume, mock VM kill failure, verify volume is detached in DB and
  reattachable

### 4.3 FC Logger (New VMs Only)

**Audit ref:** PROD-HARDENING Part 8

Configure per-VM FC logging before boot:

```go
logPath := filepath.Join(sandboxDir, "firecracker.log")
if err = fcPut(ctx, client, "/logger", fmt.Sprintf(
    `{"log_path":%q,"level":"Warning","show_level":true,"show_log_origin":true}`,
    logPath)); err != nil {
    slog.Warn("FC logger setup failed", "error", err)
}
```

Use `Warning` level - not `Debug` (guest can influence log volume).
Log file lives in the sandbox directory, destroyed with the sandbox.

Note: FC logger cannot be configured on snapshot resume (FC requires
`/snapshot/load` to be the only API call on a fresh VMM). The ring
buffer stderr (Phase 1.4) covers resume diagnostics.

**Backward compatibility:** 🔄 ROLLING. New VMs only.

### 4.4 FC Metrics (New VMs Only)

**Audit ref:** PROD-HARDENING Part 8

```go
metricsPath := filepath.Join(sandboxDir, "firecracker.metrics")
if err = fcPut(ctx, client, "/metrics", fmt.Sprintf(
    `{"metrics_path":%q}`, metricsPath)); err != nil {
    slog.Warn("FC metrics setup failed", "error", err)
}
```

Metrics are NDJSON. A future phase can aggregate these into the bhatti
metrics endpoint. For now, they exist on disk for debugging.

**Backward compatibility:** 🔄 ROLLING. New VMs only.

---

## Phase 5 - Code Quality & Process Robustness

_Internal improvements that reduce the surface area for future bugs.
No external behavior changes._

### 5.1 Graceful Shutdown Before SIGKILL

**Audit ref:** PROD-HARDENING Part 9.1

Every FC termination uses `Process.Kill()` (SIGKILL). FC supports
SIGTERM for clean VMM teardown.

```go
func (e *Engine) killFC(vm *VM, timeout time.Duration) {
    vm.cmd.Process.Signal(syscall.SIGTERM)
    done := make(chan error, 1)
    go func() { done <- vm.cmd.Wait() }()
    select {
    case <-done:
        return
    case <-time.After(timeout):
        vm.cmd.Process.Kill()
        <-done
    }
}
```

3-second timeout for Stop, 1-second for Destroy.

**Caveat:** Verify that bare Firecracker (without jailer) actually
handles SIGTERM gracefully in the version we ship. If FC ignores
SIGTERM in bare mode, this just adds latency before the SIGKILL
fallback. Test against the actual FC binary before merging — check
exit code 0 (clean SIGTERM) vs 137 (SIGKILL).

**Test:**
- `TestGracefulShutdown` - stop VM, verify FC receives SIGTERM
  (check exit code 0 vs 137)

### 5.2 Socket Path Length Validation

**Audit ref:** PROD-HARDENING Part 9.2

Unix socket paths are limited to 108 bytes on Linux. A long `DataDir`
could overflow silently.

```go
func validateSocketPath(path string) error {
    if len(path) >= 108 {
        return fmt.Errorf("socket path too long (%d >= 108): %s "+
            "- use a shorter data_dir", len(path), path)
    }
    return nil
}
```

Call in `Create()` before starting FC.

**Test:**
- `TestSocketPathTooLong` - set data_dir to a 100-char path, verify
  Create returns a clear error

### 5.3 Extract `startFC` Helper

**Audit ref:** PROD-HARDENING Part 9.3

Three call sites duplicate FC launch logic (Create, Start,
ResumeSnapshot). Extract into one function:

```go
type fcProcess struct {
    cmd       *exec.Cmd
    cancel    context.CancelFunc
    stderrBuf *ringBuffer
    socket    string
}

func (e *Engine) startFC(socketPath string) (*fcProcess, error) {
    os.Remove(socketPath)
    if err := validateSocketPath(socketPath); err != nil {
        return nil, err
    }
    vmCtx, cancel := context.WithCancel(context.Background())
    stderrBuf := &ringBuffer{max: 64 * 1024}
    cmd := exec.CommandContext(vmCtx, e.cfg.FCBinary, "--api-sock", socketPath)
    cmd.Stderr = stderrBuf
    if err := cmd.Start(); err != nil {
        cancel()
        return nil, fmt.Errorf("start firecracker: %w", err)
    }
    waitForSocket(socketPath, 2*time.Second)
    return &fcProcess{cmd: cmd, cancel: cancel, stderrBuf: stderrBuf, socket: socketPath}, nil
}
```

This consolidates ring buffer creation, socket validation, and socket
wait into one place. Changes to FC launch (e.g., jailer integration in
Phase 6) happen in one function.

### 5.4 Process Reaping in All Error Paths

**Audit ref:** PROD-HARDENING Part 9.6, RELIABILITY-AUDIT #9

Ensure `Kill()` + `Wait()` always pairs in error paths. The pattern
exists in `Create()` but is missing in some paths of `Start()` and
`ResumeSnapshot()`.

```go
defer func() {
    if err != nil && fcProc != nil {
        fcProc.cmd.Process.Kill()
        fcProc.cmd.Wait()
        fcProc.cancel()
    }
}()
```

Specifically in `Start()` (engine.go:787): if `/snapshot/load` succeeds
but `WaitReady` times out after 30 seconds, the code kills FC and
cancels the context — but the VM was never added to `e.vms`, so the
`exec.CommandContext` goroutine may leak. Ensure the `defer` cleanup
covers this path.

**Tests:**
- `TestNoZombiesAfterRestoreFailure` - fail restore 5 times, verify
  no zombie FC processes (`ps aux | grep firecracker | grep -v grep`)

---

## Phase 6 - Jailer Integration

_The big one. Chroot, PID namespace, UID drop, cgroups. Breaks existing
snapshots (volumes and images are safe)._

### Why Last

The jailer is the highest-effort, highest-risk change. Every preceding
phase is independently valuable and ships without it. By the time we
reach Phase 6:
- Snapshots are correct (Phase 1-2)
- FC processes are bounded (Phase 3)
- Network resume is simplified (Phase 3.5-3.6)
- Code is clean (Phase 5)

The jailer changes one function (`startFC`) which Phase 5.3 already
extracted.

### 6.1 What Breaks

| Entity | Survives? | User action |
|--------|-----------|-------------|
| **Volumes** | ✅ Yes | None. Reattach to new sandbox. |
| **Images** | ✅ Yes | None. Use with `--image` as before. |
| **Named snapshots** | ❌ No | Delete and recreate. |
| **Thermal snapshots** | ❌ No | Sandboxes must be destroyed/recreated. |
| **Running sandboxes** | ❌ No | Destroy and recreate. |

**Key message:** Your data (volumes) is safe. Your environments (images)
are safe. Your VM checkpoints (snapshots) must be recreated.

**Cross-mode incompatibility:** Snapshots taken in bare mode (no jailer)
cannot be resumed in jailer mode and vice versa — drive paths inside
`vm.snap` are absolute in bare mode but chroot-relative in jailer mode.
The migration documented below must include destroying all existing
snapshots.

### 6.2 Phased Rollout

**Phase 6a: Chroot + PID namespace + UID drop (no network namespace)**

Gets 80% of the security benefit. Keep existing bridge networking.

```go
func (e *Engine) startFC(socketPath string) (*fcProcess, error) {
    if e.cfg.JailerBinary == "" {
        return e.startFCBare(socketPath) // dev mode
    }
    return e.startFCJailed(socketPath)
}

func (e *Engine) startFCJailed(socketPath string) (*fcProcess, error) {
    chrootBase := filepath.Join(e.cfg.DataDir, "jails")
    cmd := exec.CommandContext(vmCtx, e.cfg.JailerBinary,
        "--id", id,
        "--exec-file", e.cfg.FCBinary,
        "--uid", fmt.Sprintf("%d", e.jailUID),
        "--gid", fmt.Sprintf("%d", e.jailGID),
        "--chroot-base-dir", chrootBase,
        "--new-pid-ns",
        "--cgroup-version", "2",
        "--cgroup", fmt.Sprintf("cpu.max=%d 100000", vcpuCount*100000),
        "--cgroup", fmt.Sprintf("memory.max=%d", (memMB+128)*1024*1024),
        "--", "--api-sock", "/run/firecracker.sock",
    )
    // ...
}
```

Note: cgroup limits come for free with the jailer's `--cgroup` flags -
no need for standalone cgroup setup (PROD-HARDENING Part 2).

**Phase 6b: Per-VM network namespaces**

Full L2 isolation. Significant networking rearchitecture - separate PR.

### 6.3 File Placement in Chroot

Hard-link or copy kernel, rootfs, config drive, and volumes into the
jailer chroot at `<chroot>/root/`:

```go
func (e *Engine) setupJailFiles(id string, ...) error {
    jailRoot := filepath.Join(e.cfg.DataDir, "jails", "firecracker", id, "root")
    os.MkdirAll(jailRoot, 0700)
    // Hard-link files into chroot, chown to jail user
    // ...
}
```

### 6.4 Snapshot Paths Simplify

Inside the chroot, all paths are relative to `<chroot>/root/`. This
actually *simplifies* the current symlink dance - instead of recreating
original absolute paths, just place files at deterministic chroot-relative
paths (`/root/rootfs.ext4`, `/root/mem.snap`, etc.).

### 6.5 Dev Mode

When `JailerBinary` is empty, fall back to bare FC launch. Keeps
dev/test workflows (macOS, local testing) working.

### 6.6 Migration

Ship behind `use_jailer: true` config flag. Default OFF for one release.
Document migration:

```bash
# Save important sandbox data to volumes
bhatti exec my-sandbox -- cp -r /important /workspace/

# Delete snapshots and sandboxes
bhatti snapshot list | xargs bhatti snapshot delete
bhatti list --json | jq -r '.[].name' | xargs bhatti destroy

# Enable jailer
# config.yaml: use_jailer: true
sudo systemctl restart bhatti

# Recreate - volumes reattach, images work
bhatti create --name my-sandbox --image my-env --volume workspace:/workspace
```

**Backward compatibility:** ⚠️ BREAKING. Snapshots and sandboxes must
be recreated. Volumes and images are safe.

**Tests:**
- `TestJailerBootAndExec` - boot with jailer, exec command, verify output
- `TestJailerChroot` - verify FC process can't see host filesystem
  (`/proc/<fc-pid>/root` is the chroot)
- `TestJailerUIDDrop` - verify FC process runs as `bhatti-vm` user
- `TestJailerCgroupLimits` - verify memory.max and cpu.max are set
- `TestJailerSnapshotRoundtrip` - create, snapshot, destroy, resume
- `TestJailerDevModeFallback` - empty JailerBinary, verify bare FC launch

---

## Phase 7 - Balloon + Hugepages

_Advanced memory management. Enables memory overcommit and faster boot.
Required for scaling beyond a handful of concurrent VMs per host._

### 7.1 Balloon Device

Virtio balloon lets the host reclaim guest memory dynamically. The
thermal manager can inflate on warm VMs (reclaim memory without full
snapshot):

```go
// On create:
fcPut(ctx, client, "/balloon",
    `{"amount_mib": 0, "deflate_on_oom": true, "stats_polling_interval_s": 5}`)

// In thermal cycle, for warm VMs:
fcPatch(ctx, client, "/balloon",
    fmt.Sprintf(`{"amount_mib": %d}`, vm.MemSizeMib/2))
```

### 7.2 Hugepages

2MB hugepages for up to 50% boot time improvement. Mutually exclusive
with dirty page tracking (no Diff snapshots). Best for ephemeral VMs.

```go
machineConfig := fmt.Sprintf(
    `{"vcpu_count":%d,"mem_size_mib":%d,"track_dirty_pages":%v,"huge_pages":%q}`,
    vcpuCount, memMB, !spec.UseHugepages,
    map[bool]string{true: "2M", false: "None"}[spec.UseHugepages])
```

Host prerequisite: `echo 1024 > /proc/sys/vm/nr_hugepages`

**Backward compatibility:** 🔄 ROLLING, new VMs only.

---

## Phase 8 - Layered Snapshots & Storage Efficiency

_Sandboxes resumed from a snapshot should build on top of the base, not
copy it. This is the #1 storage problem for scaling beyond a single
host._

### 8.0 The Problem: Sandboxes Copy Everything

Every `bhatti create` and `bhatti snapshot resume` does a full copy of
all block devices. There is no sharing between the source and the clone.

**Measured on agni-01** (2× Samsung PM983 NVMe, md RAID-1, ext4):

| Operation | Time | Disk cost |
|-----------|------|-----------|
| `create --image browser` (2 GB) | 1.6s | +2 GB |
| `create --image spc-agents-hermes` (10 GB) | 3.0s | +3.6 GB |
| `snapshot create` (rootfs + mem + config + vol) | 3-8s (VM paused!) | +4-12 GB |
| `snapshot resume` (copy everything out) | 3-8s | +4-12 GB |

The `copyRootfs()` function tries `cp --reflink=always` first, which
fails immediately on ext4 (`EOPNOTSUPP`), then falls back to
`cp --sparse=always`. Sparse preserves zero-filled regions but still
reads and writes every non-zero block. No block-level sharing.

**The storage multiplier is brutal.** Resume one snapshot 10 times:
10 full copies. The `browser-ready` snapshot is 3 GB. 10 resumes = 30 GB.
The `rory-ready-v2` snapshot is 7.9 GB. 10 resumes = 79 GB.

**But almost nothing actually changes.** Byte-level comparison of 9
sandboxes all created from the `browser-ready` snapshot against the
original rootfs:

```
Sandbox                  Bytes that differ   % of 2 GB rootfs
551ca4aa229b6db4              69,635          0.003%
72bb7fe4898040b6             814,550          0.038%
2c27a16ecfa539a3             816,610          0.038%
65e788e222a793dc             830,191          0.039%
d2380f84508c7060             830,038          0.039%
1a8bf5f652c44980             846,434          0.039%
506711ef78f2d5a5             871,183          0.041%
0a6ccf831e2f2be0           1,026,990          0.048%
990bac589bd8e309           1,518,160          0.071%
                     avg:    847,088    =     0.8 MB
```

**9 sandboxes use 18 GB of disk for 7.2 MB of actual unique data.**
The other 99.96% is byte-for-byte identical to the snapshot rootfs.
With copy-on-write, those 9 sandboxes would use 2 GB (shared base) +
7.2 MB (diffs) = **2.007 GB** instead of 18 GB. That's an **89%
reduction.**

This is the problem your user called "layered snapshots" — sandboxes
should build on top of the base snapshot, not copy it.

### 8.1 Layered Snapshots via Copy-on-Write

The layered snapshot concept maps directly to filesystem-level
copy-on-write. When a sandbox is resumed from a snapshot, the rootfs
clone *shares all blocks* with the original. Only blocks the guest
actually writes to allocate new space. The guest and Firecracker see
a normal read-write file — the sharing is invisible to them.

#### How it works

```
Snapshot:     browser-ready/rootfs.ext4  (2 GB, the base)
                  │
              reflink clone (instant, 0 bytes copied)
                  │
                  ├─── sandbox-aaa/rootfs.ext4  (shares 99.96% of blocks)
                  ├─── sandbox-bbb/rootfs.ext4  (shares 99.96% of blocks)
                  ├─── sandbox-ccc/rootfs.ext4  (shares 99.96% of blocks)
                  └─── ...100 more...           (still 2 GB total on disk,
                                               not 200 GB)

When sandbox-aaa writes to block N:
  1. CoW allocates a new block
  2. Copies original block N into it
  3. Applies the write to the new block
  4. sandbox-aaa now has its own copy of block N
  5. All other sandboxes still read the original block N
```

Firecracker doesn't know this is happening. It opens the file path,
gets a file descriptor, reads and writes normally. The filesystem
handles the sharing transparently.

#### 8.1.1 btrfs (recommended, simple)

btrfs supports instant copy-on-write clones via `cp --reflink=always`.
This is the same mechanism Docker's btrfs storage driver uses.

**Verified on agni-01** (btrfs on loopback over ext4 RAID-1):

| Operation | ext4 (current) | btrfs reflink |
|-----------|----------------|---------------|
| Clone 2 GB rootfs | 1.6s, +2 GB disk | **0.009s**, +0 bytes |
| Clone 10 GB rootfs | 3.0s, +3.6 GB disk | **0.016s**, +0 bytes |
| 3 copies of 2 GB | 4.8s, +6 GB | 0.027s, **1.90 GiB total** (sharing works) |

**Code change required: nearly zero.** `copyRootfs()` already tries
reflink first. The snapshot paths in `snapshot.go` need one flag
addition:

```go
// snapshot.go:158 — Checkpoint parallel copy
// Before:
return exec.Command("cp", "--sparse=always", c.src, dst).Run()
// After:
return exec.Command("cp", "--reflink=auto", "--sparse=always", c.src, dst).Run()

// snapshot.go:260 — ResumeSnapshot drive copy
// Before:
exec.Command("cp", "--sparse=always", src, dst).Run()
// After:
exec.Command("cp", "--reflink=auto", "--sparse=always", src, dst).Run()
```

`--reflink=auto` tries reflink, falls back to regular copy silently.
Combined with `--sparse=always`, this is optimal on any filesystem.
On ext4 it behaves exactly as before. On btrfs/XFS it's instant.

**What about mem.snap?** FC writes mem.snap fresh during snapshot
creation and reads it during load. On resume, each sandbox gets its
own clone of mem.snap. With reflink, the initial clone is instant.
When the sandbox is later stopped and a new thermal snapshot is taken,
FC rewrites mem.snap — at which point the CoW diverges and the full
memory is allocated. But the initial resume is free.

**Filesystem setup for self-hosters:** Put `/var/lib/bhatti` on btrfs.
The simplest path (no partition changes, fully reversible):

```bash
# Create btrfs image (pre-allocated to avoid ENOSPC)
fallocate -l 500G /var/lib/bhatti-btrfs.img   # or whatever size
mkfs.btrfs -f /var/lib/bhatti-btrfs.img

# Stop bhatti, rsync data, mount, restart
systemctl stop bhatti
mkdir -p /mnt/bhatti-new
mount -o loop,noatime,compress=zstd:1 /var/lib/bhatti-btrfs.img /mnt/bhatti-new
rsync -aHAX --sparse /var/lib/bhatti/ /mnt/bhatti-new/
umount /mnt/bhatti-new
mv /var/lib/bhatti /var/lib/bhatti-ext4-backup
mkdir -p /var/lib/bhatti
mount -o loop,noatime,compress=zstd:1 /var/lib/bhatti-btrfs.img /var/lib/bhatti
echo '/var/lib/bhatti-btrfs.img /var/lib/bhatti btrfs loop,noatime,compress=zstd:1 0 0' >> /etc/fstab
systemctl start bhatti
```

**Mount options:** `noatime` (no access time updates), `compress=zstd:1`
(transparent compression, ~40% reduction on ext4 images, ~300 MB/s —
faster than NVMe write), `autodefrag` (background defrag for random
writes from Firecracker).

**Rollback:** unmount btrfs, move ext4 backup back, restart. Minutes.

**Why btrfs over XFS?** Both support reflink. XFS has a refcount btree
limit — hundreds of clones of the same file can hit `ENOSPC` even with
free space. btrfs scales to arbitrary clone counts. XFS is a fine
alternative if btrfs stability is a concern — the code is identical
(`--reflink=auto` works on both).

**Why not ZFS?** Not in mainline kernel (DKMS, licensing), heavier
memory footprint (ARC competes with Firecracker guest memory),
Hetzner rescue doesn't include ZFS tools.

#### 8.1.2 dm-thin (deferred)

Device-mapper thin provisioning provides the same copy-on-write
semantics at the block layer for users who must stay on ext4. This is
what Docker's `devicemapper` storage driver used. However, the
operational complexity (pool management, monitoring, cleanup lifecycle)
is significantly higher than btrfs loopback, which works on any Linux
system. Deferred — to be tackled separately if there's real demand
from self-hosters who can't use btrfs.

### 8.2 Impact on Snapshot Pause Duration

Checkpoint copies block devices while the VM is paused. With reflink,
the copy becomes instant:

| Scenario | ext4 (current) | btrfs reflink |
|----------|----------------|---------------|
| 2 GB rootfs + 1 GB mem + 1 MB config | ~2.5s paused | **~0.05s paused** |
| 10 GB rootfs + 4 GB mem + 5 GB vol | ~8s paused | **~0.1s paused** |

mem.snap is the one file that can't be reflinked during Checkpoint —
it's freshly written by Firecracker into the snapshot directory, so
there's no source to clone from. But mem.snap is written directly
(no copy step), so the only pause cost is FC's own snapshot write
time, not our copy time.

This changes the calculus on named snapshots: they become nearly
free instead of a multi-second VM pause. Users can checkpoint
aggressively without impacting running workloads.

### 8.3 Disk Usage Impact

**Current** (ext4, agni-01):
```
images:      15 GB
snapshots:   11 GB
sandboxes:   42 GB  ← 9 sandbox rootfs copies = 18 GB of duplicated data
volumes:    122 MB
TOTAL:       67 GB
```

**Projected** (btrfs with reflink + zstd:1 compression):
```
images:      10 GB  (compression)
snapshots:    7 GB  (compression)
sandboxes:    5 GB  (reflink sharing + compression, instead of 42 GB)
volumes:     80 MB  (compression)
TOTAL:      ~22 GB  (67% reduction)
```

And it scales sub-linearly. 100 sandboxes from the same image:
- ext4: 100 × 2 GB = **200 GB**
- btrfs: 2 GB base + 100 × ~1 MB actual writes = **~2.1 GB**

### 8.4 S3-Compatible Volume Backup (Native)

Users need a way to back up volume data to their own S3-compatible
storage without setting up external tooling. This should be a
first-class bhatti feature, not an ops runbook.

#### Design

```
bhatti volume backup <volume-name> --s3-bucket my-bucket --s3-endpoint s3.amazonaws.com
bhatti volume restore <volume-name> --from s3://my-bucket/bhatti/volumes/workspace/2026-04-01T03:00:00Z.tar.zst
bhatti volume backup list <volume-name>
```

Server-side config for S3 credentials (per-user or global):

```yaml
# config.yaml
backup:
  s3_endpoint: "s3.eu-central-1.amazonaws.com"
  s3_bucket: "my-bhatti-backups"
  s3_access_key: "..."
  s3_secret_key: "..."
  # Or per-user: users can configure via `bhatti config set backup.s3_bucket ...`
```

#### What to back up

| Data | Backup via | Why |
|------|-----------|-----|
| **Volumes** | `bhatti volume backup` | Irreplaceable user data |
| **User images** | `bhatti image export` → user's own storage | Expensive to recreate |
| **Named snapshots** | Future: `bhatti snapshot archive` | Large, cold, resumable |

The backup command:
1. Pauses the sandbox (if attached and hot) or operates on the volume
   file directly (if detached)
2. Creates a zstd-compressed tar of the volume ext4 file
3. Uploads to S3 with a timestamped key
4. Resumes the sandbox

Restore creates a new volume from the S3 artifact, or replaces the
contents of an existing detached volume.

#### Scheduled backups

```yaml
backup:
  schedule:
    - volume: "workspace"
      cron: "0 3 * * *"   # daily at 3 AM
      retention: 7         # keep 7 most recent
```

The daemon runs a background goroutine that checks the schedule and
triggers backups. This keeps the feature self-contained — no external
cron, restic, or systemd timer required.

### 8.5 Implementation Order

```
8.1 btrfs + reflink code change     🛑 DOWNTIME ~15 min    ← DO FIRST (biggest impact)
  ├── --reflink=auto in snapshot.go (2 lines)
  ├── btrfs loopback setup on agni-01
  ├── Document btrfs setup for self-hosters
  └── Verify: create 10 sandboxes, measure disk usage
       ↓
8.4 S3-compatible volume backup      🔄 ROLLING
  ├── S3 upload/download in engine
  ├── CLI commands: volume backup / restore / backup list
  └── Scheduled backup goroutine
```

---


## Implementation Order

```
Phase 1 (Stop the Bleeding)           ✅ TRANSPARENT      ← DO FIRST
  1.1 SnapshotAll Full snapshots + retry on failure
  1.2 Thermal failure logging + force-pause
  1.3 Restore circuit breaker + --force reset
  1.4 FC stderr ring buffer
         ↓
Phase 2 (Snapshot Integrity)           ✅ TRANSPARENT
  2.1 Snapshot sanity checks (size + JSON validation)
  2.2 Sync error handling
  2.3 Reset has_base_snapshot on recovery
         ↓
Phase 3 (FC Hardening + Network)       🔄 ROLLING
  3.1 Disable serial console
  3.2 Network rate limiters (configurable)
  3.3 Disk rate limiters (configurable)
  3.4 Entropy device
  3.5 network_overrides for snapshot resume
  3.6 network_overrides for Start() cold→hot
         ↓ (phases 4-5 are independent, can be parallelized)
Phase 4 (Operational Improvements)     ✅ TRANSPARENT
  4.1 Server-side name resolution
  4.2 Volume detachment on destroy failure
  4.3 FC logger
  4.4 FC metrics
         ↓
Phase 5 (Code Quality)                ✅ TRANSPARENT
  5.1 Graceful shutdown (SIGTERM before SIGKILL)
  5.2 Socket path validation
  5.3 Extract startFC helper
  5.4 Process reaping in all error paths
         ↓
Phase 6 (Jailer)                       ⚠️ BREAKING
  6a  Chroot + PID ns + UID drop + cgroups
  6b  Per-VM network namespaces (separate PR)
         ↓
Phase 7 (Balloon + Hugepages)          🔄 ROLLING
Phase 8 (Layered Snapshots + Backup)   🛑 DOWNTIME
  8.1   btrfs + reflink (~15 min downtime)
  8.4   S3-compatible volume backup (native feature)
```

Phases 1-3 address every issue from the rory incident and the most
impactful Firecracker hardening gaps. Phase 8 solves the storage
multiplication problem and adds native backup. Phase 6 is the big
security investment.

### Dependency Graph

```
Phase 1 ──→ Phase 2 ──→ Phase 3 ──┬──→ Phase 4
                                   └──→ Phase 5 ──→ Phase 6
Phase 7 is independent (anytime after Phase 3)
Phase 8 is independent (can start immediately, parallel to all others)
```

Phase 1 is the only hard prerequisite for the correctness phases.
Phase 8 can start in parallel - the btrfs migration and backup feature
have no code dependencies on Phases 1-7. Phase 5 should land before
Phase 6 since the jailer modifies `startFC` (5.3).

---

## Traceability Matrix

Every item from both source documents mapped to its phase:

| Audit Ref | Issue | Phase |
|-----------|-------|-------|
| RA #1 | Silent activity query timeout | 1.2 |
| RA #2 | Sessions block pause silently | 1.2 |
| RA #3 | No metric for skipped sandboxes | 1.2 |
| RA #4 | Diff snapshot corruption | 1.1, 2.2 |
| RA #5 | No snapshot verification | 2.1 |
| RA #6 | SnapshotAll uses Diff for hot VMs | 1.1 |
| RA #7 | has_base_snapshot stale after restart | 2.3 |
| RA #8 | FC stderr not captured per-VM | 1.4 |
| RA #9 | Zombie FC on restore failure | 1.3, 5.4 |
| RA #10 | No circuit breaker on restore | 1.3 |
| RA #11 | Name resolution requires 2 calls | 4.1 |
| RA #12 | Recovered VMs use stale has_base_snapshot | 2.3 |
| RA #13 | No health check on recovered snapshots | 2.1 |
| RA #14 | Agent unresponsive blocks thermal | 1.2 |
| RA #15 | No agent liveness vs activity distinction | 1.2 (force-pause) |
| RA #16 | Volume attachment leak on partial destroy | 4.2 |
| RA #17 | No fsck after unclean VM death | Future |
| PH Part 1 | Jailer integration | 6 |
| PH Part 2 | cgroup resource limits | 6 (via jailer) |
| PH Part 3 | Serial console + stderr | 1.4, 3.1 |
| PH Part 4 | Network + disk rate limiters | 3.2, 3.3 |
| PH Part 5 | network_overrides | 3.5, 3.6 |
| PH Part 6 | Entropy device | 3.4 |
| PH Part 7 | Sync error check | 2.2 |
| PH Part 8 | FC logger + metrics | 4.3, 4.4 |
| PH Part 9 | Code quality (6 items) | 5 |
| PH Part 10 | Balloon + hugepages | 7 |

---

## What's Explicitly Not in This Plan

- **Per-VM network namespaces** - Phase 6b, separate PR after jailer lands
- **fsck on volume after unclean death** - low risk (journal replay handles
  it), adds complexity to the attach path
- **Agent liveness vs activity separation** - Phase 1.2's force-pause
  after N failures handles the practical case without a new agent endpoint
- **Balloon-aware thermal management** - requires Phase 7 balloon device,
  separate design for thermal integration
- **dm-thin storage driver** - deferred unless btrfs proves insufficient
  for self-hosters (see 8.1.2)
- **Snapshot offload to S3** (`bhatti snapshot archive`) - future
  extension of 8.4's S3 integration
- **Cross-region backup replication** - deferred until primary S3 backup
  is proven in production
