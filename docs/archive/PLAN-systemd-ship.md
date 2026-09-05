# Shipping systemd mode: what's left

Assessment of all POC work, gaps found, and remaining work before
this can go to production.

---

## What we proved

| Test | Pi 5 (ARM64) | agni (x86_64) |
|------|-------------|---------------|
| systemd boots, lohar runs as service | ✅ | ✅ |
| exec works | ✅ | ✅ |
| hostname from config drive | ✅ | ✅ |
| /etc/hosts written correctly | ✅ | ✅ |
| DNS (static resolv.conf) | ✅ | ✅ |
| systemctl is-system-running | ✅ running | ✅ running |
| Only 4 units running | ✅ | ✅ |
| lohar PID 1 path unbroken | ✅ 6ms boot | ✅ same |

### Boot timing (measured, honest)

| | Pi 5 (ARM64) | agni (x86_64) |
|---|---|---|
| lohar PID 1 | **367ms** | **345ms** |
| systemd | **708ms** | **568ms** |
| Delta | +341ms | +223ms |
| systemd-analyze userspace | 320ms | 253ms |

---

## What we did NOT test

These must pass before shipping:

### 1. Snapshot/restore with systemd

Never tested. The POC only did fresh creates. Need to verify:
- `bhatti stop` (snapshot) works on a systemd VM
- `bhatti start` (restore from snapshot) works
- systemd doesn't misbehave after restore (timer storms, service
  restarts, degraded state)
- `systemctl is-system-running` returns `running` after restore
- Processes started before snapshot survive after restore

### 2. Warm→hot resume with systemd

Never tested. The thermal manager pauses vCPUs. On ARM64, the
arch_timer hardware counter advances during pause, so
CLOCK_MONOTONIC jumps. Need to verify systemd handles this without
restarting services or going degraded.

### 3. Package install (the whole point)

Never tested. Need to verify:
- `apt-get install openssh-server` → sshd starts, DNS survives
- `apt-get install postgresql` → pg starts
- `apt-get install nginx` → nginx serves
- `apt-get install redis-server` → redis responds

The `systemd-resolved` apt pin is in the rootfs but untested with
actual package installs.

### 4. Docker/browser/computer tier boot profiles

The tier boot profiles (`/etc/bhatti/init.sh`) run in both modes.
But in systemd mode, they run as root inside a systemd-managed
service context. Need to verify:
- Docker tier: dockerd starts, docker commands work
- Browser tier: headless Chrome starts, CDP works
- Computer tier: KasmVNC + XFCE start

### 5. Exec regression benchmark

Never ran `bench/run.sh` on the systemd rootfs. Need A/B comparison
to verify no exec/file latency regression.

### 6. /tmp/boot-timing.txt broken in agent mode

During the POC, `bhatti file read` on the boot timing file returned
an error. The file doesn't exist in /tmp after boot. Likely cause:
lohar writes it before systemd's tmpfiles-setup runs, then tmpfiles
cleans /tmp. Or the write happens to the rootfs /tmp but systemd
later mounts tmpfs over it.

**Fix:** Write to `/run/bhatti/boot-timing.txt` instead of `/tmp/`.
`/run` is a tmpfs mounted by systemd very early and not cleaned by
tmpfiles.

---

## Code issues found during review

### 7. `runAsAgent()` duplicates 80% of the PID 1 path

The agent mode function is 119 lines that are almost entirely
copy-pasted from the PID 1 path: config drive reading, networking,
listeners, boot profile, init session, env loading. If either path
gets a bug fix or feature, the other will drift.

**Fix:** Extract shared logic into helper functions. The PID 1 path
calls them after doing mounts. The agent path calls them directly.
Something like:

```go
func main() {
    if os.Getpid() == 1 {
        initAsPID1()  // mounts, loopback, hostname, cgroups
    } else {
        initAsAgent() // just /dev/fuse chmod
    }
    // Shared path: config drive, networking, listeners,
    // boot profile, init session, block forever
    runAgent()
}
```

This reduces `runAsAgent()` to ~5 lines and `runAgent()` is the
shared code that both paths reach.

### 8. `create.go` init= detection is fragile

```go
if strings.Contains(baseImage, "systemd") {
    initBin = "/sbin/init"
}
```

This breaks if someone names an image `my-non-systemd-image.ext4`
with "systemd" in the name for documentation purposes. Also, it
only checks the image path, not the rootfs itself.

**Fix for production:** Add an explicit field to the create API or
store the init mode in the image metadata. For a cleaner approach,
check for `/sbin/init` existence inside the rootfs (the lohar
injection code already mounts the image). Or use a marker file
like `/etc/bhatti/systemd-mode` in the rootfs.

### 9. No `ensureResolvConf()` fallback in agent mode

In the PID 1 path, when the config drive has no DNS servers:
```go
} else {
    ensureResolvConf()
}
```

The agent mode skips this entirely when `cfg == nil` (no config
drive). The rootfs has a static resolv.conf baked in so it works
in practice, but the PID 1 path is more defensive.

**Fix:** Add the same `else { ensureResolvConf() }` to the agent
path, or handle it in the shared `runAgent()` refactor.

### 10. No config drive = no hostname/hosts in agent mode

If `loadConfigDrive()` returns nil (shouldn't happen in normal
operation but the PID 1 path handles it), the agent mode skips
hostname and /etc/hosts entirely. The PID 1 path sets hostname to
"bhatti" and writes hosts as fallback.

**Fix:** Add the same fallback to the agent path.

