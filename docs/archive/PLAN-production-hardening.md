# Bhatti Production Hardening — Firecracker Best Practices

This plan addresses every gap between how bhatti uses Firecracker today and
what the Firecracker documentation requires for production deployments. The
audit covers `prod-host-setup.md`, `jailer.md`, `snapshotting/snapshot-support.md`,
`snapshotting/network-for-clones.md`, `ballooning.md`, `entropy.md`,
`hugepages.md`, `network-setup.md`, `network-performance.md`, `seccomp.md`,
`logger.md`, `metrics.md`, and `design.md`.

Sources: all files under https://github.com/firecracker-microvm/firecracker/tree/main/docs
as of 2026-04-01.

---

## Compatibility & Downtime Summary

Every change in this plan is tagged with its impact:

| Tag | Meaning |
|-----|---------|
| ⚠️ **BREAKING** | Existing snapshots/VMs won't survive this change. Requires `bhatti destroy` + recreate. |
| 🔄 **ROLLING** | Can be deployed without downtime. Old VMs keep working. New VMs get the improvement. |
| 🛑 **DOWNTIME** | Requires stopping the bhatti daemon. Running VMs are snapshotted and resumed after. |
| ✅ **TRANSPARENT** | No user-visible change. Pure internal improvement. |

### Volumes, Images, and Snapshots — What Breaks and What Doesn't

These three things are different and have completely different lifecycles.
The user sees "my data" but under the hood they are different objects with
different durability guarantees. This section must be crystal clear because
users interacting with bhatti through the API (not through us) need to
understand what happens to their data.

#### Volumes — ✅ SAFE across every change in this plan

A volume is an ext4 file on the host at `/var/lib/bhatti/volumes/<user>/<name>.ext4`.
It is the user's persistent data (workspace files, databases, etc.).

Volumes exist **outside** the sandbox directory. They are never copied into
it. When a sandbox is created with `--volume workspace:/workspace`, the
server resolves the volume name to its host path and passes that path to
Firecracker as `path_on_host` in the drive configuration:

```go
// engine.go:480 — FC is told the REAL volume path, not a copy
fcPut(ctx, client, "/drives/vol0",
    `{"path_on_host":"/var/lib/bhatti/volumes/usr_xxx/workspace.ext4",...}`)
```

Firecracker opens the file, the guest mounts it, reads/writes happen
directly to the ext4 file. When the sandbox is destroyed, the volume file
is untouched — it stays at `/var/lib/bhatti/volumes/<user>/workspace.ext4`.

**Why volumes survive the jailer change**: The jailer changes how the FC
process is launched and what filesystem paths it can see. But the volume
file itself is just an ext4 file on the host. Destroy the old sandbox,
enable the jailer, create a new sandbox with `--volume workspace:/workspace`
→ same file, same data, new sandbox. The volume was never coupled to the
sandbox's FC process.

**The one nuance**: While a volume is *attached* to a running sandbox, the
ext4 file has an active mount inside the guest. You cannot safely attach it
to a second sandbox simultaneously (ext4 doesn't support concurrent mounts).
Destroying the sandbox or stopping it cleanly unmounts the volume. After
that, it can be reattached to any new sandbox.

#### Images — ✅ SAFE across every change in this plan

An image is an immutable ext4 file at `/var/lib/bhatti/images/<name>.ext4`.
It is used as a *source* for sandbox rootfs. On `bhatti create --image python-3.12`,
the engine copies (CoW clone or sparse copy) the image file into the sandbox
directory as `rootfs.ext4`. The original image is never modified.

Images have no relationship to Firecracker's runtime state, snapshots, or
the jailer. They are pure source material.

#### Snapshots — ⚠️ BROKEN by jailer integration (Part 1 only)

A snapshot is a frozen-in-time capture of a VM's **entire state**: CPU
registers, RAM contents, and device state. It consists of:

- `vm.snap` — serialized Firecracker device state (CPU, virtio rings, etc.)
- `mem.snap` — guest RAM contents
- Copies of all block devices (rootfs, config drive, volume data *at
  snapshot time*)

The critical detail: **`vm.snap` records the absolute host paths** that
Firecracker was configured with at boot time. When you load a snapshot,
Firecracker opens those exact paths to reconnect its virtio-blk devices.

Today those paths look like:
```
/var/lib/bhatti/sandboxes/a1b2c3d4e5f6/rootfs.ext4
/var/lib/bhatti/sandboxes/a1b2c3d4e5f6/config.ext4
/var/lib/bhatti/volumes/usr_xxx/workspace.ext4
```

The symlink dance in `ResumeSnapshot` exists precisely to place files at
these recorded paths so FC can open them.

When the jailer is enabled, FC runs inside a chroot. All paths become
relative to `<chroot>/root/` (e.g., `/root/rootfs.ext4`). A snapshot
taken without the jailer has absolute paths; a jailed FC can't see them.
A snapshot taken with the jailer has chroot-relative paths; an un-jailed
FC would look in the wrong place.

**What this means for users:**

| Entity | Survives jailer migration? | User action needed |
|--------|--------------------------|--------------------|
| **Volumes** | ✅ Yes — data files are independent | None. Reattach to new sandbox. |
| **Images** | ✅ Yes — source files, never modified | None. Use with `--image` as before. |
| **Named snapshots** (`bhatti snapshot create`) | ❌ No — `vm.snap` has wrong paths | Delete and recreate from a new sandbox. |
| **Thermal snapshots** (auto hot→cold) | ❌ No — same `vm.snap` path issue | Sandboxes must be destroyed and recreated. |
| **Running sandboxes** | ❌ No — can't live-migrate to jailer | Destroy and recreate. Volume data is safe. |

**The key message to users**: *Your data (volumes) is safe. Your
environments (images) are safe. Your VM checkpoints (snapshots) must be
recreated. Any work that lives in a volume survives. Any work that lives
only in a sandbox rootfs should be saved to a volume or image first.*

#### How Checkpoint snapshots handle volume data

When `bhatti snapshot create` runs, Checkpoint copies the volume ext4 file
into the snapshot directory alongside `vm.snap` and `mem.snap`. This is a
point-in-time copy — the snapshot is self-contained:

```go
// snapshot.go:143-149 — volume data is COPIED into the snapshot dir
for _, vol := range vm.Volumes {
    snapFile := fmt.Sprintf("vol-%s.ext4", vol.Name)
    copies = append(copies, copyJob{vol.FilePath, snapFile})
}
```

When `bhatti snapshot resume` runs, it copies those files back out and
symlinks them at the original paths so FC can find them. **The original
volume file is not modified.** The resumed sandbox uses a *copy* of the
volume data from snapshot time, not the live volume.

This means: if you take a snapshot, then write more data to the volume
via another sandbox, then resume the snapshot — the resumed sandbox sees
the volume data from *snapshot time*, not the latest writes. The snapshot
is hermetic.

### What else breaks (nothing)

- **Serial console disable (Part 3)** changes boot args. Existing cold
  snapshots have old boot args baked in, but this doesn't matter — boot
  args aren't used on snapshot resume (they're part of the vm.snap). New
  VMs get the hardened boot args. **No break.**

