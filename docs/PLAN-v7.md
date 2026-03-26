# Bhatti v0.4 — Kernel, Rootfs Tiers, Installation

v0.3 shipped images, persistent volumes, named snapshots, and OCI support.
The platform primitives are complete. v0.4 makes bhatti installable in
30 seconds instead of 10 minutes, ships three rootfs application profiles,
and builds a custom kernel that enables Docker inside VMs.

---

## Current State

The install script (`scripts/install.sh`) runs `build-rootfs.sh` which:
1. Creates a 2GB ext4 image
2. Runs debootstrap (~200MB of .debs, 3-5 min)
3. Installs 20+ packages in chroot (zsh, git, node, claude-code, etc.)
4. Clones 7 git repos for shell plugins
5. Downloads starship, node.js tarballs

Total: **10+ minutes**, requires root + debootstrap + internet, and is
non-reproducible (packages update between runs). The rootfs is a monolith
that includes everything whether the user needs it or not.

The kernel is the stock Firecracker CI kernel (v1.6 era, 6.1.58). It
lacks `CONFIG_NETFILTER` entirely — no iptables inside VMs. Docker,
WireGuard, TUN/TAP, and FUSE are all broken inside guests.

---

## Design Principle: Rootfs = Application Profile

Bhatti is programmable Linux. The rootfs defines what kind of Linux.
Three profiles ship, each serving a different use case:

```
minimal   — bare Ubuntu + lohar deps. The base layer.
browser   — minimal + headless Chromium + Playwright. Browser automation.
docker    — minimal + Docker Engine. Containers inside VMs.
```

There is no "default" tier with dev tools pre-installed. Users who want
zsh/git/node/claude-code boot from `minimal`, install what they want,
then `bhatti image save` to snapshot their custom environment. This is
the primary workflow — ship minimal, users build their own.

### Why no default tier

The current rootfs has: zsh, zinit, 3 zinit plugins, starship, tmux,
3 tmux plugins, vim-tiny, htop, jq, ripgrep, fd-find, git, curl, wget,
socat, unzip, xz-utils, node 22, claude-code, custom .zshrc, custom
.tmux.conf. None of this is needed by bhatti. It's one developer's
opinionated setup baked into every installation.

Users who want this exact setup can build it once and save it as an
image. Users who want Python instead of Node, or fish instead of zsh,
or no shell customization at all, aren't forced to carry 500MB of
someone else's preferences.

---

## Part 1 — Custom Kernel

### 1.1 Why

The stock Firecracker CI kernel (v1.6 era) is missing:

| Missing | Blocks |
|---------|--------|
| `NETFILTER` (entire subsystem) | Docker networking, iptables inside VMs |
| `TUN` | VPNs (Tailscale, WireGuard-go, cloudflared) |
| `FUSE_FS` | sshfs, rclone mount, s3fs, AppImage |
| `WIREGUARD` | Kernel WireGuard (10x faster than wireguard-go) |
| `TLS` | Kernel TLS offload (nginx, HAProxy) |
| `SECURITY_APPARMOR` | Docker's default MAC enforcement |

The v1.15 CI kernel (6.1.155) has `NETFILTER` and iptables but is
missing `IP_NF_RAW` — a single flag that Docker 28+ requires for
bridge networking.

**Verified on agni-01 (2026-03-26):** Booted the v1.15 kernel with
Firecracker v1.14.0. Docker 29.3.1 starts, pulls images, runs
`hello-world` with `--network host`. Bridge networking fails with
`iptables: can't initialize table 'raw'`. `/proc/config.gz` confirms
`CONFIG_IP_NF_RAW is not set`.

### 1.2 What we change

Start from the Firecracker CI v1.15 config (6.1.155). Enable 16 flags:

```bash
# Docker bridge networking (blocks container creation without these)
scripts/config --enable CONFIG_IP_NF_RAW
scripts/config --enable CONFIG_IP6_NF_RAW

# Docker security tables (AppArmor network labels)
scripts/config --enable CONFIG_IP_NF_SECURITY
scripts/config --enable CONFIG_IP6_NF_SECURITY

# Docker advanced networking (macvlan, ipvlan, overlay networks)
scripts/config --enable CONFIG_DUMMY
scripts/config --enable CONFIG_MACVLAN
scripts/config --enable CONFIG_IPVLAN
scripts/config --enable CONFIG_VXLAN

# Docker traffic accounting
scripts/config --enable CONFIG_NET_CLS_CGROUP
scripts/config --enable CONFIG_NETFILTER_XT_MARK

# TUN/TAP (VPNs, userspace networking)
scripts/config --enable CONFIG_TUN

# FUSE (sshfs, rclone mount, s3fs, AppImage)
scripts/config --enable CONFIG_FUSE_FS

# WireGuard VPN (kernel implementation, 10x faster than wireguard-go)
scripts/config --enable CONFIG_WIREGUARD

# Kernel TLS offload (nginx ssl_conf_command Options KTLS)
scripts/config --enable CONFIG_TLS

# Security: AppArmor (Docker's default MAC) + Landlock (unprivileged sandboxing)
scripts/config --enable CONFIG_SECURITY_APPARMOR
scripts/config --enable CONFIG_SECURITY_LANDLOCK
```

