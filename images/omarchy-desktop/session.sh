#!/bin/bash
set -euo pipefail
hyprctl output create headless desktop
systemctl --user import-environment WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE XDG_CURRENT_DESKTOP
systemctl --user restart ahvm-vnc.service ahvm-desktop-relay.service
foot --font='JetBrainsMono Nerd Font:size=13' --title='Omarchy on AHVM' bash &