- **`network_overrides` (Part 5)** changes snapshot resume logic. Old
  snapshots work fine — the override just maps the TAP name. **No break.**

- **Everything else** (rate limiters, entropy, cgroups, logger, code quality)
  only affects newly created VMs or is purely internal. **No break.**

### Recommended deployment order

```
Part 3 (serial/stderr)     ✅ TRANSPARENT   — 30 min, zero risk
Part 4 (rate limiters)     🔄 ROLLING       — 30 min, new VMs only
Part 6 (entropy device)    🔄 ROLLING       — 15 min, new VMs only
Part 7 (sync error check)  ✅ TRANSPARENT   — 15 min, zero risk
Part 8 (FC logger/metrics) 🔄 ROLLING       — 1 hr, new VMs only
Part 9 (code quality)      ✅ TRANSPARENT   — 2 hr, internal cleanup
Part 5 (network_overrides) 🔄 ROLLING       — 2 hr, simplifies snapshot resume
Part 2 (cgroups)           🛑 DOWNTIME      — 2 hr, daemon restart required
Part 1 (jailer)            ⚠️ BREAKING      — 4-6 hr, full rebuild of all VMs
Part 10 (balloon/hugepages)🔄 ROLLING       — 2 hr, optional, new VMs only
```

### Migration checklist for the jailer (Part 1)

This is the script users run before enabling the jailer:

```bash
# 1. Save any important sandbox state to volumes/images
bhatti exec my-sandbox -- cp -r /important-rootfs-data /workspace/  # move to volume
bhatti image save my-sandbox --name my-env-backup                  # save rootfs as image

# 2. Delete all named snapshots (they won't work post-jailer)
bhatti snapshot list
bhatti snapshot delete my-checkpoint                               # repeat for each

# 3. Destroy all sandboxes (volumes are NOT deleted)
bhatti destroy my-sandbox

# 4. Enable jailer
# Edit /var/lib/bhatti/config.yaml:
#   use_jailer: true
sudo systemctl restart bhatti

# 5. Recreate sandboxes — volumes reattach, images work as before
bhatti create --name my-sandbox --image my-env-backup --volume workspace:/workspace
```

---

## Part 1 — Jailer Integration

**Impact: ⚠️ BREAKING — snapshots and sandboxes must be recreated (volumes and images are safe — see above)**

### The Problem

Firecracker is launched as a bare process:

```go
// engine.go:419
fcCmd = exec.CommandContext(vmCtx, e.cfg.FCBinary, "--api-sock", socketPath)
fcCmd.Stderr = os.Stderr
```

No chroot, no namespace, no privilege drop, no cgroup. The `prod-host-setup.md`
doc is unambiguous: *"For assuring secure isolation in production deployments,
Firecracker should be started using the `jailer` binary."*

A guest kernel exploit that escapes the VM lands in a root-privileged,
un-namespaced, un-chrooted FC process with full host filesystem access.

### What the Jailer Provides

The jailer binary wraps Firecracker and applies:

| Layer | What it does | Our current state |
|-------|-------------|-------------------|
| **chroot** | FC sees only its own sandbox dir | ❌ FC sees entire host FS |
| **pid namespace** | Guest can't enumerate host processes | ❌ shares host pid namespace |
| **cgroup** | CPU/memory/IO limits per VM | ❌ unlimited |
| **uid/gid drop** | FC runs as unprivileged user | ❌ runs as root |
| **network namespace** | Proper L2 isolation | ⚠️ approximated with bridges |

### Why This Breaks Existing Snapshots

See "Volumes, Images, and Snapshots" section above for the full explanation.
In short: `vm.snap` records absolute host paths. The jailer chroot makes
those paths invisible. Snapshots must be recreated. Volumes and images are
unaffected.