All flags are `=y` (built-in). Firecracker doesn't support loadable modules.

### 1.3 What we deliberately skip

**NF_TABLES + NFT_\*:** Docker works with `iptables-legacy`. nftables adds
~20 config flags for zero benefit in our use case.

**IP_VS\*:** IPVS is Docker Swarm load balancing. Nobody runs Swarm inside
a single Firecracker VM.

**DRM / framebuffer:** No GPU in Firecracker. Computer-use tier (future)
uses Xvfb which works without kernel DRM.

**FTRACE:** Kernel function tracing adds overhead to every function call
even when disabled (nop sled in function prologues). Not worth it for
application workloads.

### 1.4 One kernel for all tiers

Every flag we enable is inert when unused. Empty iptables tables, no TUN
devices created, no FUSE mounts. Zero runtime overhead for minimal/browser
tiers. Ship one kernel artifact for all tiers.

### 1.5 Build script

**File:** `scripts/build-kernel.sh`

```bash
#!/bin/bash
# Build the bhatti kernel from Firecracker CI config + additional flags.
# Usage: ./scripts/build-kernel.sh [arch]
#   arch: x86_64 (default) or aarch64
#
# Requirements: build-essential flex bison libelf-dev libssl-dev bc
# For aarch64 cross-compile: gcc-aarch64-linux-gnu
set -euo pipefail

ARCH="${1:-x86_64}"
KERNEL_VERSION="6.1.155"
FC_CI_VERSION="v1.15"

case "$ARCH" in
    x86_64)  KARCH="x86_64"; CROSS="" ;;
    aarch64) KARCH="arm64";  CROSS="ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu-" ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

# Download kernel source if not cached
if [ ! -d "linux-${KERNEL_VERSION}" ]; then
    echo "==> Downloading kernel ${KERNEL_VERSION} source..."
    curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${KERNEL_VERSION}.tar.xz" | tar xJ
fi
cd "linux-${KERNEL_VERSION}"

# Start from Firecracker CI config
echo "==> Downloading Firecracker CI config (${FC_CI_VERSION}/${ARCH})..."
curl -fsSL "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/${FC_CI_VERSION}/${ARCH}/vmlinux-${KERNEL_VERSION}.config" -o .config

# Apply bhatti additions
echo "==> Applying bhatti kernel config..."

# Docker bridge networking
scripts/config --enable CONFIG_IP_NF_RAW
scripts/config --enable CONFIG_IP6_NF_RAW
scripts/config --enable CONFIG_IP_NF_SECURITY
scripts/config --enable CONFIG_IP6_NF_SECURITY

# Docker advanced networking
scripts/config --enable CONFIG_DUMMY
scripts/config --enable CONFIG_MACVLAN
scripts/config --enable CONFIG_IPVLAN
scripts/config --enable CONFIG_VXLAN
scripts/config --enable CONFIG_NET_CLS_CGROUP
scripts/config --enable CONFIG_NETFILTER_XT_MARK

# TUN/TAP, FUSE, WireGuard, kTLS
scripts/config --enable CONFIG_TUN
scripts/config --enable CONFIG_FUSE_FS
scripts/config --enable CONFIG_WIREGUARD
scripts/config --enable CONFIG_TLS

# Security
scripts/config --enable CONFIG_SECURITY_APPARMOR
scripts/config --enable CONFIG_SECURITY_LANDLOCK

# Resolve dependencies
make $CROSS olddefconfig

# Build
echo "==> Building vmlinux ($(nproc) cores)..."
make $CROSS -j$(nproc) vmlinux

mkdir -p ../dist
cp vmlinux "../dist/vmlinux-${KERNEL_VERSION}-${ARCH}"
echo "==> Built: dist/vmlinux-${KERNEL_VERSION}-${ARCH} ($(du -h "../dist/vmlinux-${KERNEL_VERSION}-${ARCH}" | cut -f1))"
```

