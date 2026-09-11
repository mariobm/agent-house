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
  ttf-jetbrains-mono-nerd firefox xdg-utils imagemagick gum perl lua
mkdir -p "$r/usr/share/omarchy"
cp -a "$3/." "$r/usr/share/omarchy/"
# The pinned upstream keybinding scanner mocks live compositor APIs. Its generic
# mock is a table, but qconsole compares monitor.scale with a number.
python3 - "$r/usr/share/omarchy/bin/omarchy-menu-keybindings" <<'PYFIX'
from pathlib import Path
import sys
p = Path(sys.argv[1])
s = p.read_text()
needle = "hl = setmetatable({\n"
assert s.count(needle) == 1, "Upstream keybinding scanner changed; review this adapter"
p.write_text(s.replace(needle, needle + "  get_active_monitor = function() return nil end,\n"))
PYFIX
cp -a "$r/usr/share/omarchy/config/." "$r/home/desktop/.config/"
install -m755 "$4/session.sh" "$r/usr/local/bin/desktop-session"
cat "$4/hyprland.lua" >> "$r/home/desktop/.config/hypr/hyprland.lua"
cp "$r/home/desktop/.config/hypr/hyprland.lua" "$r/home/desktop/hyprland.lua"
"$4/install-systemd.sh" "$r"
# This VM has an AHVM-managed virtual NIC, not a Wi-Fi device/NetworkManager.
printf '[[ -d /sys/class/net/wlan0/wireless ]] || exit 0\n' | cat - "$r/usr/share/omarchy/install/user/first-run/wifi.sh" > "$r/tmp/ahvm-wifi.sh"
mv "$r/tmp/ahvm-wifi.sh" "$r/usr/share/omarchy/install/user/first-run/wifi.sh"
chroot "$r" chown -R desktop:desktop /home/desktop
chroot "$r" runuser -u desktop -- env HOME=/home/desktop \
  OMARCHY_PATH=/usr/share/omarchy PATH=/usr/share/omarchy/bin:/usr/bin \
  XDG_RUNTIME_DIR=/tmp OMARCHY_THEME_HEADLESS=1 omarchy-theme-set 'Tokyo Night'
# Remove state inherited from the development base before distribution.
rm -rf "$r/home/desktop/.cache" "$r/home/desktop/.mozilla" "$r/home/desktop/.ssh" "$r/root/.ssh"
rm -f "$r/home/desktop/.bash_history" "$r/root/.bash_history" "$r/etc/ssh/ssh_host_"*
find "$r/var/log" -type f -exec truncate -s 0 {} +
rm -rf "$r/var/log/journal/"* "$r/var/cache/pacman/pkg/"*
rm -f "$r/var/lib/systemd/random-seed"
: > "$r/etc/machine-id"
INNER