### Design

#### 1.1 Jailer Binary Management

The installer already downloads the FC release tarball which includes both
`firecracker` and `jailer` binaries. Add jailer to the install:

```bash
# scripts/install.sh — in install_firecracker()
mv "$tmpdir/release-v${FC_VERSION}-${FC_ARCH}/jailer-v${FC_VERSION}-${FC_ARCH}" \
    /usr/local/bin/jailer
```

Config gets a new field:
```go
type Config struct {
    // ... existing fields ...
    JailerBinary string // path to jailer binary (empty = skip jailer, for dev)
}
```

#### 1.2 Unprivileged User Setup

The jailer needs a UID/GID to drop to. Each VM ideally gets its own UID
(as FC docs recommend), but that requires pre-allocating UIDs. Start with
a single `bhatti-vm` user:

```bash
# scripts/install.sh — in do_server_install()
useradd --system --no-create-home --shell /usr/sbin/nologin bhatti-vm
```

For Phase 2 (per-VM UIDs), allocate a range in `/etc/subuid` and map each
VM to `bhatti-vm-base-uid + vm-index`.

#### 1.3 Launch via Jailer

Replace the bare `exec.Command` with jailer invocation:

```go
func (e *Engine) startFC(ctx context.Context, id, socketPath string) (*exec.Cmd, context.CancelFunc, error) {
    vmCtx, cancel := context.WithCancel(context.Background())

    if e.cfg.JailerBinary == "" {
        // Dev mode: bare firecracker (no jailer)
        cmd := exec.CommandContext(vmCtx, e.cfg.FCBinary, "--api-sock", socketPath)
        cmd.Stderr = &ringBuffer{max: 64 * 1024} // Part 3
        return cmd, cancel, cmd.Start()
    }

    chrootBase := filepath.Join(e.cfg.DataDir, "jails")
    cmd := exec.CommandContext(vmCtx, e.cfg.JailerBinary,
        "--id", id,
        "--exec-file", e.cfg.FCBinary,
        "--uid", fmt.Sprintf("%d", e.jailUID),
        "--gid", fmt.Sprintf("%d", e.jailGID),
        "--chroot-base-dir", chrootBase,
        "--netns", fmt.Sprintf("/var/run/netns/bhatti-%s", id),
        "--new-pid-ns",
        "--cgroup-version", "2",
        "--cgroup", fmt.Sprintf("cpu.max=%d 100000", vcpuCount*100000),
        "--cgroup", fmt.Sprintf("memory.max=%d", memMB*1024*1024+128*1024*1024),
        "--resource-limit", "fsize=4294967296", // 4GB max file size
        "--resource-limit", fmt.Sprintf("no-file=%d", 4096),
        "--daemonize",
        "--", "--api-sock", "/run/firecracker.sock",
    )
    cmd.Stderr = &ringBuffer{max: 64 * 1024}
    return cmd, cancel, cmd.Start()
}
```

#### 1.4 File Placement Inside Chroot

The jailer creates the chroot, but files (kernel, rootfs, drives) must be
hard-linked or bind-mounted into it. The jailer expects all resources under
`<chroot>/root/`:

```go
func (e *Engine) setupJailFiles(id string, rootfsPath, configPath string, volumes []VolumeAttachmentInfo) error {
    jailRoot := filepath.Join(e.cfg.DataDir, "jails", "firecracker", id, "root")
    os.MkdirAll(jailRoot, 0700)

    // Hard-link (same filesystem) or copy files into chroot
    links := map[string]string{
        e.cfg.KernelPath: filepath.Join(jailRoot, "vmlinux"),
        rootfsPath:       filepath.Join(jailRoot, "rootfs.ext4"),
        configPath:       filepath.Join(jailRoot, "config.ext4"),
    }
    for _, vol := range volumes {
        links[vol.FilePath] = filepath.Join(jailRoot, filepath.Base(vol.FilePath))
    }

    for src, dst := range links {
        os.Remove(dst)
        if err := os.Link(src, dst); err != nil {
            // Cross-device: fall back to bind mount or copy
            if err := copyRootfs(src, dst); err != nil {
                return fmt.Errorf("jail file %s: %w", src, err)
            }
        }
    }

    // Chown to jail user
    return filepath.Walk(jailRoot, func(path string, info os.FileInfo, err error) error {
        if err != nil { return err }
        return os.Chown(path, e.jailUID, e.jailGID)
    })
}
```

#### 1.5 Network Namespace per VM

The jailer's `--netns` flag requires a pre-existing network namespace.
This replaces the current bridge-based approach with proper L2 isolation:

```go
func (e *Engine) setupNetNS(id, tapName, guestIP, gateway string) error {
    nsName := "bhatti-" + id
    run("ip", "netns", "add", nsName)
    // Move TAP into the namespace
    run("ip", "link", "set", tapName, "netns", nsName)
    run("ip", "netns", "exec", nsName, "ip", "link", "set", tapName, "up")
    // Connectivity via veth pair (see Part 5 of network-for-clones.md)
    // ...
    return nil
}
```

However, per-VM network namespaces are a significant rearchitecture of
the networking layer. **Phase the jailer integration:**

- **Phase 1**: Jailer with `--new-pid-ns` + cgroups + chroot + uid drop.
  Keep the existing bridge networking (no `--netns`). This gets 80% of
  the security benefit.
