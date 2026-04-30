# Jailer Integration → v1.0.0

## What the jailer does

The Firecracker jailer is a companion binary that wraps the FC process
with OS-level isolation:

1. **Chroot** — FC sees only its own directory, not the host filesystem
2. **PID namespace** — FC is PID 1 in its own namespace, can't see host
   processes
3. **UID drop** — FC runs as an unprivileged user, not root
4. **cgroup limits** — CPU and memory bounds per VM
5. **Device nodes** — creates `/dev/kvm` and `/dev/net/tun` inside the
   chroot so FC can function

Without the jailer, a VM escape (theoretical — FC has never had one)
gives the attacker root access to the entire host. With it, they land
in an empty chroot as an unprivileged user with no visibility of the
host filesystem or processes.

## What changes

### startFC becomes a branch

```go
func (e *Engine) startFC(socketPath string, opts startFCOpts) (*fcProcess, error) {
    if e.cfg.JailerBinary != "" {
        return e.startFCJailed(socketPath, opts)
    }
    return e.startFCBare(socketPath)
}
```

`startFCBare` is the current implementation — unchanged. Dev mode
(no jailer binary configured) continues to work exactly as today.

### startFCJailed

The jailer creates this directory structure:

```
<chroot_base>/firecracker/<vm-id>/root/
├── firecracker          (copied by jailer from --exec-file)
├── /dev/kvm             (mknod by jailer)
├── /dev/net/tun         (mknod by jailer)
├── rootfs.ext4          (hard-linked by us before jailer starts)
├── config.ext4          (hard-linked)
├── kernel               (hard-linked)
├── mem.snap             (hard-linked, for resume)
├── vm.snap              (hard-linked, for resume)
├── vol-*.ext4           (hard-linked, for volumes)
├── firecracker.log      (created by FC)
├── firecracker.metrics  (created by FC)
└── <api-sock>           (created by FC)
```

All paths FC sees are chroot-relative: `/rootfs.ext4`, `/config.ext4`,
etc. No absolute host paths in vm.snap — this eliminates the entire
FCPathOrigin symlink dance.

### File placement

Before invoking the jailer, bhatti must hard-link (not copy — same
filesystem, instant) all files FC needs into the chroot root:

```go
func (e *Engine) setupJailFiles(jailRoot string, opts jailFileOpts) error {
    // Hard-link kernel
    os.Link(e.cfg.KernelPath, filepath.Join(jailRoot, "kernel"))

    // Hard-link rootfs (writable by FC)
    os.Link(opts.RootfsPath, filepath.Join(jailRoot, "rootfs.ext4"))

    // Hard-link config drive
    os.Link(opts.ConfigPath, filepath.Join(jailRoot, "config.ext4"))

    // Hard-link volumes
    for _, vol := range opts.Volumes {
        os.Link(vol.FilePath, filepath.Join(jailRoot, "vol-"+vol.Name+".ext4"))
    }

    // Hard-link snapshot files (for resume)
    if opts.MemSnapPath != "" {
        os.Link(opts.MemSnapPath, filepath.Join(jailRoot, "mem.snap"))
        os.Link(opts.VMSnapPath, filepath.Join(jailRoot, "vm.snap"))
    }

    // chown the whole tree to jail user
    filepath.Walk(jailRoot, func(path string, info os.FileInfo, err error) error {
        return os.Lchown(path, e.cfg.JailUID, e.cfg.JailGID)
    })

    return nil
}
```

Hard-links work because everything is on the same btrfs filesystem.
The file appears in both the sandbox dir and the chroot — same inode,
same blocks, zero copy.

### Snapshot paths simplify

Inside the chroot, FC sees:
- `/rootfs.ext4` (not `/var/lib/bhatti/sandboxes/<id>/rootfs.ext4`)
- `/mem.snap`
- `/vm.snap`

When FC creates a snapshot, vm.snap records these short chroot-relative
paths. On resume, the jailer sets up the same chroot with the same
relative paths. No FCPathOrigin tracking, no symlink recreation, no
path mismatches. The entire symlink subsystem we built (and debugged
on agni-01) becomes unnecessary for jailed VMs.

### API socket path

The jailer creates the API socket inside the chroot. From the host's
perspective, it's at:

```
<chroot_base>/firecracker/<vm-id>/root/<api-sock>
```

We need to set `--api-sock` to a short relative path (e.g., `api.sock`)
to avoid the 108-byte Unix socket limit. The host-visible path is what
bhatti uses for HTTP API calls.

### cgroup limits

Free with the jailer's `--cgroup` flags:

```
--cgroup-version 2
--cgroup cpu.max=<vcpus * 100000> 100000
--cgroup memory.max=<(memMB + 128) * 1024 * 1024>
```

The +128MB overhead accounts for FC's own memory usage outside the guest.

### Config changes

```go
type Config struct {
    // ... existing fields ...
    JailerBinary string // path to jailer binary (empty = bare mode)
    JailUID      int    // uid for jailed FC processes
    JailGID      int    // gid for jailed FC processes
}
```

```yaml
# config.yaml
firecracker_jailer: /usr/local/bin/jailer
jail_uid: 10000
jail_gid: 10000
```

### System setup (one-time, on host)

```bash
# Create the jail user (no login, no home)
useradd -r -s /usr/sbin/nologin -u 10000 bhatti-vm

# Jailer binary (from FC release tarball)
cp jailer-v1.14.0-x86_64 /usr/local/bin/jailer
chmod +x /usr/local/bin/jailer
```

## What breaks

