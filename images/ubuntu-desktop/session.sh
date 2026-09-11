#!/bin/bash
set -euo pipefail
export DISPLAY=:1
cd /workspace
# Private Unix transport only. Authentication/ownership are enforced by AHVM.
# TigerVNC parses rfbunixmode as decimal: 384 gives Unix mode 0600.
Xtigervnc :1 -geometry 1280x720 -depth 24 -rfbport -1 \
    -rfbunixpath "$XDG_RUNTIME_DIR/vnc.sock" -rfbunixmode 384 \
    -SecurityTypes None -localhost -nolisten tcp -ac &
vnc=$!
trap 'kill "$vnc" 2>/dev/null || true' EXIT
for ((i=0;i<100;i++)); do
    if xdpyinfo >/dev/null 2>&1; then break; fi
    kill -0 "$vnc" || exit 1
    sleep 0.1
done
xdpyinfo >/dev/null
dbus-run-session -- xfce4-session &
exec /usr/local/bin/ahvm-desktop-relay