- **Phase 2**: Add per-VM network namespaces. This is the full isolation
  story but requires reworking TAP creation, bridge setup, and iptables.

#### 1.6 Snapshot Paths in Jailed Environment

Inside the chroot, all paths are relative to `<chroot>/root/`. The
`vm.snap` records these relative paths. On snapshot resume, the new
jailer chroot must have files at the same relative locations.

This actually *simplifies* the current symlink dance in `ResumeSnapshot` —
instead of recreating original absolute paths, just place files at the
same chroot-relative paths (`/root/rootfs.ext4`, `/root/mem.snap`, etc.).

#### 1.7 Dev Mode

When `JailerBinary` is empty, fall back to the current bare-FC launch.
This keeps dev/test workflows (macOS cross-compile, local testing without
jailer) working.

### Migration Plan

1. Ship the jailer code path behind a config flag (`use_jailer: true`)
2. Default OFF for one release — existing deployments unaffected
3. Document migration: `bhatti destroy --all && bhatti update && set use_jailer: true`
4. Default ON in the next release
5. Eventually remove bare-FC path (or keep for dev only)

---

## Part 2 — cgroup Resource Limits

**Impact: 🛑 DOWNTIME — daemon restart required, but VMs survive via snapshot/resume**

### The Problem

No cgroup constraints on FC processes. A single VM can:
- Exhaust host memory (FC VMM overhead + page tables are unconstrained)
- Monopolize CPU (no shares or quota)
- Saturate disk I/O (no blkio throttle)

The `prod-host-setup.md` doc dedicates entire sections to cgroup controls
for Disk, Memory, and vCPU.

### Design

If the jailer is enabled (Part 1), cgroups come for free via `--cgroup` flags.
If the jailer is not yet deployed, apply cgroups manually after FC process start.

#### 2.1 Standalone cgroup Setup (Pre-Jailer)

Apply cgroup limits after the FC process starts:

```go
func (e *Engine) applyCgroups(pid int, id string, vcpuCount, memMB int64) error {
    cgroupPath := fmt.Sprintf("/sys/fs/cgroup/bhatti/%s", id)
    os.MkdirAll(cgroupPath, 0755)

    // Move FC process into its cgroup
    os.WriteFile(filepath.Join(cgroupPath, "cgroup.procs"),
        []byte(fmt.Sprintf("%d", pid)), 0644)

    // Memory limit: guest RAM + 128MB overhead for VMM
    memLimit := (memMB + 128) * 1024 * 1024
    os.WriteFile(filepath.Join(cgroupPath, "memory.max"),
        []byte(fmt.Sprintf("%d", memLimit)), 0644)

    // CPU: proportional to vCPU count
    // 100000 = 100ms period, quota = vcpus * 100ms
    cpuMax := fmt.Sprintf("%d 100000", vcpuCount*100000)
    os.WriteFile(filepath.Join(cgroupPath, "cpu.max"),
        []byte(cpuMax), 0644)

    // I/O: weight-based (not hard limit — avoids starvation)
    os.WriteFile(filepath.Join(cgroupPath, "io.weight"),
        []byte("100"), 0644)

    return nil
}
```

#### 2.2 Cleanup on Destroy

```go
func (e *Engine) cleanupCgroup(id string) {
    cgroupPath := fmt.Sprintf("/sys/fs/cgroup/bhatti/%s", id)
    os.Remove(filepath.Join(cgroupPath, "cgroup.procs")) // must be empty first
    os.Remove(cgroupPath)
}
```

#### 2.3 Configurable Limits

Add to `SandboxSpec` so the API can control per-sandbox limits:

```go
type SandboxSpec struct {
    // ... existing fields ...
    MemoryLimitMB int // cgroup memory.max (0 = default: MemoryMB + 128)
    CPUQuota      int // cgroup cpu.max numerator (0 = default: CPUs * 100000)
    IOWeight      int // cgroup io.weight (0 = default: 100)
}
```

#### 2.4 Systemd Unit Hardening

The bhatti daemon itself should also be constrained:

```ini
[Service]
# ... existing ...
MemoryMax=512M
# Protect host filesystem
ProtectSystem=strict
ReadWritePaths=/var/lib/bhatti /sys/fs/cgroup/bhatti
PrivateTmp=true
NoNewPrivileges=false  # jailer needs to set uid/gid
```

### Backwards Compatibility

No break. Existing VMs don't have cgroups — they just keep running
without limits until the daemon restarts. New VMs after restart get
cgroups. Cold VMs resumed after restart get cgroups applied post-launch.

---

## Part 3 — Disable Serial Console & Bound Stderr

**Impact: ✅ TRANSPARENT — no user-visible change**

### The Problem

Boot args enable the serial console:
```go
"console=ttyS0 reboot=k panic=1 pci=off ..."
```

FC stderr goes to the host process's stderr unbounded:
```go
fcCmd.Stderr = os.Stderr
```

The `prod-host-setup.md` doc warns: *"Without proper handling, because the
guest has access to the serial device, this can lead to unbound memory or
storage usage on the host side... we do not recommend that users enable
the serial device in production."*

A malicious guest can write to `/dev/ttyS0` at line rate, blowing up the
host process's memory or log storage.

### Design

#### 3.1 Disable Serial Device

Add `8250.nr_uarts=0` to boot args and remove `console=ttyS0`:

