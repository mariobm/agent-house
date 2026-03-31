#!/bin/bash
# scripts/install.sh — Install bhatti on a Linux host with KVM.
#
# Downloads pre-built binaries and rootfs from GitHub Releases.
# No Go, no debootstrap, no compilation required.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/sahil-shubham/bhatti/main/scripts/install.sh | sudo bash
#   curl -fsSL https://raw.githubusercontent.com/sahil-shubham/bhatti/main/scripts/install.sh | sudo bash -s -- --tier browser
#
# Or from a local clone:
#   sudo ./scripts/install.sh
#   sudo ./scripts/install.sh --systemd --tier docker
#
# Flags:
#   --tier <minimal|browser|docker>   Rootfs tier (default: minimal)
#   --systemd                         Install and start systemd service
#   --version <tag>                   Release version (default: latest)
#   --from-source                     Build from source instead of downloading
#
# Tiers:
#   minimal  — bare Ubuntu + lohar. ~200MB.
#   browser  — minimal + headless Chromium + Playwright. ~600MB.
#   docker   — minimal + Docker Engine. ~550MB.
#
# Supports aarch64 and x86_64.
set -euo pipefail

GITHUB_REPO="sahil-shubham/bhatti"
FC_VERSION="1.14.0"
DATA_DIR="/var/lib/bhatti"
INSTALL_SYSTEMD=false
TIER="minimal"
VERSION="latest"
FROM_SOURCE=false

# Parse flags
while [[ $# -gt 0 ]]; do
    case "$1" in
        --systemd)     INSTALL_SYSTEMD=true; shift ;;
        --tier)        TIER="${2:?--tier requires a value}"; shift 2 ;;
        --version)     VERSION="${2:?--version requires a value}"; shift 2 ;;
        --from-source) FROM_SOURCE=true; shift ;;
        *) shift ;;
    esac
done

# --- Preflight ---

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root" >&2
    exit 1
fi

HOST_ARCH=$(uname -m)
case "$HOST_ARCH" in
    aarch64) FC_ARCH="aarch64"; GO_ARCH="arm64"; DEB_ARCH="arm64" ;;
    x86_64)  FC_ARCH="x86_64";  GO_ARCH="amd64"; DEB_ARCH="amd64" ;;
    *)
        echo "error: unsupported architecture $HOST_ARCH" >&2
        exit 1
        ;;
esac

if [[ ! -e /dev/kvm ]]; then
    modprobe kvm 2>/dev/null || true
    if [[ ! -e /dev/kvm ]]; then
        echo "error: /dev/kvm not available — KVM required" >&2
        exit 1
    fi
fi

for cmd in curl; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "error: $cmd is required but not installed" >&2
        exit 1
    fi
done

case "$TIER" in
    minimal|browser|docker) ;;
    *) echo "error: unknown tier '$TIER' (use minimal, browser, or docker)" >&2; exit 1 ;;
esac

# Resolve version tag
if [[ "$VERSION" == "latest" ]]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | grep '"tag_name"' | sed 's/.*"tag_name": "\(.*\)".*/\1/')
    if [[ -z "$VERSION" ]]; then
        echo "error: could not resolve latest release version" >&2
        exit 1
    fi
fi
RELEASE_URL="https://github.com/${GITHUB_REPO}/releases/download/${VERSION}"

echo "==> Installing bhatti ${VERSION} on $(hostname) ($HOST_ARCH)"
echo "    tier: $TIER"

# --- Directories ---

mkdir -p "$DATA_DIR"/{images,sandboxes,volumes,snapshots}

# --- Firecracker ---

if [[ ! -f /usr/local/bin/firecracker ]]; then
    echo "==> Installing Firecracker ${FC_VERSION}..."
    cd /tmp
    curl -fsSL \
        "https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-${FC_ARCH}.tgz" \
        | tar xz
    mv "release-v${FC_VERSION}-${FC_ARCH}/firecracker-v${FC_VERSION}-${FC_ARCH}" \
        /usr/local/bin/firecracker
    chmod +x /usr/local/bin/firecracker
    rm -rf "release-v${FC_VERSION}-${FC_ARCH}"
    echo "  installed $(firecracker --version 2>&1 | head -1)"
else
    echo "==> Firecracker already installed: $(firecracker --version 2>&1 | head -1)"
fi

# --- bhatti + lohar binaries ---