### 1.6 Verification

After building, boot a VM and check:

```bash
bhatti exec test -- sh -c 'zcat /proc/config.gz | grep CONFIG_IP_NF_RAW'
# CONFIG_IP_NF_RAW=y

bhatti exec test -- sudo iptables -t raw -L
# Chain PREROUTING (policy ACCEPT) ...

bhatti exec test -- sudo docker run --rm hello-world
# Hello from Docker!
```

### 1.7 Firecracker version

The kernel and Firecracker must be from the same generation. The v1.15 CI
kernel doesn't boot with Firecracker v1.6 (kernel panic: `VFS: Cannot open
root device "vda"`). **Upgrade to Firecracker v1.14.0** (latest stable in
the v1.15 CI artifacts).

Verified on agni-01: Firecracker v1.14.0 + kernel 6.1.155 boots and runs
correctly.

### 1.8 Engineering overhead

We are NOT patching kernel source or maintaining a fork. We take the exact
`.config` that Firecracker's CI team maintains (tested against every FC
commit), flip 16 flags from `n` to `y`, and run `make vmlinux`.

When Firecracker updates their CI kernel (e.g., 6.1.155 → 6.1.170 for a
CVE), we download their new config, apply the same 16 flags, rebuild.
A 15-minute task that runs in CI.

### 1.9 Tests

- `TestKernelHasNetfilterRaw` — boot VM, `zcat /proc/config.gz | grep IP_NF_RAW` → `=y`
- `TestKernelHasTUN` — boot VM, `ls /dev/net/tun` → exists
- `TestKernelHasFUSE` — boot VM, `ls /dev/fuse` → exists
- `TestKernelHasWireGuard` — boot VM, `ip link add wg-test type wireguard` → succeeds
- `TestDockerBridgeNetworking` — boot docker tier, start dockerd,
  `docker run --rm alpine ping -c1 8.8.8.8` → succeeds (bridge, not host network)

---

## Part 2 — Rootfs Tiers

### 2.1 Minimal (~100MB uncompressed)

The thinnest image that lohar can boot and users can work in.

**Contents:**
- Ubuntu 24.04 minbase (debootstrap `--variant=minbase`)
- `iproute2` — lohar calls `ip addr add`, `ip route add` for network setup;
  engine calls `ss -tln` via exec for port discovery
- `ca-certificates` — TLS from inside the VM
- `sudo` — lohar runs exec as uid 1000, users need root escalation
- `curl` — basic HTTP client for bootstrapping (installing tools, etc.)
- `bash` — comes with minbase, needed as fallback shell
- `lohar` binary at `/usr/local/bin/lohar`
- `lohar` user (uid 1000, gid 1000) with NOPASSWD sudo
- `/workspace` directory owned by lohar
- Static `/etc/resolv.conf` (1.1.1.1, 8.8.8.8)
- `en_US.UTF-8` locale

**Not included:** zsh, git, node, tmux, vim, htop, jq, ripgrep, fd-find,
socat, starship, any shell plugins, any shell configs. Users install what
they need.

**Use case:** Base layer for OCI images. Users who want full control.
CI runners that install their own deps. The "start from nothing" path.

### 2.2 Browser (~600MB uncompressed)

Headless Chromium with Playwright for browser automation.

**Contents (everything in minimal, plus):**
- Chromium (headless-capable, from Ubuntu repos)
- Chromium dependencies (libatk, libcups, libnss3, etc.)
- Playwright system dependencies (`npx playwright install-deps`)
- Node.js 22.x LTS (Playwright needs it)
- Python 3.12 (Playwright Python bindings, pre-installed with `pip install playwright`)
- Boot profile: `/etc/bhatti/init.sh` starts Chromium with CDP on port 9222

**Boot profile (`/etc/bhatti/init.sh`):**
```bash
#!/bin/sh
chromium-browser \
  --headless \
  --no-sandbox \
  --disable-gpu \
  --disable-dev-shm-usage \
  --remote-debugging-port=9222 \
  --remote-debugging-address=0.0.0.0 &
```

**How it's used:**
```bash
bhatti create --name scraper --image browser
# Chromium starts automatically on port 9222

# AI agent connects via CDP through bhatti's tunnel:
bhatti exec scraper -- python3 -c "
from playwright.sync_api import sync_playwright
with sync_playwright() as p:
    browser = p.chromium.connect_over_cdp('http://localhost:9222')
    page = browser.new_page()
    page.goto('https://example.com')
    print(page.title())
