#!/bin/bash
# scripts/install.sh — Install bhatti from source on a Linux host with KVM.
#
# Prerequisites: git, KVM (/dev/kvm)
# The script installs Go and Firecracker if not present.
#
# Usage (from repo root):
#   sudo ./scripts/install.sh
#
# For systemd service (optional):
#   sudo ./scripts/install.sh --systemd
#
# Supports aarch64 and x86_64.
set -euo pipefail

FC_VERSION="1.6.0"
FC_MAJOR_MINOR="1.6"
DATA_DIR="/var/lib/bhatti"
INSTALL_SYSTEMD=false

# Parse flags
for arg in "$@"; do
    case "$arg" in
        --systemd) INSTALL_SYSTEMD=true ;;
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

for cmd in curl mktemp; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "error: $cmd is required but not installed" >&2
        exit 1
    fi
done

# Find repo root (script is at scripts/install.sh)
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

if [[ ! -f "$REPO_ROOT/go.mod" ]]; then
    echo "error: cannot find repo root (expected go.mod at $REPO_ROOT)" >&2
    exit 1
fi

GO_VERSION=$(grep '^go ' "$REPO_ROOT/go.mod" | awk '{print $2}')

echo "==> Installing bhatti on $(hostname) ($HOST_ARCH)"
echo "    repo: $REPO_ROOT"

# --- Directories ---

mkdir -p "$DATA_DIR"/{images,sandboxes}

# --- Go (install if missing) ---

if ! command -v go &>/dev/null; then
    echo "==> Installing Go ${GO_VERSION}..."
    curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-${GO_ARCH}.tar.gz" \
        | tar -C /usr/local -xz
    export PATH="/usr/local/go/bin:$PATH"
    echo "  installed $(go version)"
else
    echo "==> Go already installed: $(go version)"
fi
export PATH="/usr/local/go/bin:$PATH"

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

# --- Build bhatti + lohar from source ---

echo "==> Building bhatti and lohar from source..."
cd "$REPO_ROOT"
GOOS=linux GOARCH=$GO_ARCH go build -ldflags="-s -w" -o /usr/local/bin/bhatti ./cmd/bhatti/
GOOS=linux GOARCH=$GO_ARCH go build -ldflags="-s -w" -o "$DATA_DIR/lohar" ./cmd/lohar/
chmod +x /usr/local/bin/bhatti "$DATA_DIR/lohar"
echo "  bhatti: $(ls -lh /usr/local/bin/bhatti | awk '{print $5}')"
echo "  lohar:  $(ls -lh "$DATA_DIR/lohar" | awk '{print $5}')"

# --- Kernel ---

KERNEL_VERSION="6.1.58"
KERNEL_PATH="$DATA_DIR/images/vmlinux-${GO_ARCH}"
if [[ ! -f "$KERNEL_PATH" ]]; then
    echo "==> Downloading kernel ${KERNEL_VERSION} (Firecracker CI, ${FC_ARCH})..."
    curl -fsSL \
        "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v${FC_MAJOR_MINOR}/${FC_ARCH}/vmlinux-${KERNEL_VERSION}" \
        -o "$KERNEL_PATH"
    echo "  saved to $KERNEL_PATH ($(ls -lh "$KERNEL_PATH" | awk '{print $5}'))"
else
    echo "==> Kernel already present: $KERNEL_PATH"
fi

# --- Rootfs ---

ROOTFS_PATH="$DATA_DIR/images/rootfs-base-${DEB_ARCH}.ext4"
if [[ ! -f "$ROOTFS_PATH" ]]; then
    echo "==> Building rootfs (this takes ~10 minutes on first install)..."
    if ! command -v debootstrap &>/dev/null; then
        apt-get update -qq && apt-get install -y -qq debootstrap
    fi
    SANDBOX_DIR="$REPO_ROOT/sandbox" IMG="$ROOTFS_PATH" \
        "$REPO_ROOT/scripts/build-rootfs.sh" "$DATA_DIR/lohar"
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
# The age key is generated automatically on first secret creation.
# If it already exists, leave it alone.
if [[ ! -f "$DATA_DIR/age.key" ]]; then
    echo "==> Age key will be generated on first secret creation"
fi

# --- Bootstrap admin user ---
# Create the first admin user so the API is usable immediately.
# The API key is shown once during install.

echo "==> Creating admin user..."
ADMIN_KEY=$(bhatti user create --name admin --max-sandboxes 50 2>&1 | grep "API key:" | awk '{print $NF}')

if [[ -n "$ADMIN_KEY" ]]; then
    # Write CLI config for the user who ran sudo
    SUDO_USER_HOME=""
    if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]]; then
        SUDO_USER_HOME=$(eval echo "~$SUDO_USER")
    fi

    if [[ -n "$SUDO_USER_HOME" ]]; then
        USER_CFG_DIR="$SUDO_USER_HOME/.bhatti"
        mkdir -p "$USER_CFG_DIR"
        cat > "$USER_CFG_DIR/config.yaml" << EOF
auth_token: ${ADMIN_KEY}
listen: :8080
EOF
        chown -R "$SUDO_USER:$SUDO_USER" "$USER_CFG_DIR"
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

# --- Systemd (optional) ---

if [[ "$INSTALL_SYSTEMD" == "true" ]]; then
    echo "==> Installing systemd service..."
    cp "$REPO_ROOT/deploy/bhatti.service" /etc/systemd/system/bhatti.service
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
echo "  bhatti installed on $(hostname) ($HOST_ARCH)"
echo ""
if [[ -n "$ADMIN_KEY" ]]; then
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
echo "    sudo bhatti user rotate-key alice"
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
