#!/bin/bash
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
mountpoint -q /proc || mount -t proc proc /proc
mountpoint -q /sys || mount -t sysfs sysfs /sys
mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /tmp/runtime-root
mount -t devpts devpts /dev/pts
chmod 700 /tmp/runtime-root
export XDG_RUNTIME_DIR=/tmp/runtime-root
printf '\n=== AHVM GPU PROBE ===\n'
ls -l /dev/dri
/usr/local/bin/render-probe
result=$?
echo "GPU_PROBE_EXIT=$result"
dmesg | grep -Ei 'virtio.*gpu|drm|virgl' | tail -20
sync
exit "$result"