if [[ "$FROM_SOURCE" == "true" ]]; then
    echo "==> Building from source..."
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
    if [[ ! -f "$REPO_ROOT/go.mod" ]]; then
        echo "error: --from-source requires running from the repo" >&2
        exit 1
    fi
    export PATH="/usr/local/go/bin:$PATH"
    if ! command -v go &>/dev/null; then
        GO_VERSION=$(grep '^go ' "$REPO_ROOT/go.mod" | awk '{print $2}')
        echo "==> Installing Go ${GO_VERSION}..."
        curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-${GO_ARCH}.tar.gz" \
            | tar -C /usr/local -xz
    fi
    cd "$REPO_ROOT"
    GOOS=linux GOARCH=$GO_ARCH go build -ldflags="-s -w" -o /usr/local/bin/bhatti ./cmd/bhatti/
    GOOS=linux GOARCH=$GO_ARCH CGO_ENABLED=0 go build -ldflags="-s -w" -o "$DATA_DIR/lohar" ./cmd/lohar/
else
    echo "==> Downloading bhatti ${VERSION}..."
    curl -fsSL "${RELEASE_URL}/bhatti-linux-${DEB_ARCH}" -o /usr/local/bin/bhatti
    curl -fsSL "${RELEASE_URL}/lohar-linux-${DEB_ARCH}" -o "$DATA_DIR/lohar"
fi
chmod +x /usr/local/bin/bhatti "$DATA_DIR/lohar"
echo "  bhatti: $(ls -lh /usr/local/bin/bhatti | awk '{print $5}')"
echo "  lohar:  $(ls -lh "$DATA_DIR/lohar" | awk '{print $5}')"

# --- Kernel ---

KERNEL_PATH="$DATA_DIR/images/vmlinux-${DEB_ARCH}"
if [[ ! -f "$KERNEL_PATH" ]]; then
    echo "==> Downloading kernel (${FC_ARCH})..."
    curl -fsSL "${RELEASE_URL}/vmlinux-6.1.155-${FC_ARCH}" -o "$KERNEL_PATH"
    echo "  saved to $KERNEL_PATH ($(ls -lh "$KERNEL_PATH" | awk '{print $5}'))"
else
    echo "==> Kernel already present: $KERNEL_PATH"
fi

# --- Rootfs ---

ROOTFS_PATH="$DATA_DIR/images/rootfs-${TIER}-${DEB_ARCH}.ext4"
if [[ ! -f "$ROOTFS_PATH" ]]; then
    if [[ "$FROM_SOURCE" == "true" ]]; then
        echo "==> Building ${TIER} rootfs from source..."
        if ! command -v debootstrap &>/dev/null; then
            apt-get update -qq && apt-get install -y -qq debootstrap
        fi
        IMG="$ROOTFS_PATH" "$REPO_ROOT/scripts/build-tier.sh" "$TIER" "$DEB_ARCH" "$DATA_DIR/lohar"
    else
        if ! command -v zstd &>/dev/null; then
            echo "==> Installing zstd..."
            apt-get update -qq && apt-get install -y -qq zstd
        fi
        echo "==> Downloading ${TIER} rootfs (${DEB_ARCH})..."
        curl -fsSL "${RELEASE_URL}/rootfs-${TIER}-${DEB_ARCH}.ext4.zst" \
            | zstd -d -o "$ROOTFS_PATH"
        echo "  saved to $ROOTFS_PATH ($(ls -lh "$ROOTFS_PATH" | awk '{print $5}'))"
    fi
else
    echo "==> Rootfs already present: $ROOTFS_PATH"
    # Update lohar inside existing rootfs
    echo "    updating lohar agent in rootfs..."
    MNT=$(mktemp -d)
    mount -o loop "$ROOTFS_PATH" "$MNT"
    cp "$DATA_DIR/lohar" "$MNT/usr/local/bin/lohar"
    chmod +x "$MNT/usr/local/bin/lohar"
    umount "$MNT"
    rmdir "$MNT"
    echo "    done"
fi

# --- Config ---

if [[ ! -f "$DATA_DIR/config.yaml" ]]; then
    echo "==> Generating config..."
    cat > "$DATA_DIR/config.yaml" << EOF
engine: firecracker
listen: :8080
data_dir: ${DATA_DIR}
firecracker_bin: /usr/local/bin/firecracker
firecracker_kernel: ${KERNEL_PATH}
firecracker_rootfs: ${ROOTFS_PATH}
EOF
else
    echo "==> Config already present: $DATA_DIR/config.yaml"
fi