```go
bootArgs := fmt.Sprintf(
    "reboot=k panic=1 pci=off 8250.nr_uarts=0 init=/usr/local/bin/lohar quiet loglevel=0 ip=%s::%s:255.255.255.0::eth0:off:1.1.1.1:8.8.8.8:",
    guestIP, userNet.GatewayIP)
```

**Note**: the FC docs warn that the serial device can be reactivated from within
the guest even if disabled at boot. This is defense-in-depth, not a guarantee.

#### 3.2 Ring Buffer for Stderr

Replace `os.Stderr` with a bounded ring buffer per VM. On error, the last
N bytes of FC output are available for diagnostics (this directly addresses
RELIABILITY-AUDIT item #8):

```go
// ringBuffer is a fixed-size circular buffer that implements io.Writer.
// When full, oldest bytes are silently overwritten.
type ringBuffer struct {
    mu   sync.Mutex
    buf  []byte
    max  int
    pos  int
    full bool
}

func (r *ringBuffer) Write(p []byte) (int, error) {
    r.mu.Lock()
    defer r.mu.Unlock()
    if r.buf == nil {
        r.buf = make([]byte, r.max)
    }
    n := len(p)
    for _, b := range p {
        r.buf[r.pos] = b
        r.pos = (r.pos + 1) % r.max
        if r.pos == 0 { r.full = true }
    }
    return n, nil
}

func (r *ringBuffer) String() string {
    r.mu.Lock()
    defer r.mu.Unlock()
    if !r.full {
        return string(r.buf[:r.pos])
    }
    return string(r.buf[r.pos:]) + string(r.buf[:r.pos])
}
```

Store the ring buffer on the VM struct:

```go
type VM struct {
    // ... existing fields ...
    stderrBuf *ringBuffer // last 64KB of FC stderr
}
```

On snapshot restore failure, include the FC stderr in the error:

```go
if err := fcPut(ctx, client, "/snapshot/load", ...); err != nil {
    return fmt.Errorf("load snapshot: %w\nFC stderr: %s", err, vm.stderrBuf.String())
}
```

### Backwards Compatibility

Fully transparent. Existing cold snapshots are unaffected — boot args are
only used on fresh boot, not snapshot resume. The serial device state in
`vm.snap` is restored as-is regardless of boot args.

---

## Part 4 — Network Rate Limiters

**Impact: 🔄 ROLLING — new VMs only, existing VMs unaffected**

### The Problem

Network interfaces are created without rate limiters:

```go
fcPut(ctx, client, "/network-interfaces/eth0", fmt.Sprintf(
    `{"iface_id":"eth0","guest_mac":%q,"host_dev_name":%q}`, mac, tapName))
```

A single VM can saturate the host NIC. The `prod-host-setup.md` doc
recommends rate limiters: *"Network can be flooded... mitigated by
configuring rate limiters for the network interface."*

### Design

Add rate limiter configuration to the network interface setup:

```go
// Default: 100 Mbps with 10 MB burst
const (
    defaultNetBandwidthBytes = 12_500_000  // 100 Mbps in bytes/sec
    defaultNetBurstBytes     = 10_000_000  // 10 MB burst
    defaultNetRefillMs       = 1000        // 1 second refill
    defaultNetOpsPerSec      = 10000       // 10k packets/sec
    defaultNetOpsBurst       = 5000        // 5k packet burst
)

netConfig := fmt.Sprintf(`{
    "iface_id": "eth0",
    "guest_mac": %q,
    "host_dev_name": %q,
    "rx_rate_limiter": {
        "bandwidth": {"size": %d, "one_time_burst": %d, "refill_time": %d},
        "ops":       {"size": %d, "one_time_burst": %d, "refill_time": %d}
    },
    "tx_rate_limiter": {
        "bandwidth": {"size": %d, "one_time_burst": %d, "refill_time": %d},
        "ops":       {"size": %d, "one_time_burst": %d, "refill_time": %d}
    }
}`, mac, tapName,
    defaultNetBandwidthBytes, defaultNetBurstBytes, defaultNetRefillMs,
    defaultNetOpsPerSec, defaultNetOpsBurst, defaultNetRefillMs,
    defaultNetBandwidthBytes, defaultNetBurstBytes, defaultNetRefillMs,
    defaultNetOpsPerSec, defaultNetOpsBurst, defaultNetRefillMs)
```

Make limits configurable via `SandboxSpec`:

```go
type SandboxSpec struct {
    // ... existing fields ...
    NetBandwidthMbps int // 0 = default 100 Mbps
}
```

Rate limiters can also be PATCHed on running VMs (Firecracker supports
`PATCH /network-interfaces/{id}` with updated rate limiters). This enables
runtime adjustment without restart.

### Disk Rate Limiters

Similarly, disk drives support rate limiters. The rootfs drive should
have sane defaults:

```go
driveConfig := fmt.Sprintf(`{
    "drive_id": "rootfs",
    "path_on_host": %q,
    "is_root_device": true,
    "is_read_only": false,
    "rate_limiter": {
        "bandwidth": {"size": 104857600, "refill_time": 1000},
        "ops":       {"size": 10000, "refill_time": 1000}
    }
}`, rootfsPath)
```

### Backwards Compatibility

Existing running VMs are unaffected. Only newly created VMs get rate limiters.
Rate limiters can be added to running VMs via PATCH if needed.

---

## Part 5 — Use `network_overrides` for Snapshot Resume

**Impact: 🔄 ROLLING — simplifies resume, old snapshots work fine**

### The Problem

`ResumeSnapshot` has an elaborate workaround to match the TAP device name
and file paths that Firecracker has baked into `vm.snap`:

```go
// Must recreate TAP with the EXACT same name as original
origTapName := "tap" + fcOrigin[:8]
destroyTapDevice(origTapName)  // hope nobody else is using it
tapName = origTapName

// Must place files at original absolute paths (symlinks)
origSandboxDir := filepath.Join(e.cfg.DataDir, "sandboxes", fcOrigin)
os.MkdirAll(origSandboxDir, 0700)
for _, drive := range manifest.Drives {
    os.Symlink(src, dst)
}
```

This is fragile: it assumes the original sandbox dir is free, races with
concurrent resumes from the same snapshot, and requires tracking
`FCPathOrigin` across snapshot chains.

Firecracker's `/snapshot/load` API supports `network_overrides` which
remaps host device names at load time:

```json
{
    "snapshot_path": "...",
    "mem_backend": {...},
    "network_overrides": [
        {"iface_id": "eth0", "host_dev_name": "tapNEWNAME"}
    ]
}
```

### Design

#### 5.1 Use `network_overrides`

Generate a fresh TAP name for the new sandbox (as in `Create`), then pass
it via `network_overrides`:

```go
tapName, err = createTapDevice(id, userNet.BridgeName) // fresh name

loadPayload := fmt.Sprintf(`{
    "snapshot_path": %q,
    "mem_backend": {"backend_path": %q, "backend_type": "File"},
    "resume_vm": true,
    "enable_diff_snapshots": true,
    "network_overrides": [{"iface_id": "eth0", "host_dev_name": %q}]
}`, vmSnapPath, memSnapPath, tapName)
```

This eliminates:
- The `FCPathOrigin` tracking
- The `origTapName` recreation hack
- The `destroyTapDevice(origTapName)` race
- TAP name collision on concurrent resume

#### 5.2 File Path Workaround

`network_overrides` solves the TAP problem but not the drive paths.
Firecracker still opens drives at their original absolute paths from
`vm.snap`. The symlink approach remains necessary for drives until the
jailer is adopted (Part 1), at which point all paths are chroot-relative
and deterministic.

Keep the symlink approach for drives but remove the TAP workaround:

```go
// Create TAP with a fresh name (no more origTapName dance)
tapName, err = createTapDevice(id, userNet.BridgeName)

// Symlinks still needed for drives (until jailer makes paths relative)
origSandboxDir := filepath.Join(e.cfg.DataDir, "sandboxes", fcOrigin)
// ... symlink drives only, not TAP-related ...

// Load with network_overrides
loadPayload := fmt.Sprintf(`{...,"network_overrides":[{"iface_id":"eth0","host_dev_name":%q}]}`, tapName)
```

#### 5.3 Also Apply to `Start` (Cold→Hot Resume)

The same `network_overrides` should be used in `Start()` (engine.go:770)
which resumes from a thermal snapshot. Currently it also relies on the
original TAP surviving across stop/start. With `network_overrides`, we
can recreate the TAP with any name.

This has a secondary benefit: the TAP can be destroyed in `Stop()`,
freeing host resources while the VM is cold. Currently TAPs are preserved
across cold periods because FC expects the same name on resume.

### Backwards Compatibility

Fully compatible. `network_overrides` is additive — old snapshots that
don't have it simply don't use it. The symlink fallback for drives
remains.

---

## Part 6 — Entropy Device

**Impact: 🔄 ROLLING — new VMs only**

### The Problem

No `virtio-rng` device is configured. Guests rely on the kernel's own
entropy sources (timing jitter, interrupt noise), which in a minimal VM
can be extremely slow. This causes:

- Slow boot (blocking on `getrandom()`)
- Slow SSH key generation
- Degraded crypto performance (TLS handshakes, token generation)

The `entropy.md` doc describes the `/entropy` API endpoint.

### Design

Add entropy device configuration after machine-config, before boot:

```go
if err = fcPut(ctx, client, "/entropy", `{
    "rate_limiter": {
        "bandwidth": {"size": 1024, "one_time_burst": 0, "refill_time": 100}
    }
}`); err != nil {
    return info, fmt.Errorf("set entropy: %w", err)
}
```

The rate limiter (1 KB per 100ms = 10 KB/s) prevents a guest from draining
the host entropy pool. This is per the doc's recommendation.

### Backwards Compatibility

No break. Existing VMs don't have the device — they continue working as
before. New VMs get the entropy device. Snapshot-resumed VMs keep whatever
device set they were snapshotted with.

---

## Part 7 — Check `sync` Error Before Snapshot

**Impact: ✅ TRANSPARENT — internal correctness fix**

### The Problem

Both `Stop()` and `Checkpoint()` run `sync` in the guest but discard the
error:

```go
// engine.go:547-549
syncCtx, syncCancel := context.WithTimeout(context.Background(), 10*time.Second)
vm.Agent.Exec(syncCtx, []string{"sync"}, nil, "")  // error discarded!
syncCancel()
```

The snapshot docs state: *"The disk contents are not explicitly flushed to
their backing files."* If `sync` fails (agent unresponsive, guest crash),
the snapshot proceeds with dirty buffers → potential filesystem corruption
on resume.

### Design

Check the error and fail the snapshot if sync fails. This is more
conservative but prevents silent corruption:

```go
syncCtx, syncCancel := context.WithTimeout(context.Background(), 10*time.Second)
defer syncCancel()
if _, err := vm.Agent.Exec(syncCtx, []string{"sync"}, nil, ""); err != nil {
    slog.Error("guest sync failed before snapshot — proceeding anyway",
        "sandbox", sandboxID, "error", err)
    // Don't abort — a stale-but-complete snapshot is better than no snapshot.
    // The sync failure means some buffered writes may be lost, but the
    // filesystem journal will replay on next mount.
}
```

For `SnapshotAll` (daemon shutdown), log the warning but continue — a
potentially-stale snapshot is better than no snapshot.

For `Checkpoint` (user-initiated), return the error so the user can retry:

```go
if _, err := vm.Agent.Exec(syncCtx, []string{"sync"}, nil, ""); err != nil {
    return nil, fmt.Errorf("guest filesystem sync failed — retry or force with --no-sync: %w", err)
}
```

### Backwards Compatibility

Fully transparent. No external behavior change except that Checkpoint now
returns an error when sync fails (previously it silently continued).

---

## Part 8 — Firecracker Logger & Metrics

**Impact: 🔄 ROLLING — new VMs only**

### The Problem

No FC-native logging or metrics. The only FC output goes to stderr
(addressed in Part 3). The `logger.md` and `metrics.md` docs describe
structured logging and performance metrics endpoints.

Without FC metrics, there's no visibility into:
- VM boot time
- vCPU steal time
- I/O latency
- Memory balloon stats
- Seccomp violations

### Design

#### 8.1 Logger

Configure a per-VM log file before boot:

```go
logPath := filepath.Join(sandboxDir, "firecracker.log")
if err = fcPut(ctx, client, "/logger", fmt.Sprintf(
    `{"log_path":%q,"level":"Warning","show_level":true,"show_log_origin":true}`,
    logPath)); err != nil {
    slog.Warn("FC logger setup failed", "error", err) // non-fatal
}
```

Use `Warning` level in production (not `Debug` — the guest can influence
log volume). Rotate or cap log files to prevent unbounded growth, per the
doc's recommendation.

#### 8.2 Metrics

Configure a per-VM metrics sink:

```go
metricsPath := filepath.Join(sandboxDir, "firecracker.metrics")
if err = fcPut(ctx, client, "/metrics", fmt.Sprintf(
    `{"metrics_path":%q}`, metricsPath)); err != nil {
    slog.Warn("FC metrics setup failed", "error", err) // non-fatal
}
```

Metrics are written as newline-delimited JSON. A background goroutine
can periodically read and aggregate them for the bhatti metrics endpoint.

#### 8.3 Note on Snapshot Resume

The snapshot docs state: *"Configuration information for metrics and logs
are not saved to the snapshot. These need to be reconfigured on the
restored microVM."* This means `/logger` and `/metrics` must be
re-configured after every snapshot load.

**But**: Firecracker v1.6+ requires that `/snapshot/load` is the ONLY API
call on a fresh VMM — no pre-configuration of logger/metrics before load.
And post-load, the VMM is already running.

For snapshot resume, FC logger/metrics are not available. The ring buffer
stderr (Part 3) is the only diagnostic channel.

### Backwards Compatibility

No break. Logger and metrics are additive. They're configured before boot
on new VMs only. Existing/resumed VMs don't get them (no mechanism to
add post-boot).

