#!/usr/bin/env bash
# Experimental Omarchy desktop adapted to AHVM's existing Arch GPU image.
set -euo pipefail
[[ $(uname -s) == Linux && $EUID == 0 ]] || exit 1
base=$(realpath "${1:?Usage: prepare-image.sh ARCH_DESKTOP.ext4 NEW.ext4 OMARCHY_SOURCE}")
image=$(realpath -m "${2:?New output image}")
source_dir=$(realpath "${3:?Pinned Omarchy source directory}")
[[ -f $base && ! -e $image && ! -L $image && -f $source_dir/version ]] || exit 1
recipe=$(cd "$(dirname "$0")" && pwd)
cp --sparse=always --reflink=auto "$base" "$image"
truncate -s 40G "$image"
e2fsck -f -p "$image" || [[ $? == 1 ]]
resize2fs "$image"
mount_dir=$(mktemp -d)
trap 'rmdir "$mount_dir"' EXIT
unshare --mount --pid --fork bash -se -- "$image" "$mount_dir" "$source_dir" "$recipe" <<'INNER'
mount --make-rprivate /
r=$2
mount -o loop "$1" "$r"
trap 'umount -R "$r"' EXIT
mount -t proc proc "$r/proc"
mount --rbind /dev "$r/dev"
cp -L /etc/resolv.conf "$r/etc/resolv.conf"
chroot "$r" pacman -Syu --needed --noconfirm quickshell qt6-multimedia qt6-svg qt6-5compat \
  qt6-wayland jq socat wl-clipboard cliphist playerctl pipewire wireplumber \
  ttf-jetbrains-mono-nerd firefox xdg-utils imagemagick gum
mkdir -p "$r/usr/share/omarchy"
cp -a "$3/." "$r/usr/share/omarchy/"
cp -a "$r/usr/share/omarchy/config/." "$r/home/desktop/.config/"
install -m755 "$4/session.sh" "$r/usr/local/bin/desktop-session"
cat "$4/hyprland.lua" >> "$r/home/desktop/.config/hypr/hyprland.lua"
cp "$r/home/desktop/.config/hypr/hyprland.lua" "$r/home/desktop/hyprland.lua"
# AHVM has its own PID 1; launch Quickshell without a systemd user journal.
printf '#!/bin/bash\nexec quickshell -n -p /usr/share/omarchy/shell\n' > "$r/usr/share/omarchy/bin/omarchy-launch-shell"
chmod 755 "$r/usr/share/omarchy/bin/omarchy-launch-shell"
chroot "$r" chown -R desktop:desktop /home/desktop
chroot "$r" runuser -u desktop -- env HOME=/home/desktop \
  OMARCHY_PATH=/usr/share/omarchy PATH=/usr/share/omarchy/bin:/usr/bin \
  XDG_RUNTIME_DIR=/tmp OMARCHY_THEME_HEADLESS=1 omarchy-theme-set 'Tokyo Night'
INNER
