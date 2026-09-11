#!/bin/bash
set -euo pipefail
cd /home/desktop
hyprctl output create headless desktop
sleep 1
wayvnc -u -f 30 /run/user/1000/vnc.sock &
foot --font='DejaVu Sans Mono:size=14' --title='AHVM desktop' bash &
while [[ ! -S /run/user/1000/vnc.sock ]]; do sleep 0.1; done
exec /usr/local/bin/desktop-relay
