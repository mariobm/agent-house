#!/bin/bash
set -euxo pipefail
exec > /home/desktop/session.log 2>&1
hyprctl output create headless desktop
sleep 1
hyprctl monitors
wayvnc -u -v /run/user/1000/vnc.sock > /home/desktop/wayvnc.log 2>&1 &
foot --font='DejaVu Sans Mono:size=14' --title='AHVM accelerated desktop' bash --noprofile --norc &
sleep 2
/usr/local/bin/desktop-relay > /home/desktop/relay.log 2>&1 &
grim /home/desktop/desktop.png
sleep 15
position=$(hyprctl cursorpos)
echo "Cursor: $position"
# Absolute VNC coordinates may round down by one pixel in Wayland.
[[ $position == '320, 240' || $position == '319, 239' ]]
echo DESKTOP_POINTER_OK
hyprctl systeminfo
