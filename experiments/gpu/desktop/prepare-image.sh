#!/usr/bin/env bash
# Disposable Arch/Hyprland test image. No installed AHVM state is touched.
set -euo pipefail
[[ $(uname -s) == Linux && $EUID == 0 ]] || { echo 'Requires root on Linux' >&2; exit 1; }
DESKTOP_IMAGE=$(realpath -m "${1:?Usage: prepare-image.sh NEW.ext4}")
[[ ! -e $DESKTOP_IMAGE && ! -L $DESKTOP_IMAGE ]] || { echo 'Output must be new' >&2; exit 1; }
DESKTOP_RECIPE=$(cd "$(dirname "$0")" && pwd)
DESKTOP_WORK=$(mktemp -d)
trap 'rm -rf "$DESKTOP_WORK"' EXIT
curl -fL --retry 3 -o "$DESKTOP_WORK/bootstrap.tar.zst" \
    https://geo.mirror.pkgbuild.com/iso/2026.09.01/archlinux-bootstrap-2026.09.01-x86_64.tar.zst
printf '%s  %s\n' 895661bdf6c64e91b7725874165fd05dd30c438d3ffec661671ab5cfb261ca58 \
    "$DESKTOP_WORK/bootstrap.tar.zst" | sha256sum -c -
tar --zstd -xf "$DESKTOP_WORK/bootstrap.tar.zst" -C "$DESKTOP_WORK"
unshare --mount bash -se -- "$DESKTOP_WORK/root.x86_64" "$DESKTOP_RECIPE" "$DESKTOP_IMAGE" <<'INNER'
mount --make-rprivate /
r=$1
mount --bind "$r" "$r"
mount -t proc proc "$r/proc"
mount --rbind /dev "$r/dev"
mount --rbind /sys "$r/sys"
trap 'if [[ -c $r/dev/null ]]; then chroot "$r" gpgconf --homedir /etc/pacman.d/gnupg --kill all || true; fi; umount -Rl "$r"' EXIT
cp -L /etc/resolv.conf "$r/etc/resolv.conf"
printf 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch\n' > "$r/etc/pacman.d/mirrorlist"
chroot "$r" pacman-key --init
chroot "$r" pacman-key --populate archlinux
# Arch packages remain signature checked. Versions roll; see README for tested versions.
chroot "$r" pacman -Syu --noconfirm hyprland wayvnc foot grim mesa dbus seatd ttf-dejavu python
chroot "$r" useradd -m -G video,render -s /bin/bash desktop
install -m755 "$2/init.sh" "$r/init.krun"
install -m755 "$2/session.sh" "$r/usr/local/bin/desktop-session"
install -m755 "$2/vsock-relay.py" "$r/usr/local/bin/desktop-relay"
cp "$2/hyprland.lua" "$r/home/desktop/hyprland.lua"
chroot "$r" chown -R desktop:desktop /home/desktop
chroot "$r" gpgconf --homedir /etc/pacman.d/gnupg --kill all
umount -R "$r/proc"
umount -Rl "$r/dev"
umount -R "$r/sys"
mkdir -p "$(dirname "$3")"
truncate -s 6G "$3"
mkfs.ext4 -q -F -d "$r" "$3"
INNER