---

## Part 9 — Code Quality & Robustness

**Impact: ✅ TRANSPARENT — all internal improvements**

### 9.1 Graceful Shutdown Before SIGKILL

Every FC process termination uses `Process.Kill()` (SIGKILL):

```go
vm.cmd.Process.Kill()  // 8 occurrences in engine.go, snapshot.go
```

Firecracker supports `SendCtrlAltDel` for guest-initiated shutdown and
responds to SIGTERM for clean VMM teardown. SIGKILL can leave dirty data
in host page cache and kernel structures.

```go
func (e *Engine) killFC(vm *VM, timeout time.Duration) {
    // Try SIGTERM first (FC flushes and exits cleanly)
    vm.cmd.Process.Signal(syscall.SIGTERM)

    done := make(chan error, 1)
    go func() { done <- vm.cmd.Wait() }()

    select {
    case <-done:
        return // clean exit
    case <-time.After(timeout):
        vm.cmd.Process.Kill() // hard kill after timeout
        <-done
    }
}
```

Use 3-second timeout for normal Stop, 1-second for Destroy.

### 9.2 Socket Path Length Validation

Unix socket paths are limited to 108 bytes on Linux. The current path
format is `/var/lib/bhatti/sandboxes/<16-hex>/firecracker.sock.resume`
which is ~65 bytes. A custom `DataDir` could overflow.

