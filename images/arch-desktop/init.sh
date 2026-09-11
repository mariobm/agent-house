#!/bin/bash
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc
printf '%s\n' ahvm-desktop > /proc/sys/kernel/hostname
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /dev/shm /run
mount -t devpts devpts /dev/pts
mount -t tmpfs -o mode=1777 tmpfs /dev/shm
mount -t tmpfs tmpfs /run
mkdir -p /run/user/1000 /home/desktop/.local/share /home/desktop/.cache
chown -R desktop:desktop /home/desktop/.local /home/desktop/.cache

chmod 700 /run/user/1000
chown desktop:desktop /run/user/1000
/usr/lib/systemd/systemd-udevd --daemon
udevadm trigger
udevadm settle
chgrp render /dev/dri/renderD128
chmod 660 /dev/dri/renderD128
ip link set lo up
mkdir -p /run/dbus
dbus-daemon --system --fork
SEATD_VTBOUND=0 seatd -g video > /tmp/seatd.log 2>&1 &
sleep 1
runuser -u desktop -- env HOME=/home/desktop XDG_RUNTIME_DIR=/run/user/1000 AQ_NO_KMS_REQUIREMENT=1 LIBSEAT_BACKEND=seatd LANG=C.UTF-8 dbus-run-session -- start-hyprland -- --config /home/desktop/hyprland.lua > /home/desktop/hyprland.log 2>&1 &
# The parent remains PID 1 and reaps children. Forge owns the control socket.
/usr/local/bin/ahvm-forge &
while true; do wait -n || true; sleep 1; done