# --- Age key (for secret encryption) ---
if [[ ! -f "$DATA_DIR/age.key" ]]; then
    echo "==> Age key will be generated on first secret creation"
fi

# --- Bootstrap admin user ---

echo "==> Creating admin user..."
ADMIN_KEY=$(bhatti user create --name admin --max-sandboxes 50 2>&1 | grep "API key:" | awk '{print $NF}')

if [[ -n "$ADMIN_KEY" ]]; then
    # Write CLI config for the user who ran sudo.
    # Use getent to reliably resolve the home directory (handles NFS,
    # LDAP, non-standard homes). Fallback to eval if getent unavailable.
    if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
        SUDO_USER_HOME=$(getent passwd "$SUDO_USER" 2>/dev/null | cut -d: -f6)
        SUDO_USER_GROUP=$(id -gn "$SUDO_USER" 2>/dev/null || echo "$SUDO_USER")
        if [[ -z "$SUDO_USER_HOME" ]]; then
            SUDO_USER_HOME=$(eval echo "~$SUDO_USER" 2>/dev/null)
        fi

        if [[ -n "$SUDO_USER_HOME" && -d "$SUDO_USER_HOME" ]]; then
            USER_CFG_DIR="$SUDO_USER_HOME/.bhatti"
            mkdir -p "$USER_CFG_DIR"
            cat > "$USER_CFG_DIR/config.yaml" << EOF
auth_token: ${ADMIN_KEY}
listen: :8080
EOF
            chown -R "$SUDO_USER:$SUDO_USER_GROUP" "$USER_CFG_DIR"
        else
            echo "  note: home directory for $SUDO_USER not found, skipping user config"
            echo "  run 'bhatti setup' as $SUDO_USER to configure the CLI"
        fi
    fi

    # Also for root
    mkdir -p /root/.bhatti
    cat > /root/.bhatti/config.yaml << EOF
auth_token: ${ADMIN_KEY}
listen: :8080
EOF
else
    echo "  warning: admin user may already exist, skipping"
fi

# --- Systemd ---

if [[ "$INSTALL_SYSTEMD" == "true" ]]; then
    echo "==> Installing systemd service..."

    # Download service file if not in a repo checkout
    if [[ "$FROM_SOURCE" == "true" && -f "$REPO_ROOT/deploy/bhatti.service" ]]; then
        cp "$REPO_ROOT/deploy/bhatti.service" /etc/systemd/system/bhatti.service
    else
        cat > /etc/systemd/system/bhatti.service << 'UNIT'
[Unit]
Description=Bhatti Sandbox Infrastructure
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/bhatti serve
WorkingDirectory=/var/lib/bhatti
Environment=HOME=/root
Restart=always
RestartSec=5
KillMode=process
TimeoutStopSec=120
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT
    fi

    systemctl daemon-reload
    systemctl enable bhatti
    systemctl restart bhatti

    echo -n "  waiting for daemon..."
    for i in $(seq 1 30); do
        if curl -sf http://localhost:8080/health >/dev/null 2>&1; then
            echo " ready"
            break
        fi
        if [[ $i -eq 30 ]]; then
            echo " TIMEOUT"
            echo "error: daemon did not start. Check: journalctl -u bhatti" >&2
            exit 1
        fi
        sleep 1
        echo -n "."
    done
fi

# --- Summary ---

echo ""
echo "============================================"
echo "  bhatti ${VERSION} installed on $(hostname) ($HOST_ARCH)"
echo "  tier: $TIER"
echo ""
if [[ -n "${ADMIN_KEY:-}" ]]; then
    echo "  Admin API key: ${ADMIN_KEY}"
    echo "  (saved to ~/.bhatti/config.yaml)"
    echo ""
fi
echo "  To start the daemon:"
echo "    cd $DATA_DIR && sudo bhatti serve"
echo ""
echo "  Manage users:"
echo "    sudo bhatti user list"
echo "    sudo bhatti user create --name alice"
echo ""
echo "  Then as a user:"
echo "    bhatti create --name hello"
echo "    bhatti exec hello -- echo 'it works'"
echo "    bhatti shell hello"
echo "    bhatti destroy hello"
echo ""
echo "  ⚠  BACK UP: $DATA_DIR/age.key"
echo "     If lost, all encrypted secrets become unrecoverable."
if [[ "$INSTALL_SYSTEMD" == "true" ]]; then
    echo ""
    echo "  systemd service: active (bhatti.service)"
fi
echo "============================================"