```go
func validateSocketPath(path string) error {
    if len(path) >= 108 {
        return fmt.Errorf("socket path too long (%d >= 108 bytes): %s — use a shorter data_dir", len(path), path)
    }
    return nil
}
```

Call this in `Create()` before starting FC.

### 9.3 Extract `startFC` Helper

Three call sites duplicate FC process launch logic (Create, Start,
ResumeSnapshot). Extract into a single function:

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

    // Wait for socket
    for i := 0; i < 50; i++ {
        if _, err := os.Stat(socketPath); err == nil {
            break
        }
        time.Sleep(20 * time.Millisecond)
    }

    return &fcProcess{cmd: cmd, cancel: cancel, stderrBuf: stderrBuf, socket: socketPath}, nil
}
```

### 9.4 Deduplicate Agent Guard Pattern

Every operation (Exec, Shell, FileRead, FileWrite, FileStat, FileList,
ListeningPorts, SessionList) has identical boilerplate:

```go
vm.stateMu.Lock()
if vm.Thermal != "hot" {
    vm.stateMu.Unlock()
    return ..., fmt.Errorf("sandbox %q is not hot (thermal=%s)", id, vm.Thermal)
}
ag := vm.Agent
vm.stateMu.Unlock()
```

Extract into a helper:

```go
// acquireHotAgent returns the agent for a hot VM, or an error if not hot.
func (e *Engine) acquireHotAgent(id string) (*agent.AgentClient, error) {
    vm, err := e.getVM(id)
    if err != nil {
        return nil, err
    }
    vm.stateMu.Lock()
    defer vm.stateMu.Unlock()
    if vm.Thermal != "hot" {
        return nil, fmt.Errorf("sandbox %q is not hot (thermal=%s)", id, vm.Thermal)
    }
    return vm.Agent, nil
}
```

This reduces 10+ copies of the same pattern to a single call site.

### 9.5 Avoid Creating HTTP Client per API Call

`fcAPIClient(socketPath)` is called on every FC API interaction, creating
a new `http.Client` and `http.Transport` each time. Store the client on
the VM struct:

```go
type VM struct {
    // ... existing fields ...
    fcClient *http.Client // cached FC API client
}
```

Initialize in `Create()` / `Start()`, nil out in `Stop()` / `Destroy()`.

### 9.6 Process Reaping

The RELIABILITY-AUDIT identified zombie FC processes on repeated restore
failures. Ensure `Kill()` + `Wait()` always pairs in error paths:

```go
defer func() {
    if err != nil {
        if fcProc != nil {
            fcProc.cmd.Process.Kill()
            fcProc.cmd.Wait() // reap zombie
            fcProc.cancel()
        }
    }
}()
```

This pattern already exists in `Create()` but is missing in some paths
of `Start()` and `ResumeSnapshot()`.

---

## Part 10 — Balloon Device & Hugepages (Optional)

**Impact: 🔄 ROLLING — new VMs only, fully optional**

### 10.1 Balloon Device

The `ballooning.md` doc describes a virtio balloon device that lets the
host reclaim guest memory dynamically. For a sandbox platform with many
VMs, this enables memory overcommit.

```go
if err = fcPut(ctx, client, "/balloon", fmt.Sprintf(
    `{"amount_mib": 0, "deflate_on_oom": true, "stats_polling_interval_s": 5}`)); err != nil {
    slog.Warn("balloon setup failed", "error", err)
}
```

The thermal manager could inflate the balloon on warm VMs (reclaim memory
without full snapshot):

```go
// In thermal cycle, for warm VMs:
fcPatch(ctx, client, "/balloon", fmt.Sprintf(
    `{"amount_mib": %d}`, vm.MemSizeMib / 2))  // reclaim half
