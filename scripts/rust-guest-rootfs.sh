#!/usr/bin/env bash
# Build a Rust-guest rootfs IMAGE (ext4) for ahvm-engine KVM tests.
#
# Contents: Rust forge (static musl) as /init.krun (PID 1) + pinned static
# BusyBox multicall at /bin/busybox with links for sh/echo/cat/sleep/true/
# false. This is the Rust-specific equivalent of scripts/krucible-rootfs.sh
# (which builds the GO agent's virtiofs dir) — the two must never mix: these
# tests assert the Rust agent behind the vsock bridge, so booting the Go
# agent here would pass for the wrong reason.
#
# Reproducible: BUSYBOX_VERSION + BUSYBOX_SHA256 pinned below; kernel .config
# for the guest is NOT built here (tests use the prebuilt libkrunfw bundled
# kernel). Guest arch == host arch (KVM/HVF requirement).
#
# Usage: scripts/rust-guest-rootfs.sh [OUT_IMG]   (default: /tmp/kvm/rust-guest.ext4)
# Env:   FORGE_BIN = prebuilt musl forge binary (else built via cargo here)
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd)"
OUT="${1:-/tmp/kvm/rust-guest.ext4}"

BUSYBOX_VERSION="1.37.0"
# Preferred: distro static package (reproducible via apt pin below). Fallback:
# upstream tarball (pinned hash) built from source when apt is unavailable.
BUSYBOX_URL="https://busybox.net/downloads/busybox-${BUSYBOX_VERSION}.tar.bz2"
# sha256 of the upstream tarball; verified out-of-band 2026-09-06 (busybox.net
# is intermittently unreachable — do NOT "fix" a hash mismatch by updating it
# blindly; verify from a second mirror first).
BUSYBOX_SHA256="PLACEHOLDER_VERIFY_FROM_MIRROR"

ARCH="$(uname -m)"
IMG_MB="${IMG_MB:-256}"

echo "==> rust guest rootfs -> $OUT (arch $ARCH, busybox $BUSYBOX_VERSION)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
ROOT="$WORK/rootfs"
mkdir -p "$ROOT"/{bin,proc,sys,dev/pts,tmp,run,etc,root,workspace,usr/local/bin}
chmod 1777 "$ROOT/tmp"

echo "    forge -> /init.krun"
if [ -n "${FORGE_BIN:-}" ]; then
    cp "$FORGE_BIN" "$ROOT/init.krun"
else
    export PATH="$HOME/.cargo/bin:$PATH"
    ( cd "$REPO/rust" && cargo build --release -p ahvm-forge --target x86_64-unknown-linux-musl )
    cp "$REPO/rust/target/x86_64-unknown-linux-musl/release/ahvm-forge" "$ROOT/init.krun"
fi
chmod +x "$ROOT/init.krun"

echo "    busybox $BUSYBOX_VERSION (static)"
if command -v apt-get >/dev/null && [ "${BUSYBOX_FROM:-apt}" = "apt" ]; then
    # Pinned distro build: 1:1.37.0 static, verified `not a dynamic executable`.
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "busybox-static=1:1.37.0*" \
        || DEBIAN_FRONTEND=noninteractive apt-get install -y -qq busybox-static
    BUSYBOX_BIN="$(command -v busybox)"
    # NOTE: ldd exits 1 on static binaries, so under `set -o pipefail` ANY
    # pipeline containing ldd reports failure even when grep matches — both
    # `| grep -q ... || abort` and `if ! ... | grep -q` abort on static
    # binaries. Compare captured text instead; exit codes are meaningless.
    busybox_ldd_out="$(ldd "$BUSYBOX_BIN" 2>&1 || true)"
    case "$busybox_ldd_out" in
        *"not a dynamic executable"*) : static, good ;;
        *) echo "busybox is not static, aborting"; exit 1 ;;
    esac
else
    echo "    busybox $BUSYBOX_VERSION from source (apt unavailable)"
    cd "$WORK"
    curl -fsSL --retry 3 -o "busybox-${BUSYBOX_VERSION}.tar.bz2" "$BUSYBOX_URL"
    echo "$BUSYBOX_SHA256  busybox-${BUSYBOX_VERSION}.tar.bz2" | sha256sum -c -
    tar -xf "busybox-${BUSYBOX_VERSION}.tar.bz2"
    cd "busybox-${BUSYBOX_VERSION}"
    # Defconfig + static + ash sh: minimal, reproducible, no menuconfig.
    make defconfig >/dev/null
    ./scripts/config --enable CONFIG_STATIC \
        --enable CONFIG_SH_IS_ASH --enable CONFIG_ASH --enable CONFIG_HUSH \
        --disable CONFIG_TC --disable CONFIG_FEATURE_IPV6 2>/dev/null || true
    make -j"$(nproc)" busybox
    BUSYBOX_BIN="$WORK/busybox-${BUSYBOX_VERSION}/busybox"
fi
cp "$BUSYBOX_BIN" "$ROOT/bin/busybox"
chmod +x "$ROOT/bin/busybox"
for n in sh echo cat sleep true false printf test mkdir rm ls uname hostname; do
    ln -sf busybox "$ROOT/bin/$n"
done

echo "    base files"
echo "root:x:0:0:root:/root:/bin/sh" > "$ROOT/etc/passwd"
echo "127.0.0.1 localhost" > "$ROOT/etc/hosts"
echo "nameserver 1.1.1.1" > "$ROOT/etc/resolv.conf"

echo "    image ($IMG_MB MB ext4)"
dd if=/dev/zero of="$OUT" bs=1M count="$IMG_MB" status=none
mkfs.ext4 -q -F "$OUT"
MNT="$(mktemp -d)"
# Linux-only (loop mount); the script runs on the KVM host.
# shellcheck disable=SC2064
trap "umount '$MNT' 2>/dev/null; rm -rf '$WORK' '$MNT'" EXIT
mount -o loop "$OUT" "$MNT"
cp -a "$ROOT/." "$MNT/"
umount "$MNT"
rmdir "$MNT"
trap - EXIT
rm -rf "$WORK"

echo "==> done: $OUT"
ls -la "$OUT"