"
```

**The snapshot story:** Navigate to a page, log in, get to a specific
state — `bhatti snapshot create`. Resume that snapshot 100 times, each
starting from the logged-in state with Chromium's full process memory
restored. No re-login, no cookie management, no re-navigation.

**Use case:** AI web agents, scraping, browser testing, screenshot
capture. No new bhatti API needed — CDP over port tunnel works today.

### 2.3 Docker (~800MB uncompressed)

Docker Engine running inside the VM.

**Contents (everything in minimal, plus):**
- `docker-ce`, `containerd`, `runc` (from Docker's apt repo)
- `iptables` (legacy mode configured via `update-alternatives`)
- Boot profile: `/etc/bhatti/init.sh` mounts cgroups and starts dockerd

**Boot profile (`/etc/bhatti/init.sh`):**
```bash
#!/bin/sh
# Mount cgroups v2 (Docker needs this)
mkdir -p /sys/fs/cgroup
mount -t cgroup2 cgroup2 /sys/fs/cgroup 2>/dev/null || true

# Switch to iptables-legacy (kernel has legacy iptables, not nftables)
update-alternatives --set iptables /usr/sbin/iptables-legacy 2>/dev/null
update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy 2>/dev/null

# Start Docker daemon
dockerd > /var/log/dockerd.log 2>&1 &

# Wait for socket
while [ ! -S /var/run/docker.sock ]; do sleep 0.1; done

# Allow lohar user (uid 1000) to use Docker without sudo
chmod 666 /var/run/docker.sock
```

**How it's used:**
```bash
bhatti create --name ci --image docker --memory 2048
bhatti exec ci -- docker run --rm postgres:16 postgres --version
bhatti exec ci -- docker compose up -d
```

**The snapshot story:** Boot a sandbox, `docker compose up` your full
stack (Postgres + Redis + your app), wait for everything to be healthy,
then `bhatti snapshot create`. Resume later — all containers are running,
databases have data, app is connected. No cold start, no seeding.

**Kernel requirement:** Custom kernel with `CONFIG_IP_NF_RAW=y`. Without
it, Docker's bridge networking refuses to create containers. Verified on
agni-01: with the stock v1.15 kernel, `docker run hello-world` fails
with `can't initialize iptables table 'raw'`. With `IP_NF_RAW=y`, bridge
networking works.

**Use case:** Docker-based CI pipelines, testcontainers, running
databases, any workload that needs Docker's ecosystem.

---

## Part 3 — Boot Profiles

### 3.1 Mechanism

A boot profile is a script at `/etc/bhatti/init.sh` inside the rootfs.
Lohar runs it after system setup (mounts, networking, config drive) but
before the user's `--init` script.

**Execution order:**
```
1. lohar PID 1 init (mount /proc, /sys, /dev, etc.)
2. loadConfigDrive() — env vars, files, volumes
3. setupNetworking()
4. /etc/bhatti/init.sh (boot profile — if it exists)
5. user's --init script (from create request)
```

The boot profile starts tier-specific services (Chromium, dockerd). The
user's init runs in an environment where those services are already up.

### 3.2 lohar changes

**File:** `cmd/lohar/main.go`

After `installSignalHandlers()`, before the user init session:

```go
// Run boot profile if present
if _, err := os.Stat("/etc/bhatti/init.sh"); err == nil {
    cmd := exec.Command("/bin/sh", "/etc/bhatti/init.sh")
    cmd.Stdout = os.Stderr // boot profile logs go to lohar's stderr
    cmd.Stderr = os.Stderr
    cmd.Env = buildEnv(nil)
    if err := cmd.Run(); err != nil {
        fmt.Fprintf(os.Stderr, "lohar: boot profile failed: %v\n", err)
        // Non-fatal — sandbox is still usable, just without tier services
    }
}
```

The boot profile runs as root (PID 1 context). It needs root to start
dockerd, mount cgroups, etc. The user's `--init` runs as uid 1000 (via
the existing `runInitSession` which applies `Credential{Uid: 1000}`).

### 3.3 Shell selection fix

**Problem:** `engine.go` hardcodes `/bin/zsh` for shell sessions:

```go
return ag.Shell(ctx, []string{"/bin/zsh", "-li"}, ...)
```

The minimal tier doesn't have zsh. `bhatti shell` would fail.

**Fix:** Fall back through available shells.

**File:** `pkg/engine/firecracker/engine.go`

