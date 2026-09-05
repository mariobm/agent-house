# Bhatti — ARM64 Host Setup

Setting up an ARM64 Linux machine (Raspberry Pi 5, AWS Graviton, Ampere, etc.)
to run Firecracker microVM sandboxes.

**Tested on:**
- Raspberry Pi 5 (8GB) running Ubuntu 24.04, kernel 6.8, NVMe storage
- Should work on any arm64 Linux with KVM support

**Time:** ~20 minutes (mostly waiting for debootstrap + npm install)

---

## 1. Prerequisites

```bash
  Verify KVM support (required for Firecracker)
ls -la /dev/kvm
# If missing:
sudo modprobe kvm
echo "kvm" | sudo tee /etc/modules-load.d/kvm.conf

# Verify architecture
uname -m   # must be aarch64

# Ensure your user can access KVM
sudo usermod -aG kvm $(whoami)
# Log out and back in, then verify:
#   id | grep kvm
```

## 2. Install Firecracker

```bash
VERSION=1.15.0
ARCH=aarch64

curl -fsSL \
  "https://github.com/firecracker-microvm/firecracker/releases/download/v${VERSION}/firecracker-v${VERSION}-${ARCH}.tgz" \
  | tar xz

sudo mv release-v${VERSION}-${ARCH}/firecracker-v${VERSION}-${ARCH} /usr/local/bin/firecracker
sudo mv release-v${VERSION}-${ARCH}/jailer-v${VERSION}-${ARCH} /usr/local/bin/jailer
sudo chmod +x /usr/local/bin/firecracker /usr/local/bin/jailer
rm -rf release-v${VERSION}-${ARCH}

# Verify
firecracker --version
```

## 3. System Configuration

```bash
# Enable IP forwarding (so VMs can reach the internet via NAT)
echo 'net.ipv4.ip_forward = 1' | sudo tee /etc/sysctl.d/99-bhatti.conf
sudo sysctl -p /etc/sysctl.d/99-bhatti.conf

# Create bhatti data directories
sudo mkdir -p /var/lib/bhatti/{images,sandboxes}
sudo chown $(whoami):$(whoami) /var/lib/bhatti -R
```

## 4. Download Kernel

Pre-built aarch64 Linux 6.1 kernel from Firecracker CI artifacts.
Includes virtio-blk, virtio-net, virtio-vsock, ext4 — everything Firecracker needs.

**Important:** Do NOT use the old quickstart kernel (4.14) — it crashes on
Pi 5 (Cortex-A76) and is missing features needed by Ubuntu 24.04.

```bash
curl -fsSL \
  'https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.6.0/aarch64/vmlinux-6.1.58' \
  -o /var/lib/bhatti/images/vmlinux-arm64

# Verify
file /var/lib/bhatti/images/vmlinux-arm64
# → Linux kernel ARM64 boot executable Image, little-endian, 4K pages
ls -lh /var/lib/bhatti/images/vmlinux-arm64
# → ~31MB
```

## 5. Build Agent Binary

On your Mac (or any machine with Go):

```bash
cd /path/to/forge
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build \
    -ldflags='-s -w' \
    -o bin/lohar-linux-arm64 \
    ./cmd/lohar

# Copy to Pi
scp bin/lohar-linux-arm64 user@<PI_IP>:/var/lib/bhatti/lohar
```

## 6. Build Base Rootfs

This creates a 2GB ext4 disk image with Ubuntu 24.04 + dev tools + the guest
agent baked in. Run **on the Pi** (needs root for mount/chroot).

```bash
# Install debootstrap if not present
sudo apt-get update && sudo apt-get install -y debootstrap

# Copy the build script and sandbox configs to the Pi (from your Mac)
#   scp scripts/build-rootfs.sh user@<PI_IP>:/var/lib/bhatti/
#   scp -r sandbox/ user@<PI_IP>:/var/lib/bhatti/sandbox/

# Run the build
sudo /var/lib/bhatti/build-rootfs.sh /var/lib/bhatti/lohar
```

The script creates `/var/lib/bhatti/images/rootfs-base-arm64.ext4` containing:
- Ubuntu 24.04 minimal (debootstrap noble)
- zsh, git, curl, wget, tmux, vim, htop, jq, socat, iproute2
- Starship prompt
- Node.js 22.x + Claude Code CLI
- tmux plugins (dracula theme, sensible, cpu)
- zsh plugins (zinit, syntax-highlighting, autosuggestions, zsh-z)
- User `lohar` with sudo, zsh shell
- `/workspace` directory for project files
- `lohar` at `/usr/local/bin/lohar` (VM init process)
- Static DNS (1.1.1.1, 8.8.8.8)

