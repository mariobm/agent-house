# Bhatti — FUSE Support

FUSE (Filesystem in USErspace) lets userspace programs implement filesystem
interfaces. The kernel handles VFS routing and forwards read/write/readdir
calls to a userspace daemon via `/dev/fuse`. This enables tools like sshfs,
rclone mount, s3fs-fuse, gcsfuse, AppImage, fuse-overlayfs, and Mesa
(programmable storage layer for AI agents) — all of which are commonly
needed inside development sandboxes.

Currently FUSE doesn't work. Three things are wrong, and fixing them
requires touching three layers of the stack: kernel, rootfs, and lohar.

---

## Current State

### Kernel: flag is not compiled

`docs/kernel.md` documents 16 flags — including `CONFIG_FUSE_FS=y` — as
if they are all active, with a complete build script showing
`scripts/config --enable CONFIG_FUSE_FS`. But the actual build script
(`scripts/build-kernel.sh`) only enables 11 of those 16 flags (the core
Docker networking flags). The other 5 — TUN, FUSE, WireGuard, TLS, and
the security pair (AppArmor + Landlock) — are documented in kernel.md
but never made it into the real build. Additionally, 4 Docker advanced
networking flags (DUMMY, MACVLAN, IPVLAN, VXLAN) appear in kernel.md
but not in the build script — and conversely, 5 flags in the build
script (BRIDGE, VETH, OVERLAY_FS, NF_CONNTRACK, NETFILTER_XT_MATCH_CONNTRACK)
are treated as defensive re-enables of flags the FC CI config already
provides. In total, the build script and docs are significantly out of
sync. (The build script also has a pre-existing bug: it says `"13 flags"`
on the echo line but only enables 11.)

**Result:** `CONFIG_FUSE_FS` is not set in the shipped kernel. Any attempt to
mount a FUSE filesystem inside a sandbox fails with:

```
mount: /mnt/remote: unknown filesystem type 'fuse.sshfs'.
```

Or, if using fusermount directly:

```
fusermount3: mount failed: No such device
```

Because `/dev/fuse` doesn't exist — the kernel doesn't register the FUSE
misc device when `CONFIG_FUSE_FS` is off.

### Rootfs: no FUSE userspace tools installed

None of the three rootfs tiers (minimal, browser, docker) install `fuse3`.
Even if the kernel had FUSE support, there's no `fusermount3` binary, no
`libfuse3`, and no `/etc/fuse.conf`.

`fusermount3` is the setuid-root helper that non-root users call to mount
FUSE filesystems. Without it, only root can mount FUSE. Since lohar runs
exec as uid 1000 (the `lohar` user), FUSE mounts would fail even with a
working kernel.

### validate.go: actively warns against FUSE

`pkg/oci/validate.go` checks for `fusermount`/`fusermount3` in
imported images and emits:

```
image contains FUSE tools — FUSE is not supported in the Firecracker guest kernel
```