```go
func (e *Engine) Shell(ctx context.Context, id string) (engine.TerminalConn, error) {
    // ... existing lock/thermal check ...

    // Detect available shell — try zsh, bash, sh in order
    shell := "/bin/sh"
    for _, candidate := range []string{"/bin/zsh", "/bin/bash"} {
        result, err := ag.Exec(ctx, []string{"test", "-x", candidate}, nil, "")
        if err == nil && result.ExitCode == 0 {
            shell = candidate
            break
        }
    }

    args := []string{shell}
    if shell != "/bin/sh" {
        args = append(args, "-li") // login + interactive
    }

    return ag.Shell(ctx, args, map[string]string{
        "TERM": "xterm-256color",
    }, 24, 80)
}
```

This adds two round-trips on first shell connection (~2ms total). The
result could be cached per-VM but the cost is negligible.

### 3.4 Tests

- `TestBootProfileRuns` — create sandbox from image with `/etc/bhatti/init.sh`
  that writes a marker file, exec `cat /tmp/boot-profile-ran` → exists
- `TestBootProfileBeforeUserInit` — boot profile writes timestamp to
  `/tmp/profile-ts`, user init writes to `/tmp/init-ts`, verify profile
  timestamp is earlier
- `TestBootProfileFailureNonFatal` — boot profile that exits 1, sandbox
  still boots, exec works
- `TestBootProfileRunsAsRoot` — boot profile runs `whoami > /tmp/who`,
  verify contents is `root`
- `TestShellFallbackToSh` — boot from minimal (no zsh), `bhatti shell` works
- `TestShellPrefersZsh` — boot from image with zsh installed, shell is zsh
- `TestShellPrefersBashOverSh` — image with bash but no zsh, shell is bash

---

## Part 4 — Tier Build Scripts

### 4.1 Structure

```
scripts/
  build-kernel.sh           # Part 1.5
  tiers/
    minimal.sh              # tier script — runs inside chroot
    browser.sh              # sources minimal.sh, adds Chromium
    docker.sh               # sources minimal.sh, adds Docker
  build-tier.sh             # orchestrator: create ext4, debootstrap, run tier script
```

### 4.2 Orchestrator

**File:** `scripts/build-tier.sh`

```bash
#!/bin/bash
# Build a rootfs tier image.
# Usage: sudo ./scripts/build-tier.sh <tier> <arch> <lohar-binary>
#   tier: minimal, browser, docker
#   arch: amd64, arm64
#
# Output: dist/rootfs-<tier>-<arch>.ext4
# Environment:
#   SIZE_MB — image size (default: auto per tier)
set -euo pipefail

TIER="${1:?usage: build-tier.sh <tier> <arch> <lohar-binary>}"
ARCH="${2:?}"
AGENT="${3:?}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Tier-specific defaults
case "$TIER" in
    minimal) SIZE_MB="${SIZE_MB:-512}" ;;
    browser) SIZE_MB="${SIZE_MB:-2048}" ;;
    docker)  SIZE_MB="${SIZE_MB:-2048}" ;;
    *) echo "unknown tier: $TIER" >&2; exit 1 ;;
esac

case "$ARCH" in
    amd64) DEB_ARCH="amd64"; MIRROR="http://archive.ubuntu.com/ubuntu" ;;
    arm64) DEB_ARCH="arm64"; MIRROR="http://ports.ubuntu.com/ubuntu-ports" ;;
    *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

IMG="dist/rootfs-${TIER}-${ARCH}.ext4"
MOUNT="/mnt/bhatti-${TIER}-$$"

mkdir -p dist
trap 'umount "$MOUNT/dev/pts" "$MOUNT/dev" "$MOUNT/sys" "$MOUNT/proc" "$MOUNT" 2>/dev/null; rmdir "$MOUNT" 2>/dev/null' EXIT

# Create ext4 image
dd if=/dev/zero of="$IMG" bs=1M count="$SIZE_MB" status=progress
mkfs.ext4 -F -q "$IMG"
mkdir -p "$MOUNT"
mount "$IMG" "$MOUNT"

# Bootstrap minimal Ubuntu
debootstrap --variant=minbase --arch="$DEB_ARCH" noble "$MOUNT" "$MIRROR"

# Set up chroot
cp /etc/resolv.conf "$MOUNT/etc/resolv.conf"
mount --bind /proc    "$MOUNT/proc"
mount --bind /sys     "$MOUNT/sys"
mount --bind /dev     "$MOUNT/dev"
mount --bind /dev/pts "$MOUNT/dev/pts"

# Run tier script
export MOUNT ARCH DEB_ARCH AGENT SCRIPT_DIR
"$SCRIPT_DIR/tiers/${TIER}.sh"

echo "==> Built: $IMG ($(du -h "$IMG" | cut -f1))"
```

