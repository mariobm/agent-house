#!/usr/bin/env bash
# Build an ext4 KVM test image with Rust forge as PID 1 and static BusyBox.
# Run on Linux. Requires cargo + native musl target, C toolchain, curl,
# bzip2, make, readelf, and mkfs.ext4. No mount or root privileges needed.
# Usage: scripts/rust-guest-rootfs.sh [OUT_IMG]
# FORGE_BIN may supply a prebuilt native static forge; IMG_MB defaults to 256.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$PWD"
OUT="${1:-/tmp/kvm/rust-guest.ext4}"
[[ "$(uname -s)" == Linux ]] || { echo 'Run this script on Linux' >&2; exit 1; }
case "$(uname -m)" in
    x86_64) TARGET=x86_64-unknown-linux-musl ;;
    aarch64) TARGET=aarch64-unknown-linux-musl ;;
    *) echo 'Unsupported guest architecture' >&2; exit 1 ;;
esac

BUSYBOX_VERSION=1.37.0
# https://github.com/docker-library/busybox/blob/master/latest-1/musl/Dockerfile.builder
BUSYBOX_SHA256=3311dff32e746499f4df0d5df04d7eb396382d7e108bb9250e7b519b837043a4
WORK="$(mktemp -d)"
IMAGE_TMP=''
cleanup() { rm -rf "$WORK"; [[ -z "$IMAGE_TMP" ]] || rm -f "$IMAGE_TMP"; }
trap cleanup EXIT
ROOT="$WORK/rootfs"
mkdir -p "$ROOT"/{bin,proc,sys,dev/pts,tmp,run,etc,root,workspace,usr/local/bin}
chmod 1777 "$ROOT/tmp"
if [[ -n "${FORGE_BIN:-}" ]]; then
    cp "$FORGE_BIN" "$ROOT/init.krun"
else
    export PATH="$HOME/.cargo/bin:$PATH"
    cargo build --manifest-path "$REPO/rust/Cargo.toml" --locked --release -p ahvm-forge --target "$TARGET"
    cp "$REPO/rust/target/$TARGET/release/ahvm-forge" "$ROOT/init.krun"
fi

archive="$WORK/busybox.tar.bz2"
curl -fL --connect-timeout 10 --max-time 90 --retry 2 \
    "https://busybox.net/downloads/busybox-$BUSYBOX_VERSION.tar.bz2" -o "$archive" || \
    curl -fL --connect-timeout 10 --max-time 90 --retry 2 \
        "https://deb.debian.org/debian/pool/main/b/busybox/busybox_${BUSYBOX_VERSION}.orig.tar.bz2" -o "$archive"
echo "$BUSYBOX_SHA256  $archive" | sha256sum -c -
tar -xf "$archive" -C "$WORK"
(
    cd "$WORK/busybox-$BUSYBOX_VERSION"
    make defconfig >/dev/null
    sed -i -e 's/# CONFIG_STATIC is not set/CONFIG_STATIC=y/' \
        -e 's/CONFIG_TC=y/# CONFIG_TC is not set/' .config
    make oldconfig </dev/null >/dev/null
    make -j"${BUILD_JOBS:-$(nproc)}" busybox > "$WORK/busybox-build.log" 2>&1 || {
        cat "$WORK/busybox-build.log" >&2; exit 1;
    }
)
cp "$WORK/busybox-$BUSYBOX_VERSION/busybox" "$ROOT/bin/busybox"
for binary in "$ROOT/init.krun" "$ROOT/bin/busybox"; do
    headers="$(readelf -l "$binary")"
    if [[ "$headers" == *INTERP* ]]; then
        echo "$binary requires a dynamic loader; supply a static binary" >&2; exit 1
    fi
done
chmod +x "$ROOT/init.krun" "$ROOT/bin/busybox"
"$ROOT/bin/busybox" --install -s "$ROOT/bin"
# --install uses its absolute build path; guest links must resolve in /bin.
for link in "$ROOT/bin/"*; do
    [[ ! -L "$link" ]] || ln -sf busybox "$link"
done
# Forge remains PID 1 after exec, but needs the guest pseudo-filesystems for
# PTYs and /proc-backed tooling. These mounts happen inside the guest only.
mv "$ROOT/init.krun" "$ROOT/usr/local/bin/ahvm-forge"
cat > "$ROOT/init.krun" <<'INIT'
#!/bin/sh
set -eu
export PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin
mountpoint -q /proc || mount -t proc proc /proc
mountpoint -q /sys || mount -t sysfs sysfs /sys
mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts
mountpoint -q /dev/pts || mount -t devpts -o newinstance,ptmxmode=0666,mode=0620 devpts /dev/pts
ln -sf pts/ptmx /dev/ptmx
exec /usr/local/bin/ahvm-forge
INIT
chmod +x "$ROOT/init.krun"
printf 'root:x:0:0:root:/root:/bin/sh\n' > "$ROOT/etc/passwd"
printf '127.0.0.1 localhost\n' > "$ROOT/etc/hosts"
printf 'nameserver 1.1.1.1\n' > "$ROOT/etc/resolv.conf"
mkdir -p "$(dirname "$OUT")"
IMAGE_TMP="$(mktemp "${OUT}.XXXXXX")"
truncate -s "${IMG_MB:-256}M" "$IMAGE_TMP"
mkfs.ext4 -q -F -d "$ROOT" "$IMAGE_TMP"
mv -f "$IMAGE_TMP" "$OUT"
IMAGE_TMP=''
echo "Built $OUT ($TARGET, BusyBox $BUSYBOX_VERSION)"