This was correct when it was written (the kernel didn't have FUSE). After
this plan is implemented, the warning becomes wrong.

---

## The Fix

Three changes, one per layer. No API changes, no wire protocol changes,
no lohar code changes beyond what `/dev/fuse` needs. We add only
`CONFIG_FUSE_FS` — the other undocumented flags (TUN, WireGuard, TLS,
AppArmor, Landlock) can be added in separate releases when driven by
concrete use cases.

### Part 1 — Kernel: enable CONFIG_FUSE_FS in build-kernel.sh

The build script currently enables 11 flags (Docker networking, container
plumbing, traffic accounting). The echo line incorrectly says `"13 flags"`
— a pre-existing bug we fix here too. We add `CONFIG_FUSE_FS=y`, bringing
the real count to 12.

**Changes to `scripts/build-kernel.sh`:**

1. Add FUSE_FS to the audit loop (so the pre-build state is logged).
2. Add the `scripts/config --enable CONFIG_FUSE_FS` call.
3. Fix the echo line: `"13 flags"` → `"12 flags"`.
4. Add FUSE_FS to the post-build verification loop.

```bash
# --- Existing flags (already in script) ---
# CONFIG_IP_NF_RAW, CONFIG_IP6_NF_RAW
# CONFIG_BRIDGE, CONFIG_VETH, CONFIG_OVERLAY_FS
# CONFIG_NF_CONNTRACK, CONFIG_NETFILTER_XT_MATCH_CONNTRACK
# CONFIG_IP_NF_SECURITY, CONFIG_IP6_NF_SECURITY
# CONFIG_NET_CLS_CGROUP, CONFIG_NETFILTER_XT_MARK

# --- New: FUSE (sshfs, rclone, s3fs, Mesa, AppImage) ---
scripts/config --enable CONFIG_FUSE_FS
```

Add FUSE_FS to the post-build verification alongside the existing critical
flags:

```bash
for flag in IP_NF_RAW IP6_NF_RAW BRIDGE VETH OVERLAY_FS NF_CONNTRACK \
    NETFILTER_XT_MATCH_CONNTRACK FUSE_FS; do
    if ! grep -q "CONFIG_${flag}=y" .config; then
        echo "FATAL: CONFIG_${flag} not set to =y after olddefconfig" >&2
        MISSING=1
    fi
done
```

**Why `=y` and not `=m`.** Firecracker doesn't support loadable kernel
modules. There's no initramfs, no `modprobe`, no `/lib/modules`. Everything
must be compiled into the kernel. This is already the pattern for all 11
existing flags. `CONFIG_FUSE_FS=y` means the FUSE subsystem is always
present — the kernel registers `/dev/fuse` (major 10, minor 229) at boot
via the `misc` device framework.

**Kernel size impact.** FUSE adds ~60KB of kernel text (the `fs/fuse/`
directory compiles to `fuse.o` — about 45KB, plus some VFS glue). The
current kernel is ~43-45MB. This is noise.

**Dependency chain.** `CONFIG_FUSE_FS` depends on nothing exotic. It
requires `CONFIG_MISC_FILESYSTEMS` (already enabled in the FC CI config —
it's the parent menu for FUSE, overlayfs, etc.) and the standard VFS
infrastructure. `make olddefconfig` will resolve this automatically.

**Compatibility.** The new kernel is a strict superset of the old one.
Existing sandboxes, snapshots, and images work without changes. VMs that
don't use FUSE pay nothing — the FUSE code is idle, `/dev/fuse` exists
but is never opened.

**Snapshot note.** Snapshots taken with the old kernel and restored with
the new kernel work fine — the new kernel has everything the old one had,
plus FUSE. Snapshots taken with the new kernel cannot be restored with the
old kernel if FUSE was actively in use (the FUSE device state would be
missing), but this is a forward-only upgrade — we never downgrade kernels.

### Part 2 — Rootfs: install fuse3 in all tiers

**What to install.** The `fuse3` package on Ubuntu 24.04 provides:

- `/usr/bin/fusermount3` — setuid-root mount helper (allows uid 1000 to mount)
- `/usr/lib/*/libfuse3.so.*` — shared library for FUSE clients
- `/etc/fuse.conf` — configuration (notably `user_allow_other`)

FUSE clients (sshfs, rclone, s3fs) are NOT installed by default. They're
large and only needed by specific use cases. The user installs them via
`sudo apt-get install sshfs` or `rclone` binary download. We just provide
the kernel + plumbing so they work when installed.

**Why all tiers, not just browser/docker.** FUSE is a fundamental kernel
capability, like networking or filesystem mounting. It's a ~200KB addition
to the rootfs (the `fuse3` package is tiny — it's just the helper binary,
a small library, and a config file). Even the minimal tier should support
it — someone using the minimal tier for a lightweight dev environment
should be able to `apt install sshfs` and have it work.

**Changes to `scripts/tiers/minimal.sh`:**

Add `fuse3` to the apt-get install line:

```bash
apt-get install -y --no-install-recommends \
    iproute2 ca-certificates sudo curl locales fuse3
```

Since browser and docker tiers source minimal.sh first, they inherit FUSE
automatically. No changes to `scripts/tiers/browser.sh` or
`scripts/tiers/docker.sh`.

**Changes to `scripts/build-rootfs.sh`:**

`build-rootfs.sh` is the full-featured rootfs builder (zsh, Node.js,
Claude Code, starship, shell plugins). It's not legacy — it's the
production rootfs for the hosted product. The tiered builder
(`build-tier.sh`) produces lighter images for self-hosted / CI use.
Both need `fuse3`. Add it to the apt-get line in the chroot:

```bash
apt-get install -y --no-install-recommends \
    zsh git curl wget ca-certificates gnupg \
    tmux vim-tiny htop jq unzip xz-utils \
    locales sudo socat iproute2 \
    ripgrep fd-find fuse3
```

### Part 3 — Permissions: /dev/fuse and fuse.conf

Three things must be right for the `lohar` user (uid 1000) to mount FUSE
filesystems.

#### 3.1 /dev/fuse device node

When `CONFIG_FUSE_FS=y`, the kernel registers FUSE as a misc device at
boot. The FUSE misc driver calls `misc_register()` with minor 229. When
lohar mounts devtmpfs on `/dev` (`mustMount("devtmpfs", "/dev", ...)`
in `cmd/lohar/main.go`), the kernel auto-populates `/dev/fuse` with the
correct major/minor numbers.

**No lohar changes needed.** The existing `mustMount("devtmpfs", "/dev",
"devtmpfs", 0, "")` already handles this. devtmpfs is kernel-managed —
it automatically creates device nodes for all registered devices. Once
the kernel has FUSE, `/dev/fuse` appears in devtmpfs automatically.

Verification (after deploying new kernel):

```bash
bhatti exec test -- ls -la /dev/fuse
# crw-rw-rw- 1 root root 10, 229 ... /dev/fuse
```

**Permissions on `/dev/fuse`.** devtmpfs creates device nodes with the
permissions specified by the kernel driver. The FUSE misc driver registers
with mode `0666` (world read-write) — this is hardcoded in
`fs/fuse/inode.c`:

```c
static struct miscdevice fuse_miscdevice = {
    .minor = FUSE_MINOR,
    .name  = "fuse",
    .fops  = &fuse_dev_operations,
    .mode  = S_IRUGO | S_IWUGO,   /* 0666 */
};
```

So uid 1000 can open `/dev/fuse` directly. No udev rules, no group
changes, no chmod needed.

#### 3.2 fusermount3 setuid bit

`fusermount3` is installed setuid-root by the `fuse3` package:

```
-rwsr-xr-x 1 root root 35K ... /usr/bin/fusermount3
```

This is necessary because the `mount(2)` syscall requires `CAP_SYS_ADMIN`.
`fusermount3` uses its setuid-root privilege to call `mount(2)`, then drops
privileges. This is the standard FUSE security model — it's how every Linux
distribution ships FUSE.

The `fuse3` Ubuntu package sets the setuid bit during installation. No
manual `chmod u+s` needed. But we should verify it survives the rootfs
build process. The build-rootfs scripts use `chroot` + `apt-get install`,
which preserves setuid bits. The ext4 image format preserves permission
bits. And we never run a blanket `chmod -R` on the rootfs. So this works
out of the box.

Verification:

```bash
bhatti exec test -- stat -c '%a %U %n' /usr/bin/fusermount3
# 4755 root /usr/bin/fusermount3
```

#### 3.3 /etc/fuse.conf: user_allow_other

By default, FUSE mounts are only visible to the user who mounted them.
The mount is invisible to other UIDs, including root. This is a security
feature — it prevents a malicious FUSE filesystem from trapping root into
a fake directory tree.

For bhatti, this default is usually fine. The `lohar` user (uid 1000)
mounts FUSE filesystems and accesses them as uid 1000. No other UID
needs to see the mount.

However, some tools (notably `rclone mount` and Docker's `fuse-overlayfs`)
use the `-o allow_other` option so that other users (or containers) can
access the mount. This requires the `user_allow_other` setting in
`/etc/fuse.conf`.

The `fuse3` package installs `/etc/fuse.conf` with `user_allow_other`
commented out by default:

```
# /etc/fuse.conf
# user_allow_other
```

**Enable it in the rootfs build.** A bhatti sandbox is a single-user VM.
The isolation boundary is the VM, not filesystem permissions within it.
Enabling `user_allow_other` lets tools like rclone work with `-o allow_other`
when they need it, without requiring the user to edit fuse.conf.

**Changes to `scripts/tiers/minimal.sh`:**

After the apt-get install block, add:

```bash
# FUSE: allow -o allow_other (needed by rclone mount, fuse-overlayfs)
# Safe in a single-user VM — the VM itself is the isolation boundary.
sed -i 's/^#[[:space:]]*user_allow_other$/user_allow_other/' /etc/fuse.conf
```

The pattern is anchored on both ends (`^` and `$`) so it only matches
the standalone `#user_allow_other` line, not comment lines like
`# user_allow_other - Using the allow_other mount option...` that also
appear in fuse.conf. `[[:space:]]*` handles both `#user_allow_other`
and `# user_allow_other` (with a space). If the fuse3 package changes
the format in a future Ubuntu release, the sed is a no-op (doesn't
break, just doesn't uncomment). We could also write the file directly,
but sed preserves any other defaults the package ships.

Same change in `scripts/build-rootfs.sh`.

#### 3.4 fuse group membership

The `fuse3` package on Ubuntu 24.04 creates a `fuse` group. Historically
(fuse2 / older distros), membership in the `fuse` group was required to
access `/dev/fuse`. On modern Ubuntu with fuse3, `/dev/fuse` is mode 0666
(kernel-level, see 3.1), so group membership is technically not required
for opening the device.

However, some FUSE client tools check for group membership as an additional
safety measure, and `fusermount3` may check it on some configurations. Add
the lohar user to the fuse group as defense-in-depth — but guard the call,
because `fuse3` on Ubuntu 24.04 (noble) may not create the group (the
fuse2-era group is unnecessary when `/dev/fuse` is 0666):

```bash
# In the chroot, after useradd:
getent group fuse >/dev/null 2>&1 && usermod -aG fuse lohar || true
```

If the group exists, lohar is added. If not, the command is a no-op.
This prevents the rootfs build from failing on a missing group while
still covering edge cases where a FUSE tool checks gid.

### Part 4 — Remove the FUSE warning in validate.go

**`pkg/oci/validate.go`:**

Remove the fusermount check entirely:

```go
// Before:
if exists(rootDir, "usr/bin/fusermount") || exists(rootDir, "usr/bin/fusermount3") {
    warnings = append(warnings, "image contains FUSE tools — FUSE is not supported in the Firecracker guest kernel")
}

// After: (delete the block)
```

This warning was added in the v0.3 image import plan when the kernel
genuinely didn't have FUSE. After this plan, FUSE is supported. The
warning would be a lie.

**Also update `docs/kernel.md`:** The docs already describe FUSE as one
of the 16 flags. Add a note to the verification section clarifying which
flags are actually compiled into the shipped kernel vs. documented-but-
not-yet-enabled:

```
## Verification

After building, verify the 12 shipped flags are present:

\`\`\`bash
# Post-build check (also run automatically by build-kernel.sh)
for flag in IP_NF_RAW IP6_NF_RAW BRIDGE VETH OVERLAY_FS NF_CONNTRACK \
    NETFILTER_XT_MATCH_CONNTRACK IP_NF_SECURITY IP6_NF_SECURITY \
    NET_CLS_CGROUP NETFILTER_XT_MARK FUSE_FS; do
    ...
\`\`\`
```

**Seccomp.** Verified: lohar (`cmd/lohar/main.go`) does not install
seccomp filters, and Firecracker's default seccomp policy does not block
`mount(2)` or `/dev/fuse` ioctl calls — these are needed by the existing
devtmpfs mount logic and fusermount3 respectively. No seccomp changes
needed.

---

## Implementation

### build-kernel.sh — add FUSE_FS

One flag added, audit loop updated, echo fixed (was `"13 flags"` — a
pre-existing bug; now correctly says `"12 flags"`), verification loop
updated.

```diff
--- a/scripts/build-kernel.sh
+++ b/scripts/build-kernel.sh
@@ -35,7 +35,7 @@
 echo "==> Current state of bhatti flags in CI config:"
 for flag in IP_NF_RAW IP6_NF_RAW BRIDGE VETH OVERLAY_FS NF_CONNTRACK \
     NETFILTER_XT_MATCH_CONNTRACK IP_NF_SECURITY IP6_NF_SECURITY \
-    NET_CLS_CGROUP NETFILTER_XT_MARK; do
+    NET_CLS_CGROUP NETFILTER_XT_MARK FUSE_FS; do
     grep "CONFIG_${flag}[= ]" .config 2>/dev/null || echo "# CONFIG_${flag} is not set"
 done
 
-# Apply bhatti additions (idempotent — safe if already =y)
-echo "==> Applying bhatti kernel config (13 flags)..."
+echo "==> Applying bhatti kernel config (12 flags)..."
 
 # Docker bridge networking (hard blockers)
 scripts/config --enable CONFIG_IP_NF_RAW
@@ -62,6 +62,9 @@
 scripts/config --enable CONFIG_NET_CLS_CGROUP
 scripts/config --enable CONFIG_NETFILTER_XT_MARK
 
+# FUSE (sshfs, rclone, s3fs, Mesa, AppImage, fuse-overlayfs)
+scripts/config --enable CONFIG_FUSE_FS
+
 # Resolve dependencies
 make $CROSS olddefconfig
 
@@ -69,7 +72,7 @@
 echo "==> Verifying critical flags in final config..."
 MISSING=0
-for flag in IP_NF_RAW IP6_NF_RAW BRIDGE VETH OVERLAY_FS NF_CONNTRACK NETFILTER_XT_MATCH_CONNTRACK; do
+for flag in IP_NF_RAW IP6_NF_RAW BRIDGE VETH OVERLAY_FS NF_CONNTRACK NETFILTER_XT_MATCH_CONNTRACK FUSE_FS; do
     if ! grep -q "CONFIG_${flag}=y" .config; then
```

### tiers/minimal.sh — add fuse3

```diff
--- a/scripts/tiers/minimal.sh
+++ b/scripts/tiers/minimal.sh
@@ -9,7 +9,7 @@
 apt-get update -qq
 apt-get install -y --no-install-recommends \
-    iproute2 ca-certificates sudo curl locales
+    iproute2 ca-certificates sudo curl locales fuse3
 
 # Locale
 sed -i "/en_US.UTF-8/s/^# //g" /etc/locale.gen
@@ -18,6 +18,10 @@
 useradd -m -s /bin/bash -G sudo lohar
 echo "lohar ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers
 
+# FUSE: add lohar to fuse group (if group exists), enable user_allow_other
+getent group fuse >/dev/null 2>&1 && usermod -aG fuse lohar || true
+sed -i "s/^#[[:space:]]*user_allow_other$/user_allow_other/" /etc/fuse.conf
+
 apt-get clean
 rm -rf /var/lib/apt/lists/*
```

### build-rootfs.sh — add fuse3 (full-featured builder)

```diff
--- a/scripts/build-rootfs.sh
+++ b/scripts/build-rootfs.sh
@@ -95,7 +95,7 @@
 apt-get install -y --no-install-recommends \
     zsh git curl wget ca-certificates gnupg \
     tmux vim-tiny htop jq unzip xz-utils \
     locales sudo socat iproute2 \
-    ripgrep fd-find
+    ripgrep fd-find fuse3
 
 ...
 
 useradd -m -s /bin/zsh -G sudo lohar
 echo "lohar ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers
+getent group fuse >/dev/null 2>&1 && usermod -aG fuse lohar || true
+sed -i "s/^#[[:space:]]*user_allow_other$/user_allow_other/" /etc/fuse.conf
```

### validate.go — remove FUSE warning

```diff
--- a/pkg/oci/validate.go
+++ b/pkg/oci/validate.go
@@ -27,10 +27,6 @@
-	if exists(rootDir, "usr/bin/fusermount") || exists(rootDir, "usr/bin/fusermount3") {
-		warnings = append(warnings, "image contains FUSE tools — FUSE is not supported in the Firecracker guest kernel")
-	}
-
```

---

## What Doesn't Need to Change

- **Lohar (guest agent).** devtmpfs already handles `/dev/fuse`. FUSE is a guest userspace concern.
- **Bhatti daemon / API.** FUSE is entirely guest-side. No new endpoints, config, or feature flags.
- **Firecracker configuration.** No VM boot parameter or drive changes. FUSE is a kernel filesystem.
- **Firecracker seccomp.** Verified: Firecracker's default seccomp policy does not block `mount(2)` or the `/dev/fuse` ioctls. Lohar also installs no seccomp filters.
- **Config drive.** No FUSE-related config is needed per-sandbox.
- **Snapshots.** FUSE state is per-process. Warm pause/resume: mounts survive (daemon + kernel both frozen). Cold stop/start: daemon is killed, mount is gone, user remounts. Same as any userspace daemon (dockerd, sshd).

---

## Testing

### Kernel verification (post-build)

The build script's existing verification loop catches missing flags.
After adding FUSE_FS to the loop, a failed build exits with a clear
error. Additionally, boot a VM and check:

```bash
bhatti exec test -- zcat /proc/config.gz | grep CONFIG_FUSE_FS
# CONFIG_FUSE_FS=y

bhatti exec test -- ls -la /dev/fuse
# crw-rw-rw- 1 root root 10, 229 ... /dev/fuse
```

### Rootfs verification (post-build)

```bash
bhatti exec test -- which fusermount3
# /usr/bin/fusermount3

bhatti exec test -- stat -c '%a %U' /usr/bin/fusermount3
# 4755 root

bhatti exec test -- id lohar
# uid=1000(lohar) gid=1000(lohar) groups=1000(lohar),27(sudo),<fuse-gid>(fuse)

bhatti exec test -- cat /etc/fuse.conf
# user_allow_other
```

### Functional test: simple FUSE filesystem

A self-contained test using C and libfuse3. No pip, no Python — works
on the minimal tier. Installs build deps, compiles a trivial FUSE
filesystem, mounts, reads, unmounts.

```bash
# Install build deps (gcc + fuse3 headers)
bhatti exec test -- sudo apt-get update -qq
bhatti exec test -- sudo apt-get install -y --no-install-recommends gcc libfuse3-dev

# Write the FUSE hello world
bhatti exec test -- sh -c 'cat > /tmp/hellofs.c << "EOF"
#define FUSE_USE_VERSION 30
#include <fuse3/fuse.h>
#include <string.h>
#include <errno.h>

static const char *hello = "fuse works!\n";
static const size_t hello_len = 12;

static int fs_getattr(const char *path, struct stat *st, struct fuse_file_info *fi) {
    (void)fi;
    memset(st, 0, sizeof(*st));
    if (strcmp(path, "/") == 0)     { st->st_mode = S_IFDIR | 0755; st->st_nlink = 2; return 0; }
    if (strcmp(path, "/hello") == 0) { st->st_mode = S_IFREG | 0444; st->st_nlink = 1; st->st_size = hello_len; return 0; }
    return -ENOENT;
}
static int fs_readdir(const char *path, void *buf, fuse_fill_dir_t f, off_t o, struct fuse_file_info *fi, enum fuse_readdir_flags fl) {
    (void)o; (void)fi; (void)fl;
    f(buf, ".", NULL, 0, 0); f(buf, "..", NULL, 0, 0); f(buf, "hello", NULL, 0, 0); return 0;
}
static int fs_read(const char *path, char *buf, size_t size, off_t off, struct fuse_file_info *fi) {
    (void)fi;
    if (strcmp(path, "/hello") != 0) return -ENOENT;
    if ((size_t)off >= hello_len) return 0;
    if (off + size > hello_len) size = hello_len - off;
    memcpy(buf, hello + off, size);
    return size;
}
static struct fuse_operations ops = { .getattr = fs_getattr, .readdir = fs_readdir, .read = fs_read };
int main(int argc, char *argv[]) { return fuse_main(argc, argv, &ops, NULL); }
EOF'

# Compile, mount, verify, unmount
bhatti exec test -- gcc -Wall /tmp/hellofs.c -o /tmp/hellofs $(pkg-config --cflags --libs fuse3)
bhatti exec test -- mkdir -p /home/lohar/mnt
bhatti exec test -- /tmp/hellofs /home/lohar/mnt -f &   # foreground, backgrounded
sleep 1
bhatti exec test -- cat /home/lohar/mnt/hello
# fuse works!
bhatti exec test -- fusermount3 -u /home/lohar/mnt
```

For the Go integration test suite (see CI section below), this becomes a
`TestFUSEMount` that uses `execWithTimeout` to run each step inside the VM.

### Mesa volume test (post-release experiment)

Mesa is the motivating use case for this plan — a programmable storage
layer for AI agents that uses FUSE to mount Git repositories as local
directories. The `mesa mount --daemonize` command starts a FUSE daemon
that translates filesystem calls into Mesa API requests over gRPC/TLS.
This makes it an ideal real-world smoke test for FUSE behavior across
bhatti's thermal states.

**This section is a post-release experiment, not a CI gate.** It requires
a Mesa API key and external network access. Run manually after the
release ships.

**What we're testing:** How a long-lived FUSE process with network
connections behaves across bhatti's three thermal transitions.

#### Setup

```bash
# Create a sandbox
bhatti create --name mesa-test

# Install mesa CLI (uses apt on Linux)
bhatti exec mesa-test -- sh -c 'curl -fsSL https://mesa.dev/install.sh | sudo sh'

# Install FUSE dependencies (already in rootfs after this plan, but
# mesa also needs ca-certificates, libssl3, openssl for gRPC/TLS)
bhatti exec mesa-test -- sudo apt-get update
bhatti exec mesa-test -- sudo apt-get install -y ca-certificates libssl3 openssl

# Write mesa config (API key from environment)
bhatti exec mesa-test -- mkdir -p /home/lohar/.config/mesa
bhatti exec mesa-test -- sh -c 'cat > /home/lohar/.config/mesa/config.toml << EOF
api_key = "mesa_sk_..."
EOF'

# Mount mesa repos as a FUSE filesystem
bhatti exec mesa-test -- mesa mount --daemonize

# Verify the mount works
bhatti exec mesa-test -- ls ~/.local/share/mesa/mnt/my-org/my-repo
bhatti exec mesa-test -- cat ~/.local/share/mesa/mnt/my-org/my-repo/README.md
```

#### Test 1: Hot → Warm → Hot (pause/resume vCPUs)

When bhatti pauses a VM (hot→warm), vCPUs freeze. The mesa daemon's
event loop stops mid-iteration. The kernel's FUSE request queue freezes
with it. The gRPC connection to Mesa's API sits idle with TCP keepalive
timers frozen.

On resume (warm→hot), vCPUs restart. The mesa daemon's event loop
continues. Pending FUSE requests (if any) get processed. The gRPC
connection either resumes (if the server kept it alive) or reconnects.

```bash
# Write a file through the FUSE mount
bhatti exec mesa-test -- sh -c 'echo "before pause" > ~/.local/share/mesa/mnt/my-org/my-repo/test.txt'

# Wait for thermal manager to pause (or set a short warm timeout)
# Then wake with any exec:
bhatti exec mesa-test -- cat ~/.local/share/mesa/mnt/my-org/my-repo/test.txt
# Expected: "before pause" — mount survived pause/resume

# Write another file to verify the daemon is still functional
bhatti exec mesa-test -- sh -c 'echo "after pause" > ~/.local/share/mesa/mnt/my-org/my-repo/test2.txt'
bhatti exec mesa-test -- cat ~/.local/share/mesa/mnt/my-org/my-repo/test2.txt
# Expected: "after pause"
```

**What can go wrong:**

- **gRPC connection timeout.** Mesa's API server may close idle gRPC
  connections after a timeout (typically 60-120s). If the VM was paused
  longer than that, the daemon's gRPC channel is dead when vCPUs resume.
  A well-written FUSE daemon (which mesa should be) reconnects on the
  next filesystem operation. If it doesn't, the FUSE mount returns EIO
  or hangs. **This is the key thing to observe.**

- **TCP RST from intermediate hops.** NAT tables, firewalls, or cloud
  load balancers may expire the TCP connection state during the pause.
  On resume, the mesa daemon's next gRPC call gets a RST. Same recovery
  path as above — the daemon must reconnect.

- **Kernel FUSE queue overflow.** If a process had an in-flight FUSE
  request when the VM paused, the request sits in the kernel's FUSE
  queue. On resume, it's delivered to the daemon. The daemon's response
  goes back to the kernel. The calling process sees the operation
  complete. This should Just Work — the kernel FUSE queue is
  persistent across pause/resume.

#### Test 2: Hot → Cold → Hot (snapshot/restore)

When bhatti stops a VM (hot→cold), it creates a snapshot (CPU + memory
+ disk), then kills the Firecracker process. The mesa daemon, as a
userspace process, is included in the memory snapshot.

On start (cold→hot), Firecracker restores from the snapshot. The mesa
daemon's process is resurrected with its exact memory state. But:

- Its gRPC TCP connection is dead (the remote side has long closed it)
- The kernel's FUSE mount is restored (the device state is in the
  snapshot), but the `/dev/fuse` fd is reconnected
- The daemon's in-memory cache is stale (the snapshot is from the
  past, but Mesa's API may have newer data)

```bash
# Write through mesa, verify
bhatti exec mesa-test -- sh -c 'echo "before stop" > ~/.local/share/mesa/mnt/my-org/my-repo/snap-test.txt'
bhatti exec mesa-test -- cat ~/.local/share/mesa/mnt/my-org/my-repo/snap-test.txt

# Snapshot and stop
bhatti stop mesa-test

# Wait a while (simulates real cold storage)
sleep 30

# Restore
bhatti start mesa-test

# Test 1: Is the mount point still there?
bhatti exec mesa-test -- mount | grep fuse
# Expected: shows the mesa FUSE mount

# Test 2: Can we read?
bhatti exec mesa-test -- cat ~/.local/share/mesa/mnt/my-org/my-repo/snap-test.txt
# Expected: "before stop" — from daemon's cache or re-fetched from API
# OR: "Transport endpoint is not connected" — daemon died

# Test 3: Can we write?
bhatti exec mesa-test -- sh -c 'echo "after restore" > ~/.local/share/mesa/mnt/my-org/my-repo/snap-test2.txt'
# Expected: works (daemon reconnected to API)
# OR: Input/output error (daemon's gRPC channel is dead, no reconnect)
```

**What can go wrong:**

- **gRPC channel is permanently dead.** The daemon's TCP connection was
  serialized into the snapshot. On restore, the socket fd exists but
  the connection is severed. If the daemon doesn't detect this and
  reconnect, every FUSE operation returns EIO. **This is the most
  likely failure mode.** The daemon would need to be restarted:
  `fusermount3 -u ~/.local/share/mesa/mnt && mesa mount --daemonize`

- **FUSE mount is healthy but daemon cache is stale.** If another
  client wrote to the Mesa repo while the VM was cold, the restored
  daemon's cache doesn't know about it. The next read may return
  stale data until the daemon's cache invalidation kicks in. This is
  a mesa-side concern, not a bhatti concern, but worth observing.

- **Everything works perfectly.** If mesa's gRPC client library has
  reconnect logic (most modern gRPC clients do), and the FUSE fd
  survives snapshot/restore (it should — Firecracker restores all
  device state), then the mount Just Works after restore. This would
  be the ideal outcome.

#### Test 3: Rapid pause/resume cycles

The thermal manager can cycle between hot↔warm frequently (every 10s
under default config). Rapid transitions stress the daemon's connection
management:

```bash
# Run a workload that reads from mesa, with deliberate idle gaps
for i in $(seq 1 10); do
    bhatti exec mesa-test -- cat ~/.local/share/mesa/mnt/my-org/my-repo/README.md > /dev/null
    echo "Read $i OK"
    sleep 15  # long enough for warm timeout (default 10s)
done
```

Each iteration wakes the VM (warm→hot), does a FUSE read (which may
trigger a gRPC call), then lets the VM go idle (hot→warm). After 10
cycles, verify the daemon is still healthy.

#### What we learn from these tests

| Scenario | Expected | Watch for |
|----------|----------|-----------|
| Warm→hot | Mount works, daemon resumes | gRPC reconnect latency, first-read timeout |
| Cold→hot | Mount may or may not survive | Does mesa daemon auto-reconnect gRPC? |
| Rapid cycling | Mount works across cycles | Connection leak, fd leak, memory growth |

If cold→hot kills the mesa mount (likely), the fix is to add a mesa
remount to the sandbox's init script:

```bash
# /etc/bhatti/init.sh — re-establish mesa mount after cold restore
if command -v mesa >/dev/null 2>&1 && [ -f /home/lohar/.config/mesa/config.toml ]; then
    # Check if mount is healthy
    if ! ls ~/.local/share/mesa/mnt/ >/dev/null 2>&1; then
        fusermount3 -u ~/.local/share/mesa/mnt 2>/dev/null || true
        su - lohar -c 'mesa mount --daemonize'
    fi
fi
```

This is a user-level workaround, not a bhatti infrastructure change.
The same pattern applies to any FUSE daemon that doesn't survive
snapshot/restore (sshfs, rclone mount, etc.).

### Snapshot + FUSE interaction (generic)

Verify the basic kernel-level FUSE behavior across thermal states:

```bash
# Mount a FUSE filesystem (using hellofs from above)
bhatti exec test -- cat /home/lohar/mnt/hello
# fuse works!

# Warm (pause) — FUSE mount should survive
# Wait for thermal manager to pause, then wake:
bhatti exec test -- cat /home/lohar/mnt/hello
# fuse works!    ← mount survived pause/resume

# Cold (stop + start) — FUSE mount should be gone
bhatti stop test
bhatti start test
bhatti exec test -- cat /home/lohar/mnt/hello
# cat: /home/lohar/mnt/hello: No such file or directory
# OR: Transport endpoint is not connected
```

The "Transport endpoint is not connected" error is the expected Linux
behavior when a FUSE daemon dies but the mount point still exists. The
user runs `fusermount3 -u /home/lohar/mnt` and remounts.

### validate.go test update

The existing `TestValidateImageClean` passes (no FUSE tools in the temp
dir = no warning). Add a new test that explicitly includes fusermount3
and asserts no warning is emitted:

```go
func TestValidateImageWithFUSE(t *testing.T) {
    dir := t.TempDir()
    os.MkdirAll(filepath.Join(dir, "usr/bin"), 0755)
    os.WriteFile(filepath.Join(dir, "usr/bin/fusermount3"), []byte("x"), 0755)
    os.MkdirAll(filepath.Join(dir, "bin"), 0755)
    os.WriteFile(filepath.Join(dir, "bin/sh"), []byte("x"), 0755)
    os.WriteFile(filepath.Join(dir, "usr/bin/sudo"), []byte("x"), 0755)

    warnings := validateImage(dir)
    for _, w := range warnings {
        if strings.Contains(w, "FUSE") {
            t.Errorf("unexpected FUSE warning: %s", w)
        }
    }
}
```

### Integration CI (`integration.yml`)

The integration workflow builds the minimal rootfs on `ubuntu-24.04-arm`,
then runs the full test suite on `arc-runner-set` (bare-metal ARM with
`/dev/kvm`). After this plan, the rootfs built in CI includes `fuse3`,
so the integration tests exercise FUSE inside a real Firecracker VM.

Add a `TestFUSEBasic` to `pkg/engine/firecracker/integration_test.go`:

```go
func TestFUSEBasic(t *testing.T) {
	eng := testEngine(t)
	ctx := context.Background()

	info, err := eng.Create(ctx, testSpec("fuse-basic"))
	if err != nil {
		t.Fatalf("Create: %v", err)
	}
	defer eng.Destroy(ctx, info.ID)

	// /dev/fuse exists with correct major:minor
	r, _ := execWithTimeout(t, eng, info.ID, []string{"stat", "-c", "%t:%T", "/dev/fuse"})
	if strings.TrimSpace(r.Stdout) != "a:e5" { // major 10, minor 229
		t.Fatalf("/dev/fuse major:minor: %q, want a:e5", strings.TrimSpace(r.Stdout))
	}
	t.Log("✓ /dev/fuse exists (10:229)")

	// fusermount3 is setuid root
	r, _ = execWithTimeout(t, eng, info.ID, []string{"stat", "-c", "%a %U", "/usr/bin/fusermount3"})
	if !strings.HasPrefix(strings.TrimSpace(r.Stdout), "4755 root") {
		t.Fatalf("fusermount3 perms: %q, want 4755 root", strings.TrimSpace(r.Stdout))
	}
	t.Log("✓ fusermount3 is setuid root (4755)")

	// user_allow_other is enabled
	r, _ = execWithTimeout(t, eng, info.ID, []string{"grep", "-c", "^user_allow_other", "/etc/fuse.conf"})
	if strings.TrimSpace(r.Stdout) != "1" {
		t.Fatalf("fuse.conf: user_allow_other not enabled")
	}
	t.Log("✓ /etc/fuse.conf has user_allow_other")

	// CONFIG_FUSE_FS=y in running kernel
	r, _ = execWithTimeout(t, eng, info.ID, []string{"sh", "-c",
		"zcat /proc/config.gz | grep CONFIG_FUSE_FS"})
	if !strings.Contains(r.Stdout, "CONFIG_FUSE_FS=y") {
		t.Fatalf("kernel config: %q", r.Stdout)
	}
	t.Log("✓ CONFIG_FUSE_FS=y in running kernel")
}
```

This runs in CI on every tag push and `workflow_dispatch`, catching
regressions in the kernel config or rootfs build. The functional
hellofs test (compile + mount + read + unmount) can be added as a
separate `TestFUSEMount` when the CI runner has gcc/libfuse3-dev in
the rootfs, or as a manual post-build check.

---

## Rollout

### Phase 1: Kernel rebuild + rootfs rebuild

1. Update `scripts/build-kernel.sh` — add `CONFIG_FUSE_FS`
2. Rebuild kernel for x86_64 and aarch64
3. Verify `CONFIG_FUSE_FS=y` in both builds
4. Update `scripts/tiers/minimal.sh` — add `fuse3`, group, fuse.conf
5. Update `scripts/build-rootfs.sh` — same changes (full-featured builder)
6. Rebuild all three tier rootfs images for both architectures
7. Update `pkg/oci/validate.go` — remove FUSE warning
8. Update tests

### Phase 2: Release + upgrade path

The kernel and rootfs are release artifacts. They ship as part of the next
bhatti release. Users get FUSE support automatically on `bhatti update`
(which downloads the new kernel + rootfs).

**The upgrade problem.** Existing sandboxes do NOT get FUSE automatically.
Both the kernel and rootfs are baked in at create time:

| Sandbox state | What happens on `bhatti update` |
|---------------|--------------------------------|
| Hot (running) | Nothing. Kernel is loaded, rootfs is the copy from create time. |
| Warm (paused) | Nothing. Resume uses the in-memory kernel + rootfs. |
| Cold (snapshot) | Nothing. Snapshot restore loads the exact kernel + memory state from when it was snapshotted. |
| Destroyed + recreated | **Gets the new kernel + rootfs.** This is the only path. |

There is no hot-upgrade path for kernels or rootfs. This is fundamental
to Firecracker's architecture (the kernel is a boot parameter, not a
swappable component). The same constraint applies to every Firecracker
kernel change we've ever shipped.

**Preserving user data across recreate.** Volumes (`/workspace` and
user-attached volumes) are stored as separate ext4 images on the host.
They survive `bhatti destroy` + `bhatti create` — only the rootfs and
kernel are replaced. User files in `/home/lohar` (dotfiles, SSH keys,
installed packages) are on the rootfs and are lost on recreate.

For users who have significant environment customization, the
recommended workflow is:

1. Export dotfiles: `bhatti exec dev -- tar czf /workspace/.dotfiles.tar.gz -C /home/lohar .bashrc .ssh .gitconfig` (or whatever they care about)
2. Destroy + recreate: `bhatti destroy dev && bhatti create --name dev --volume mydata:/workspace`
3. Re-import: `bhatti exec dev -- tar xzf /workspace/.dotfiles.tar.gz -C /home/lohar`

**Detection.** The `bhatti` CLI should detect sandboxes running with an
old kernel and surface a one-line hint:

```
$ bhatti ls
NAME    STATE   KERNEL          NOTE
dev     hot     6.1.155-old     → recreate to get FUSE support
prod    cold    6.1.155-old     → recreate to get FUSE support
new     hot     6.1.155         (current)
```

This can be done by comparing the kernel hash in the sandbox metadata
against the current release's kernel hash. Implementation is a follow-up
task — not blocking this release, but should ship soon after.

**Communication to users:**

```
## What's new in v0.X.Y

### FUSE support

FUSE filesystems now work inside sandboxes. Install and use sshfs, rclone
mount, s3fs-fuse, gcsfuse, or any FUSE-based tool:

    bhatti exec dev -- sudo apt-get install -y sshfs
    bhatti exec dev -- sshfs user@server:/data /mnt/remote

Existing sandboxes must be recreated to pick up the new kernel and rootfs.
Volumes are preserved across recreate:

    # Save any dotfiles/config to your volume first
    bhatti exec dev -- cp ~/.gitconfig /workspace/
    bhatti destroy dev
    bhatti create --name dev --volume mydata:/workspace
```

### Phase 3: Mesa experiment

After the kernel + rootfs ship, run the Mesa test suite above on a
real deployment. Document findings — particularly cold→hot behavior —
and decide whether bhatti needs any infrastructure-level support for
FUSE daemon recovery or whether it's purely a user-space concern.

### Dependency graph

```
Kernel rebuild ────────────────────┐
                                   ├──→ Phase 2: Release (kernel + rootfs + binary)
Rootfs rebuild (needs fuse3 pkg) ──┘
                                   ↑
validate.go + integration test ────┘ (ships with the bhatti binary)
                                   ↓
                            Phase 3: Mesa experiment (post-release)
```

The kernel and rootfs rebuilds are independent of each other — they can
happen in parallel. Both must complete before the release. The validate.go
change and `TestFUSEBasic` ship with the bhatti binary (Go code), which is
also part of the release. The Mesa experiment runs after deployment.

---

## What's Explicitly Not in This Plan

**Installing FUSE clients (sshfs, rclone, s3fs) in the rootfs.** These
are large, opinionated, and only needed by specific workflows. The rootfs
provides the plumbing (kernel + fusermount3 + libfuse3). Users install
the clients they need. This is the same philosophy as Docker — we don't
pre-install Docker clients, just the daemon.

**CUSE (Character Device in USErspace).** `CONFIG_CUSE` enables userspace
character device drivers. Extremely niche (used by some audio/MIDI
software). Not worth the kernel text.

**virtiofs / virtio-fs.** Firecracker does not support virtiofs (it
requires vhost-user, which FC doesn't implement). virtiofs would be the
"right" way to share host directories with guests, but it's a FC
limitation, not a kernel limitation. FUSE inside the guest is the
workaround — mount remote filesystems from within the guest.

**FUSE-specific bhatti API.** No `bhatti mount` command. FUSE mounts are
a guest-side concern — the user runs sshfs/rclone via `bhatti exec`. The
host doesn't need to know about FUSE mounts.

**Persisting FUSE mounts across cold snapshot/restore.** FUSE daemons are
userspace processes — they die on snapshot and don't come back. The user
remounts after restore. This could theoretically be handled by an init
script (`/etc/bhatti/init.sh`) that re-establishes mounts on boot, but
that's user-level configuration, not bhatti infrastructure.

**fuse-overlayfs in the rootfs.** The Docker tier uses kernel overlayfs
(`CONFIG_OVERLAY_FS=y`). `fuse-overlayfs` is only needed for rootless
containers where the kernel overlay doesn't support unprivileged mounts.
Since the Docker tier runs `dockerd` as root inside the VM, kernel
overlayfs works. fuse-overlayfs is unnecessary.

**Automatic FUSE daemon recovery after cold restore.** If the Mesa
experiment shows that cold→hot kills FUSE daemons (likely), we could
add infrastructure to lohar that detects stale FUSE mounts on restore
and cleans them up. But this is premature — the user can handle it
via init scripts, and different FUSE daemons have different restart
semantics. Wait for the experiment results.

**Other kernel.md flags (TUN, WireGuard, TLS, AppArmor, Landlock).**
kernel.md documents these as active but `build-kernel.sh` doesn't enable
them yet. They should be added individually as use cases arise, each with
its own verification. After this plan ships FUSE, `kernel.md` should be
updated to clearly distinguish "shipped in build-kernel.sh" (12 flags)
from "documented but not yet enabled" (TUN, WireGuard, TLS, AppArmor,
Landlock, DUMMY, MACVLAN, IPVLAN, VXLAN). No need to batch them with
FUSE.
