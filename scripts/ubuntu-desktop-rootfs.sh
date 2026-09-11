#!/usr/bin/env bash
# Extend an immutable Ubuntu dev template. All mounts stay in a private namespace.
set -euo pipefail
umask 022
DESKTOP_REPO=$(cd "$(dirname "$0")/.." && pwd)
[[ $(uname -s) == Linux && $(uname -m) == x86_64 && $EUID == 0 ]] || { echo 'Requires root on Linux x86_64' >&2; exit 1; }
DESKTOP_OUT=$(realpath -m "${1:?Usage: FORGE_BIN=... [UBUNTU_DEV_IMAGE=...] ubuntu-desktop-rootfs.sh NEW.ext4}")
[[ ! -e $DESKTOP_OUT && ! -L $DESKTOP_OUT ]] || { echo 'Refusing existing image' >&2; exit 1; }
DESKTOP_FORGE=$(realpath "${FORGE_BIN:?Supply static Forge}")
if readelf -l "$DESKTOP_FORGE" | grep -q INTERP; then echo 'Forge must be statically linked' >&2; exit 1; fi
mkdir -p "$(dirname "$DESKTOP_OUT")"
DESKTOP_WORK=$(mktemp -d)
DESKTOP_TMP=$(mktemp "${DESKTOP_OUT}.build.XXXXXX")
trap 'rm -rf "$DESKTOP_WORK"; rm -f "$DESKTOP_TMP"' EXIT
if [[ -n ${UBUNTU_DEV_IMAGE:-} ]]; then
    cp --sparse=always --reflink=auto "$UBUNTU_DEV_IMAGE" "$DESKTOP_TMP"
else
    "$DESKTOP_REPO/scripts/ubuntu-dev-rootfs.sh" "$DESKTOP_WORK/base.ext4"
    cp --sparse=always "$DESKTOP_WORK/base.ext4" "$DESKTOP_TMP"
fi
mkdir "$DESKTOP_WORK/root"
unshare --mount --pid --fork --kill-child bash -se -- "$DESKTOP_TMP" "$DESKTOP_WORK/root" "$DESKTOP_REPO" "$DESKTOP_FORGE" <<'INNER'
set -euo pipefail
mount --make-rprivate /
mount -o loop "$1" "$2"
mount -t proc proc "$2/proc"
mount --rbind /dev "$2/dev"
cp -L /etc/resolv.conf "$2/etc/resolv.conf"
install -m755 "$3/images/ubuntu-desktop/provision.sh" "$2/tmp/desktop-provision.sh"
chroot "$2" /usr/bin/env -i HOME=/root PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin /bin/bash /tmp/desktop-provision.sh
install -m755 "$3/images/ubuntu-desktop/init.krun" "$2/init.krun"
install -m755 "$3/images/ubuntu-desktop/session.sh" "$2/usr/local/bin/ahvm-desktop-session"
install -m755 "$3/images/ubuntu-desktop/relay.py" "$2/usr/local/bin/ahvm-desktop-relay"
install -m755 "$4" "$2/usr/local/bin/ahvm-forge"
printf 'nameserver 1.1.1.1\n' > "$2/etc/resolv.conf"
sync
umount -R "$2"
INNER
chmod 644 "$DESKTOP_TMP"
mv "$DESKTOP_TMP" "$DESKTOP_OUT"
printf 'Built %s\n' "$DESKTOP_OUT"