---

## Rootfs build issues

### 11. No build script in the repo

The systemd rootfs was built manually with chroot commands on each
machine. For production, need `scripts/tiers/systemd-minimal.sh`
that's reproducible and runs in CI.

### 12. The systemd rootfs is 1GB vs 512MB for minimal

The rootfs was grown to 1GB for headroom. Need to decide: is the
systemd tier a variant of minimal (replacing it) or a separate tier?
If all tiers build on minimal, and minimal gets systemd, the base
size grows. The 512MB minimal rootfs has 290MB free — systemd adds
~50MB, leaving 240MB. That's workable without growing to 1GB.

### 13. Masked services list is ad-hoc

We discovered services to mask through trial-and-error
(`systemd-analyze blame`, then mask, repeat). The full list was
built across multiple iterations. Need a canonical list in the
build script, with comments explaining why each is masked.

### 14. lohar.service unit file is not in the repo

The systemd unit file was created inline during chroot. Needs to
live in `scripts/` or `sandbox/` so it's version-controlled and
reviewable.

### 15. systemd-resolved apt pin is rootfs-only

The apt pin (`/etc/apt/preferences.d/no-resolved`) prevents resolved
from being installed. This is critical for DNS safety. But it's only
in the manually-built rootfs, not in any build script. If someone
rebuilds without it, openssh installs will break DNS again.

---

## Architecture decisions needed

### 16. Is systemd the default or opt-in?

Options:
- **a)** New tier: `rootfs-systemd-minimal`. Users choose with
  `--image systemd-minimal`. All other tiers stay on lohar PID 1.
- **b)** Replace minimal: the default `rootfs-minimal` gets systemd.
  All tiers inherit it. `--fast` flag or env var for lohar PID 1.
- **c)** Build flag: `scripts/build-tier.sh` takes `--systemd` flag.
  Each tier can be built with or without.

Recommendation: **(a)** for the first release. Ship as a separate
tier, get user feedback, graduate to default if stable.

### 17. How do other tiers adopt systemd?

If systemd-minimal becomes the base, docker/browser/computer tiers
inherit systemd. Their boot profiles (`/etc/bhatti/init.sh`) still
work — lohar runs them in both modes. But the long-term win is
converting them to proper systemd units:
- `dockerd.service` (ships with docker-ce, just enable it)
- `headless-chrome.service` (custom unit)
- `kasmvnc.service` + `xfce.service` (custom units)

This is a follow-up, not a blocker.

### 18. Snapshot/restore init= preservation

Confirmed: snapshot/restore doesn't re-set boot args. The kernel
cmdline (including `init=`) is baked into the snapshot from the
original create. A VM created with `init=/sbin/init` restores with
`init=/sbin/init`. No issue here.

The checkpoint/resume path (named snapshots) also doesn't touch boot
args. Clean.

---

## Cleanup needed on machines

### 19. Pi (raspi-5a)

- `/var/lib/bhatti/images/rootfs-systemd-arm64.ext4` — 1GB test rootfs
- `/usr/local/bin/bhatti` — POC binary (has systemd create.go change)
- `/var/lib/bhatti/lohar` — POC lohar (has runAsAgent)

The Pi is running the POC binaries, not the released v1.8.7. Need to
update to v1.8.7 (which only has the TAP fix, not the systemd changes).

### 20. agni

- `/var/lib/bhatti-test/` — 10GB test btrfs (mounted)
- `/var/lib/bhatti-test.img` — 10GB loopback file
- `/usr/local/bin/bhatti-test` — test binary
- `/usr/local/bin/bt` — wrapper script
- `/etc/bhatti-test/` — test config
- `/root/.bhatti-test/` — test client config
- Production bhatti has the TAP fix (v1.8.7 equivalent, `dev` build)

Need to unmount and clean the test artifacts. Production binary
should be updated to the tagged v1.8.7 from CI.

---

## Ordered checklist for shipping

### Phase 1: Clean up (now)

```
[ ] Update Pi to v1.8.7 release binary (revert POC binaries)
[ ] Clean agni test artifacts (unmount, rm test files)
[ ] Update agni to v1.8.7 release binary from CI
```

### Phase 2: Code quality (before merging)

```
[ ] Refactor runAsAgent() — extract shared logic, eliminate duplication (#7)
[ ] Add cfg==nil fallback in agent mode (hostname, resolv.conf) (#9, #10)
[ ] Write to /run/bhatti/boot-timing.txt instead of /tmp/ (#6)
[ ] Replace strings.Contains("systemd") with a robust detection (#8)
[ ] Add lohar.service to repo (scripts/ or sandbox/) (#14)
[ ] Create scripts/tiers/systemd-minimal.sh with canonical mask list (#11, #13)
[ ] Add systemd-resolved apt pin to build script (#15)
```

### Phase 3: Testing (before release)

```
[ ] Snapshot/restore test on Pi and agni (#1)
[ ] Warm→hot thermal cycle test (#2)
[ ] Package install test: openssh, postgresql, nginx, redis (#3)
[ ] Docker tier boot profile with systemd rootfs (#4)
[ ] bench/run.sh A/B comparison (#5)
[ ] Integration test suite against systemd rootfs
```

### Phase 4: Ship

```
[ ] Add systemd-minimal to CI release matrix
[ ] Update docs: cli-reference.md (new image option)
[ ] Update docs: tiers.md (new tier)
[ ] Update docs: limitations.md (what works now that didn't before)
[ ] Tag release
```
