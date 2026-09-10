#!/usr/bin/env bash
# Build on Linux x86_64 as root. Mounts exist only in a private namespace.
# FORGE_BIN must point to the packaged static ahvm-forge.
set -euo pipefail
umask 022
REPO=$(cd "$(dirname "$0")/.." && pwd)
source "$REPO/images/ubuntu-dev/versions.env"
[[ $(uname -s) == Linux && $(uname -m) == x86_64 && $EUID == 0 ]] || { echo 'Requires root on Linux x86_64' >&2; exit 1; }
OUT=$(realpath -m "${1:?Usage: FORGE_BIN=/path/ahvm-forge ubuntu-dev-rootfs.sh OUTPUT.ext4}")
[[ ! -e $OUT && ! -L $OUT ]] || { echo "Refusing existing output: $OUT" >&2; exit 1; }
FORGE_BIN=$(realpath "${FORGE_BIN:?Supply a static ahvm-forge binary}")
for tool in curl sha256sum tar unshare chroot mount mkfs.ext4 readelf; do command -v "$tool" >/dev/null; done
if readelf -l "$FORGE_BIN" | grep -q INTERP; then echo 'Forge must be statically linked' >&2; exit 1; fi
mkdir -p "$(dirname "$OUT")"
WORK=$(mktemp -d)
IMAGE_TMP=$(mktemp "${OUT}.build.XXXXXX")
# This parent never mounts anything. Namespace teardown precedes cleanup.
cleanup() { rm -rf "$WORK"; rm -f "$IMAGE_TMP"; }
trap cleanup EXIT
fetch() { curl --proto '=https' --proto-redir '=https' -fLsS --retry 3 --connect-timeout 15 --max-time 600 "$1" -o "$2"; }
fetch "https://cdimage.ubuntu.com/ubuntu-base/releases/24.04/release/ubuntu-base-$UBUNTU_VERSION-base-amd64.tar.gz" "$WORK/ubuntu.tar.gz"
echo "$UBUNTU_SHA256  $WORK/ubuntu.tar.gz" | sha256sum -c -
fetch "https://nodejs.org/dist/v$NODE_VERSION/node-v$NODE_VERSION-linux-x64.tar.xz" "$WORK/node.tar.xz"
echo "$NODE_SHA256  $WORK/node.tar.xz" | sha256sum -c -
ROOT=$WORK/rootfs
mkdir "$ROOT"
tar -xpf "$WORK/ubuntu.tar.gz" -C "$ROOT"
tar -xJf "$WORK/node.tar.xz" -C "$ROOT/usr/local" --strip-components=1
install -m755 "$FORGE_BIN" "$ROOT/usr/local/bin/ahvm-forge"
install -m755 "$REPO/images/ubuntu-dev/init.krun" "$ROOT/init.krun"
install -m755 "$REPO/images/ubuntu-dev/provision.sh" "$ROOT/tmp/provision.sh"
install -m644 "$REPO/images/ubuntu-dev/versions.env" "$ROOT/tmp/versions.env"
# Ubuntu base has no host credentials. Only DNS configuration crosses over.
rm -f "$ROOT/etc/resolv.conf"
cp -L /etc/resolv.conf "$ROOT/etc/resolv.conf"
unshare --mount --pid --fork --kill-child bash -eu -c '
    mount --make-rprivate /
    mount -t proc proc "$1/proc"
    mount --rbind /dev "$1/dev"
    chroot "$1" /usr/bin/env -i HOME=/root PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin DEBIAN_FRONTEND=noninteractive /bin/bash /tmp/provision.sh
' bash "$ROOT"
printf 'nameserver 1.1.1.1\n' > "$ROOT/etc/resolv.conf"
truncate -s "${IMG_MB:-16384}M" "$IMAGE_TMP"
mkfs.ext4 -q -F -m 0 -d "$ROOT" "$IMAGE_TMP"
chmod 644 "$IMAGE_TMP"
mv "$IMAGE_TMP" "$OUT"
echo "Built $OUT (Ubuntu $UBUNTU_VERSION, developer image; $(du -h "$OUT" | cut -f1) allocated)"
