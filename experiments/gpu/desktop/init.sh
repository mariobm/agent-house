#!/bin/bash
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /dev/shm /run
mount -t devpts devpts /dev/pts
mount -t tmpfs -o mode=1777 tmpfs /dev/shm
mount -t tmpfs tmpfs /run
mkdir -p /run/user/1000 /home/desktop/.local/share /home/desktop/.cache
chown -R desktop:desktop /home/desktop/.local /home/desktop/.cache
rm -f /home/desktop/input-ok /run/user/1000/vnc.sock
chmod 700 /run/user/1000
chown desktop:desktop /run/user/1000
/usr/lib/systemd/systemd-udevd --daemon
udevadm trigger
udevadm settle
chgrp render /dev/dri/renderD128
chmod 660 /dev/dri/renderD128
ls -l /dev/dri
mkdir -p /run/dbus
dbus-daemon --system --fork
SEATD_VTBOUND=0 seatd -g video > /tmp/seatd.log 2>&1 &
sleep 1
runuser -u desktop -- env HOME=/home/desktop XDG_RUNTIME_DIR=/run/user/1000 AQ_NO_KMS_REQUIREMENT=1 LIBSEAT_BACKEND=seatd LANG=C.UTF-8 dbus-run-session -- start-hyprland -- --config /home/desktop/hyprland.lua > /home/desktop/hyprland.log 2>&1 &
sleep 35
cat /home/desktop/hyprland.log
cat /home/desktop/session.log /home/desktop/wayvnc.log
if grep -qx AHVM-VNC-INPUT-OK /home/desktop/input-ok; then echo DESKTOP_INPUT_OK; fi
sync
exit 0
