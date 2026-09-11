#!/usr/bin/env bash
# Install only into an already mounted AHVM image, never the host root.
set -euo pipefail
r=$(realpath "${1:?Supply mounted guest root}")
[[ $r != / && -f $r/init.krun && -f $r/usr/local/bin/ahvm-forge ]] || exit 1
recipe=$(cd "$(dirname "$0")" && pwd)
install -m755 "$recipe/init.krun" "$r/init.krun"
install -m755 "$recipe/session.sh" "$r/usr/local/bin/desktop-session"
install -m755 "$recipe/wait-ready.sh" "$r/usr/local/bin/ahvm-desktop-wait"
install -m755 "$recipe/relay.py" "$r/usr/local/bin/desktop-relay"
mkdir -p "$r/etc/systemd/system/multi-user.target.wants" "$r/etc/systemd/user" \
  "$r/etc/systemd/system/seatd.service.d" "$r/etc/systemd/journald.conf.d" "$r/etc/sysctl.d"
for unit in ahvm-forge ahvm-desktop; do
 install -m644 "$recipe/$unit.service" "$r/etc/systemd/system/$unit.service"
 ln -sf "../$unit.service" "$r/etc/systemd/system/multi-user.target.wants/$unit.service"
done
install -m644 "$recipe/ahvm-vnc.service" "$recipe/ahvm-desktop-relay.service" "$r/etc/systemd/user/"
printf '[Service]\nEnvironment=SEATD_VTBOUND=0\nExecStart=\nExecStart=/usr/bin/seatd -g video\n' > "$r/etc/systemd/system/seatd.service.d/ahvm.conf"
printf '[Journal]\nStorage=persistent\nSystemMaxUse=64M\n' > "$r/etc/systemd/journald.conf.d/ahvm.conf"
# The bundled kernel uses the small PID limit; Arch's 4194304 exceeds it.
printf 'kernel.pid_max = 32768\n' > "$r/etc/sysctl.d/99-ahvm.conf"
printf 'ahvm-omarchy\n' > "$r/etc/hostname"
printf 'LANG=C.UTF-8\n' > "$r/etc/locale.conf"
ln -sf /usr/share/zoneinfo/UTC "$r/etc/localtime"
: > "$r/etc/machine-id"
ln -sf /etc/machine-id "$r/var/lib/dbus/machine-id"
# AHVM owns the virtual interface and DNS configuration, not DHCP.
for path in "$r"/usr/lib/systemd/system/systemd-networkd* "$r"/usr/lib/systemd/system/systemd-resolved*; do
 [[ -f $path ]] || continue
 ln -sf /dev/null "$r/etc/systemd/system/$(basename "$path")"
done
ln -sf /dev/null "$r/etc/systemd/system/systemd-firstboot.service"
mkdir -p "$r/workspace"
chroot "$r" chown desktop:desktop /workspace