| Entity | Survives? | Why |
|--------|-----------|-----|
| **Volumes** | ✅ | Standalone ext4 files, hard-linked into chroot |
| **Images** | ✅ | Standalone rootfs files, used as source for copy/reflink |
| **Named snapshots** | ❌ | vm.snap has absolute paths from bare mode |
| **Thermal snapshots** | ❌ | Same — absolute paths in vm.snap |
| **Running sandboxes** | ❌ | FC processes started in bare mode |
| **state.db** | ✅ | Schema unchanged |

## Implementation plan

### Step 1: startFCJailed

Modify `startFC` to branch on `JailerBinary`. The jailed path:

1. Generate jail ID (= sandbox ID, safe characters only)
2. Create `<chroot_base>/firecracker/<id>/root/`
3. Hard-link kernel, rootfs, config, volumes into chroot root
4. chown the chroot tree to JailUID:JailGID
5. Build jailer command with all flags
6. Start jailer process (which starts FC inside the chroot)
7. Wait for API socket at host-visible path
8. Return fcProcess with the host socket path

### Step 2: Adjust all FC API paths

When jailed, FC configuration uses chroot-relative paths:
- `/rootfs.ext4` instead of `/var/lib/bhatti/sandboxes/<id>/rootfs.ext4`
- `/config.ext4` instead of full path
- `/kernel` instead of `/var/lib/bhatti/images/vmlinux-amd64`
- `/vol-<name>.ext4` for volumes

The Create() flow needs to pass these relative paths to the FC API
calls when in jailed mode.

### Step 3: Snapshot create/resume in jailed mode

**Create (Stop):** FC writes `vm.snap` and `mem.snap` inside the
chroot. Paths in vm.snap are chroot-relative. After FC exits, the
files are at `<chroot_root>/vm.snap` and `<chroot_root>/mem.snap`.
Copy (or hard-link) them to the sandbox dir for persistence.

**Resume (Start):** Set up a new chroot, hard-link the snapshot files
plus all drives into it, call `/snapshot/load` with chroot-relative
paths. No symlinks needed — the chroot IS the path namespace.

### Step 4: Cleanup

On Destroy, remove:
- The chroot directory (`<chroot_base>/firecracker/<id>/`)
- The cgroup (`/sys/fs/cgroup/firecracker/<id>/`)
- The sandbox directory (existing behavior)

### Step 5: Checkpoint (named snapshots)

Checkpoint copies block devices to the snapshot dir. In jailed mode,
the source files are inside the chroot. The copy uses host-visible
paths (the chroot root IS on the host filesystem). No change needed
in the copy logic — just the source paths.

The manifest records chroot-relative paths. On resume, a new chroot
is set up with the snapshot files hard-linked in.

### Step 6: Install script

Update `scripts/install.sh` to:
- Download and install the jailer binary alongside firecracker
- Create the `bhatti-vm` user if it doesn't exist
- Add `firecracker_jailer`, `jail_uid`, `jail_gid` to config.yaml

### Step 7: Pre-upgrade check

On daemon startup, if `JailerBinary` is configured but there are
existing sandboxes with absolute paths in their FC state (bare mode
sandboxes), log an error and refuse to start:

```
ERROR: jailer is configured but bare-mode sandboxes exist.
Destroy all sandboxes and snapshots before enabling the jailer.
See: bhatti.sh/docs/jailer-migration
```

This prevents silent data loss from trying to resume bare-mode
snapshots in jailed mode.

## Test plan

All tests run on real Firecracker with the jailer binary.

| Test | What it validates |
|------|-------------------|
| `TestJailerBootAndExec` | Boot VM via jailer, exec command, verify output |
| `TestJailerChroot` | FC process root is the chroot (`/proc/<pid>/root`) |
| `TestJailerUIDDrop` | FC runs as `bhatti-vm` user, not root |
| `TestJailerCgroupLimits` | `memory.max` and `cpu.max` set correctly |
| `TestJailerSnapshotRoundtrip` | Create → stop → start → exec works |
| `TestJailerCheckpointResume` | Checkpoint → destroy → resume from snapshot |
| `TestJailerVolumeAttach` | Volume data persists across create/destroy |
| `TestJailerDevModeFallback` | Empty JailerBinary → bare mode (existing tests pass) |
| `TestJailerCleanupOnDestroy` | Chroot dir and cgroup removed after destroy |
| `TestJailerNetworkOverrides` | network_overrides work in jailed mode |
| `TestJailerBalloon` | Balloon device works in jailed mode |
| `TestJailerFCLogger` | Log file created inside chroot |
| `TestJailerPreUpgradeCheck` | Bare-mode sandboxes block jailer startup |

## Migration for existing users

```bash
# 1. Save any important sandbox state to volumes
bhatti exec my-sandbox -- cp -r /important /workspace/

# 2. Save sandbox rootfs as images (if customized)
bhatti image save my-sandbox --name my-env

# 3. Delete all snapshots
bhatti snapshot list --json | jq -r '.[].name' | xargs -I{} bhatti snapshot delete {} --yes

# 4. Delete all sandboxes
bhatti list --json | jq -r '.[].id' | xargs -I{} bhatti destroy {} --yes

# 5. Update bhatti (install script installs jailer + creates user)
curl -fsSL bhatti.sh/install | sudo bash

# 6. Recreate sandboxes
bhatti create --name my-sandbox --image my-env --volume workspace:/workspace
```

## Not in this phase

- **Per-VM network namespaces** (Phase 6b) — full L2 isolation,
  significant networking rearchitecture, separate PR
- **seccomp filters** — FC has its own seccomp, jailer doesn't add more
- **Nested cgroup delegation** — for users who want to control cgroups
  inside the VM (container-in-VM scenarios)