### 4.3 Minimal tier script

**File:** `scripts/tiers/minimal.sh`

```bash
#!/bin/bash
# Minimal tier: bare Ubuntu + lohar dependencies.
# Called by build-tier.sh with $MOUNT, $ARCH, $AGENT set.
set -euo pipefail

chroot "$MOUNT" /bin/bash -c '
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends \
    iproute2 ca-certificates sudo curl locales

# Locale
sed -i "/en_US.UTF-8/s/^# //g" /etc/locale.gen
locale-gen

# Create lohar user
useradd -m -s /bin/bash -G sudo lohar
echo "lohar ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

apt-get clean
rm -rf /var/lib/apt/lists/*
'

# Workspace
mkdir -p "$MOUNT/workspace"
chown 1000:1000 "$MOUNT/workspace"

# DNS
cat > "$MOUNT/etc/resolv.conf" << 'EOF'
nameserver 1.1.1.1
nameserver 8.8.8.8
EOF

# Install lohar
cp "$AGENT" "$MOUNT/usr/local/bin/lohar"
chmod 755 "$MOUNT/usr/local/bin/lohar"
```

### 4.4 Browser tier script

**File:** `scripts/tiers/browser.sh`

```bash
#!/bin/bash
# Browser tier: minimal + headless Chromium + Playwright.
# Sources minimal.sh first.
set -euo pipefail

# Build minimal base first
"$SCRIPT_DIR/tiers/minimal.sh"

chroot "$MOUNT" /bin/bash -c '
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq

# Chromium + dependencies
apt-get install -y --no-install-recommends \
    chromium-browser \
    fonts-liberation fonts-noto-color-emoji \
    libnss3 libatk-bridge2.0-0 libcups2 libxdamage1 \
    libxrandr2 libgbm1 libpango-1.0-0 libasound2t64

# Node.js (for Playwright)
NODE_VERSION=22.16.0
case $(dpkg --print-architecture) in
    amd64) NODE_ARCH=x64 ;;
    arm64) NODE_ARCH=arm64 ;;
esac
curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
    | tar -xJ --strip-components=1 -C /usr/local

# Python 3 + Playwright
apt-get install -y --no-install-recommends python3 python3-pip
pip3 install --break-system-packages playwright
npx playwright install-deps 2>/dev/null || true

apt-get clean
rm -rf /var/lib/apt/lists/* /root/.cache /tmp/*
'

# Boot profile: start Chromium with CDP
mkdir -p "$MOUNT/etc/bhatti"
cat > "$MOUNT/etc/bhatti/init.sh" << 'PROFILE'
#!/bin/sh
chromium-browser \
    --headless \
    --no-sandbox \
    --disable-gpu \
    --disable-dev-shm-usage \
    --remote-debugging-port=9222 \
    --remote-debugging-address=0.0.0.0 &
PROFILE
chmod 755 "$MOUNT/etc/bhatti/init.sh"
```

### 4.5 Docker tier script

**File:** `scripts/tiers/docker.sh`

```bash
#!/bin/bash
# Docker tier: minimal + Docker Engine.
# Sources minimal.sh first.
set -euo pipefail

# Build minimal base first
"$SCRIPT_DIR/tiers/minimal.sh"

chroot "$MOUNT" /bin/bash -c '
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq

# Docker prerequisites
apt-get install -y --no-install-recommends \
    ca-certificates curl gnupg iptables

# Docker repo
curl -fsSL https://download.docker.com/linux/ubuntu/gpg \
    | gpg --dearmor -o /usr/share/keyrings/docker-archive-keyring.gpg
echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/docker-archive-keyring.gpg] \
    https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo $VERSION_CODENAME) stable" \
    > /etc/apt/sources.list.d/docker.list

apt-get update -qq
apt-get install -y --no-install-recommends docker-ce docker-ce-cli containerd.io

# Configure iptables-legacy (kernel has legacy iptables, not nftables)
update-alternatives --set iptables /usr/sbin/iptables-legacy
update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy

# Add lohar user to docker group
usermod -aG docker lohar

apt-get clean
rm -rf /var/lib/apt/lists/*
'

# Boot profile: mount cgroups + start dockerd
mkdir -p "$MOUNT/etc/bhatti"
cat > "$MOUNT/etc/bhatti/init.sh" << 'PROFILE'
#!/bin/sh
# Mount cgroups v2
mkdir -p /sys/fs/cgroup
mount -t cgroup2 cgroup2 /sys/fs/cgroup 2>/dev/null || true

# Start Docker daemon
dockerd > /var/log/dockerd.log 2>&1 &

# Wait for socket
while [ ! -S /var/run/docker.sock ]; do sleep 0.1; done
PROFILE
chmod 755 "$MOUNT/etc/bhatti/init.sh"
```