```

This is a significant feature addition and should be a separate PR.

### 10.2 Hugepages

The `hugepages.md` doc notes up to 50% boot time improvement with 2MB
hugepages. Add support in machine-config:

```go
machineConfig := fmt.Sprintf(
    `{"vcpu_count":%d,"mem_size_mib":%d,"track_dirty_pages":%v,"huge_pages":%q}`,
    vcpuCount, memMB,
    !spec.UseHugepages, // dirty tracking and hugepages are mutually exclusive
    hugePageSize)        // "None" or "2M"
```

**Important caveat from the docs**: *"Enabling dirty page tracking for
hugepage memory negates the performance benefits of using huge pages."*
So hugepages are only for VMs that won't use diff snapshots (ephemeral
CI workloads, short-lived sandboxes).

### 10.3 Prerequisites for Hugepages

The host must have a pre-allocated hugepage pool:
```bash
echo 1024 > /proc/sys/vm/nr_hugepages  # 1024 * 2MB = 2GB pool
```

Add a check in the installer and a config option:
```yaml
hugepages_pool_mb: 2048  # 0 = disabled
```

### Backwards Compatibility

Both features are additive and optional. No effect on existing VMs.

---

## Summary

| Part | Change | Impact | Effort | Risk |
|------|--------|--------|--------|------|
| 1 | Jailer integration | ⚠️ BREAKING | 4-6 hr | High (rearchitecture) |
| 2 | cgroup resource limits | 🛑 DOWNTIME | 2 hr | Low |
| 3 | Serial console disable + stderr ring buffer | ✅ TRANSPARENT | 30 min | None |
| 4 | Network + disk rate limiters | 🔄 ROLLING | 30 min | None |
| 5 | `network_overrides` for snapshot resume | 🔄 ROLLING | 2 hr | Low |
| 6 | Entropy device | 🔄 ROLLING | 15 min | None |
| 7 | Sync error check before snapshot | ✅ TRANSPARENT | 15 min | None |
| 8 | FC logger + metrics | 🔄 ROLLING | 1 hr | None |
| 9 | Code quality (6 sub-items) | ✅ TRANSPARENT | 2 hr | None |
| 10 | Balloon + hugepages | 🔄 ROLLING | 2 hr | Low |

Total: ~15 hours of implementation. Parts 3, 4, 6, 7 can ship in a single
PR tomorrow. Parts 2, 5, 8, 9 are a second PR. Part 1 is a separate release
with migration docs. Part 10 is optional/future.
