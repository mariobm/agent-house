#!/usr/bin/env bash
# Omarchy 4.0.4 userland adapted to a clean AHVM Arch GPU image.
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
trap 'chroot "$r" gpgconf --homedir /etc/pacman.d/gnupg --kill all || true; umount -Rl "$r"' EXIT
mount -t proc proc "$r/proc"
mount --rbind /dev "$r/dev"
cp -L /etc/resolv.conf "$r/etc/resolv.conf"
# Trust the release signing key identified by upstream omarchy-update-keyring.
# Keep package signature verification enabled for both repositories.
chroot "$r" pacman-key --recv-keys 40DFB630FF42BCFFB047046CF0134EE680CAC571 --keyserver keys.openpgp.org
chroot "$r" pacman-key --lsign-key 40DFB630FF42BCFFB047046CF0134EE680CAC571
install -m644 "$3/default/pacman/pacman-stable.conf" "$r/etc/pacman.conf"
install -m644 "$3/default/pacman/mirrorlist-stable" "$r/etc/pacman.d/mirrorlist"
printf 'Server = https://geo.mirror.pkgbuild.com/$repo/os/$arch\n' >> "$r/etc/pacman.d/mirrorlist"
mapfile -t packages < <(sed '/^[[:space:]]*#/d; /^[[:space:]]*$/d' "$3/install/omarchy-base.packages")
# Install upstream's complete application set. The omarchy meta-package pulls
# physical bootloader/snapshot hooks, so install settings and runtime separately.
chroot "$r" pacman -Syu --needed --noconfirm omarchy-keyring 'omarchy-settings=4.0.4' \
  "${packages[@]}" sudo base-devel python python-pip wayvnc seatd mesa dbus \
  qt6-multimedia qt6-svg qt6-5compat qt6-wayland cliphist playerctl pipewire lua firefox
mkdir -p "$r/usr/share/omarchy"
cp -a "$3/." "$r/usr/share/omarchy/"
# Upstream v4.0.4 still contains an alpha version file; the release tag/package is authoritative.
printf '4.0.4\n' > "$r/usr/share/omarchy/version"
for executable in "$r/usr/share/omarchy/bin/"*; do
  [[ -f $executable ]] || continue
  name=$(basename "$executable")
  # Settings owns a few support binaries. Leave its package-owned files intact.
  [[ -e "$r/usr/bin/$name" ]] || ln -s "/usr/share/omarchy/bin/$name" "$r/usr/bin/$name"
done
cp -a "$r/etc/skel/." "$r/home/desktop/"
install -m644 "$3/default/bashrc" "$r/home/desktop/.bashrc"
install -m644 "$3/etc/profile.d/omarchy.sh" "$r/etc/profile.d/omarchy.sh"
chroot "$r" usermod -aG wheel,docker desktop
printf 'desktop ALL=(ALL:ALL) NOPASSWD: ALL\n' > "$r/etc/sudoers.d/ahvm-desktop"
chmod 440 "$r/etc/sudoers.d/ahvm-desktop"
chroot "$r" visudo -cf /etc/sudoers.d/ahvm-desktop
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
# Runtime is installed from the verified source archive, without the physical
# machine meta-package. Preserve upstream's package-first version lookup.
python3 - "$r/usr/share/omarchy/bin/omarchy-version" <<'PYVERSION'
from pathlib import Path
import sys
p = Path(sys.argv[1])
s = p.read_text()
needle = '[[ -n $version ]] || exit 1'
assert s.count(needle) == 1, "Review upstream version lookup"
p.write_text(s.replace(needle, '[[ -n $version ]] || version=$(cat "$omarchy_path/version")'))
PYVERSION
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
# Seed Omarchy's launchers, then eagerly install the common coding tools so
# opening a terminal does not require a first-use package download.
chroot "$r" runuser -u desktop -- env HOME=/home/desktop OMARCHY_PATH=/usr/share/omarchy \
  PATH=/usr/share/omarchy/bin:/usr/bin bash -ec '
    OMARCHY_SETUP_CONTEXT=ahvm-build omarchy-provision-user --first-install
    mise use -g node@lts bun@latest codex@latest claude@latest opencode@latest pi@latest
    mise reshim
    mise prune --yes
    mkdir -p ~/Work/tries
  '
mkdir -p "$r/usr/share/ahvm"
chroot "$r" pacman -Q > "$r/usr/share/ahvm/packages.txt"
chroot "$r" runuser -u desktop -- env HOME=/home/desktop mise ls --json > "$r/usr/share/ahvm/tools.json"
printf 'omarchy=4.0.4\nsource=c668141e9c42b13c80c9ca4ea108e11708c5e8a5\n' > "$r/usr/share/ahvm/image-build.txt"
# Remove state inherited from the development base before distribution.
rm -rf "$r/home/desktop/.cache" "$r/home/desktop/.mozilla" "$r/home/desktop/.ssh" "$r/root/.ssh"
rm -f "$r/home/desktop/.bash_history" "$r/root/.bash_history" "$r/etc/ssh/ssh_host_"*
find "$r/var/log" -type f -exec truncate -s 0 {} +
rm -rf "$r/var/log/journal/"* "$r/var/cache/pacman/pkg/"*
rm -f "$r/var/lib/systemd/random-seed"
: > "$r/etc/machine-id"
# Remove discarded package caches from the distributed sparse disk too.
fstrim "$r"
INNER