### 4.6 Tests

**Tier build tests (CI, ubuntu-latest):**
- `TestMinimalTierBoots` — build minimal, boot VM, exec `whoami` → `lohar`
- `TestMinimalTierHasIproute` — exec `ip addr` → works
- `TestMinimalTierHasCurl` — exec `curl -V` → works
- `TestMinimalTierHasSudo` — exec `sudo whoami` → `root`
- `TestMinimalTierNoZsh` — exec `which zsh` → not found
- `TestMinimalTierSize` — rootfs < 200MB uncompressed

**Browser tier tests (agni-01):**
- `TestBrowserTierChromiumStarts` — boot, wait 5s, exec
  `curl -s http://localhost:9222/json/version` → returns Chromium version
- `TestBrowserTierPlaywright` — boot, exec playwright script that
  navigates to example.com and returns title
- `TestBrowserTierSnapshotResume` — navigate to a page, snapshot, resume,
  verify Chromium is still running with the page loaded (CDP connection alive)

**Docker tier tests (agni-01):**
- `TestDockerTierDaemonStarts` — boot, exec `docker version` → succeeds
- `TestDockerTierHelloWorld` — exec `docker run --rm hello-world` → works
  with bridge networking (not `--network host`)
- `TestDockerTierBuildAndRun` — write a Dockerfile, `docker build`, `docker run`
- `TestDockerTierComposeUp` — write docker-compose.yml, `docker compose up -d`,
  verify services running
- `TestDockerTierSnapshotResume` — start containers, snapshot, resume,
  verify containers still running

---

## Part 5 — Distribution

### 5.1 Artifacts per release

```
dist/
  # Binaries (existing)
  bhatti-darwin-arm64
  bhatti-darwin-amd64
  bhatti-linux-amd64
  bhatti-linux-arm64

  # Kernel (new)
  vmlinux-6.1.155-x86_64
  vmlinux-6.1.155-aarch64

  # Rootfs images (new, zstd-compressed)
  rootfs-minimal-amd64.ext4.zst
  rootfs-minimal-arm64.ext4.zst
  rootfs-browser-amd64.ext4.zst
  rootfs-browser-arm64.ext4.zst
  rootfs-docker-amd64.ext4.zst
  rootfs-docker-arm64.ext4.zst

  # Firecracker binary (new)
  firecracker-v1.14.0-x86_64
  firecracker-v1.14.0-aarch64
```

Total: 4 binaries + 2 kernels + 6 rootfs + 2 firecracker = 14 artifacts.
GitHub Releases limit is 2GB per file. Largest artifact is the docker
rootfs at ~800MB uncompressed, ~300MB with zstd. Well within budget.

### 5.2 Install script changes

**File:** `scripts/install.sh`

The install script changes from "build everything" to "download pre-built":

```bash
# Old (10+ min):
debootstrap ... && chroot ... && apt-get install ...

# New (30 seconds):
curl -fsSL "$RELEASE_URL/vmlinux-6.1.155-${ARCH}" -o "$KERNEL_PATH"
curl -fsSL "$RELEASE_URL/rootfs-${TIER}-${ARCH}.ext4.zst" | zstd -d > "$ROOTFS_PATH"
curl -fsSL "$RELEASE_URL/firecracker-v1.14.0-${ARCH}" -o /usr/local/bin/firecracker
```

**Tier selection:**
```bash
sudo ./scripts/install.sh --systemd                    # minimal (default)
sudo ./scripts/install.sh --systemd --image browser    # browser tier
sudo ./scripts/install.sh --systemd --image docker     # docker tier
```

**Fallback:** `build-tier.sh` remains for users who want to build from
source or customize tier scripts. The install script checks if rootfs
already exists before downloading.

### 5.3 CI workflow

**File:** `.github/workflows/build-images.yml`