## 7. Smoke Test — Boot a VM

Verify everything works before writing any Go engine code.

```bash
# Must run as root (or with KVM + CAP_NET_ADMIN access)
sudo bash

cd /var/lib/bhatti

# Create a copy of the rootfs for this test VM
cp images/rootfs-base-arm64.ext4 /tmp/test-rootfs.ext4

# Clean up any previous test sockets
rm -f /tmp/fc-test.sock /tmp/fc-test-vsock.sock

# Start Firecracker
firecracker --api-sock /tmp/fc-test.sock &
FC_PID=$!
sleep 0.5

# Configure the VM via Firecracker's HTTP API
curl --unix-socket /tmp/fc-test.sock -s -X PUT \
  http://localhost/boot-source \
  -d '{
    "kernel_image_path": "/var/lib/bhatti/images/vmlinux-arm64",
    "boot_args": "console=ttyS0 reboot=k panic=1 pci=off init=/usr/local/bin/lohar quiet loglevel=0"
  }'

curl --unix-socket /tmp/fc-test.sock -s -X PUT \
  http://localhost/drives/rootfs \
  -d '{
    "drive_id": "rootfs",
    "path_on_host": "/tmp/test-rootfs.ext4",
    "is_root_device": true,
    "is_read_only": false
  }'

curl --unix-socket /tmp/fc-test.sock -s -X PUT \
  http://localhost/machine-config \
  -d '{"vcpu_count": 1, "mem_size_mib": 512}'

curl --unix-socket /tmp/fc-test.sock -s -X PUT \
  http://localhost/vsock \
  -d '{"guest_cid": 3, "uds_path": "/tmp/fc-test-vsock.sock"}'

# Boot it
curl --unix-socket /tmp/fc-test.sock -s -X PUT \
  http://localhost/actions \
  -d '{"action_type": "InstanceStart"}'

echo "Waiting for VM to boot..."
sleep 3

# Test vsock connectivity — send CONNECT to agent's control port
echo "CONNECT 1024" | socat - UNIX-CONNECT:/tmp/fc-test-vsock.sock
# Expected output: "OK <number>"
# This proves: kernel booted → agent started → vsock is working

# Clean up
kill $FC_PID 2>/dev/null
rm -f /tmp/fc-test.sock /tmp/fc-test-vsock.sock /tmp/test-rootfs.ext4
```

If the socat command prints `OK <number>`, the full stack works:
kernel → rootfs → agent (PID 1) → vsock listener → ready for commands.

## 8. Troubleshooting

**"Cannot open /dev/kvm"** — Load the kvm module: `sudo modprobe kvm`.
On some kernels you may need `kvm` in `/etc/modules-load.d/`.

**VM doesn't boot / kernel panic** — Check Firecracker's stderr output.
Common causes:
- Wrong kernel architecture (needs aarch64 vmlinux)
- Rootfs doesn't have the agent at `/usr/local/bin/lohar`
- Rootfs image is corrupt (re-run build-rootfs.sh)

**"CONNECT 1024" hangs** — The agent hasn't started yet. Increase the
sleep time, or check if the kernel booted at all (remove `quiet` from
boot_args to see console output).

**Agent starts but exec fails** — The rootfs might be missing basic
utilities. Shell into the rootfs via `mount + chroot` and verify
`/bin/sh`, `/usr/bin/echo` etc. exist.

---

## Directory Layout

After setup, the Pi looks like:

```
/usr/local/bin/
  firecracker          — VMM binary
  jailer               — (optional) sandboxing for firecracker itself

/var/lib/bhatti/
  lohar          — guest agent binary (copied into rootfs)
  images/
    vmlinux-arm64       — Linux kernel (~8MB)
    rootfs-base-arm64.ext4  — base rootfs (~2GB)
  sandboxes/
    <sandbox-id>/
      rootfs.ext4       — copy of base rootfs (per-sandbox)
      vsock.sock        — Firecracker vsock UDS
      firecracker.sock  — Firecracker API UDS
      mem.snap          — memory snapshot (when stopped)
      vm.snap           — VM state snapshot (when stopped)
```