```yaml
name: Build Images

on:
  workflow_dispatch:
  push:
    tags: ['v*']
    paths:
      - 'scripts/tiers/**'
      - 'scripts/build-tier.sh'
      - 'scripts/build-kernel.sh'

jobs:
  kernel:
    strategy:
      matrix:
        arch: [x86_64, aarch64]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install build deps
        run: |
          sudo apt-get update
          sudo apt-get install -y build-essential flex bison libelf-dev libssl-dev bc
          ${{ matrix.arch == 'aarch64' && 'sudo apt-get install -y gcc-aarch64-linux-gnu' || '' }}
      - name: Build kernel
        run: ./scripts/build-kernel.sh ${{ matrix.arch }}
      - uses: actions/upload-artifact@v4
        with:
          name: kernel-${{ matrix.arch }}
          path: dist/vmlinux-*

  rootfs:
    strategy:
      matrix:
        tier: [minimal, browser, docker]
        arch: [amd64, arm64]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-go@v5
        with: { go-version-file: go.mod }
      - name: Build lohar
        run: |
          GOARCH=${{ matrix.arch }} GOOS=linux CGO_ENABLED=0 \
            go build -ldflags="-s -w" -o lohar ./cmd/lohar/
      - name: Install debootstrap
        run: sudo apt-get install -y debootstrap qemu-user-static
      - name: Build rootfs
        run: sudo ./scripts/build-tier.sh ${{ matrix.tier }} ${{ matrix.arch }} ./lohar
      - name: Compress
        run: zstd -19 dist/rootfs-${{ matrix.tier }}-${{ matrix.arch }}.ext4
      - uses: actions/upload-artifact@v4
        with:
          name: rootfs-${{ matrix.tier }}-${{ matrix.arch }}
          path: dist/rootfs-*.ext4.zst

  release:
    needs: [kernel, rootfs]
    if: startsWith(github.ref, 'refs/tags/')
    runs-on: ubuntu-latest
    steps:
      - uses: actions/download-artifact@v4
      - uses: softprops/action-gh-release@v2
        with:
          files: |
            kernel-*/*
            rootfs-*/*
```

For arm64 rootfs: `qemu-user-static` + binfmt_misc enables running arm64
debootstrap on x86_64 runners. Slow (~20-30 min) but only runs on release.

---

## Part 6 — Implementation Phases

### Phase 1: Kernel + lohar changes

No rootfs rebuilds. No CI changes. Just get the custom kernel working
and lohar supporting boot profiles + shell fallback.

1. Write `scripts/build-kernel.sh`
2. Build kernel on agni-01 (3-5 min)
3. Add boot profile support to lohar (`/etc/bhatti/init.sh`)
4. Add cgroups mount to lohar (always, harmless for non-docker)
5. Fix hardcoded `/bin/zsh` in `engine.go`
6. Deploy kernel + updated lohar + updated bhatti to agni-01
7. Verify: boot existing rootfs with new kernel, Docker hello-world works

### Phase 2: Tier scripts + manual build

Write the tier scripts, build locally on agni-01, test each tier.

1. Write `scripts/build-tier.sh` orchestrator
2. Write `scripts/tiers/minimal.sh`
3. Build minimal, boot, test
4. Write `scripts/tiers/browser.sh`
5. Build browser, boot, test Chromium + CDP + Playwright
6. Write `scripts/tiers/docker.sh`
7. Build docker, boot, test `docker run --rm hello-world` with bridge networking
8. Test snapshot/resume for each tier

### Phase 3: CI + distribution

Add CI workflows, update install script, cut release.

1. Add `.github/workflows/build-images.yml`
2. Update `scripts/install.sh` to download pre-built artifacts
3. Update `.github/workflows/release.yml` to include kernel + rootfs
4. Test clean install from scratch on a fresh machine
5. Tag v0.4.0

### Dependency graph

```
Phase 1 (kernel + lohar)
     ↓
Phase 2 (tier scripts)    — needs Phase 1 kernel for docker tier
     ↓
Phase 3 (CI + distribution) — needs Phase 2 tier scripts
```

---

## Open Questions

1. **Rootfs default size.** Minimal at 512MB, browser/docker at 2GB.
   Users can resize with `--disk-size`. Should minimal be smaller
   (256MB) to minimize download?

2. **Chromium version pinning.** Ubuntu repos ship whatever version is
   current. Should we pin a specific Chromium version for reproducibility,
   or accept rolling updates per rootfs build?

3. **Docker version pinning.** Same question. Docker 29.3.1 is current.
   Pin to a specific version in the tier script, or use `docker-ce`
   (latest)?

4. **Playwright version.** Pin `playwright==X.Y.Z` or use latest?
   Playwright + Chromium version coupling is notoriously fragile.

5. **arm64 rootfs build time.** QEMU userspace emulation for arm64
   debootstrap on x86 CI runners takes 20-30 min. Acceptable for
   release builds? Alternative: self-hosted arm64 runner.
