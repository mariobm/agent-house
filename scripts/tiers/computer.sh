#!/bin/bash
# Computer tier: minimal + full desktop (KasmVNC + window manager + Chromium).
# Sources minimal.sh first.
# Called by build-tier.sh with $MOUNT, $ARCH, $DEB_ARCH, $AGENT, $SCRIPT_DIR set.
#
# Gives a complete graphical Linux desktop inside a Firecracker microVM.
# Access via KasmVNC web client on port 6080.
#
# Usage after boot:
#   bhatti create --name desktop --image computer --cpus 2 --memory 4096 --disk 8192
#   bhatti publish desktop -p 6080
#   # Open the URL in your browser → full Ubuntu desktop
#
# For AI agents (DISPLAY is pre-set for uid 1000 via /run/bhatti/env):
#   bhatti exec desktop -- screenshot                       # → /tmp/screen.png
#   bhatti exec desktop -- screenshot --base64              # → base64 PNG to stdout
#   bhatti exec desktop -- xdotool mousemove 640 360 click 1
#   bhatti exec desktop -- xdotool type "hello world"
#   bhatti exec desktop -- xdotool key Return
#   bhatti exec desktop -- chromium-browser https://example.com
set -euo pipefail

# Build minimal base first
"$SCRIPT_DIR/tiers/minimal.sh"

echo "==> Installing computer tier packages..."
chroot "$MOUNT" /bin/bash -c '
set -eu
export DEBIAN_FRONTEND=noninteractive

# Enable universe repo
sed -i "s/ main$/ main universe/" /etc/apt/sources.list
apt-get update -qq

# --- KasmVNC (replaces Xvfb + x11vnc + noVNC + websockify) ---
apt-get install -y --no-install-recommends wget
case $(dpkg --print-architecture) in
    amd64) KASM_DEB="kasmvncserver_noble_1.4.0_amd64.deb" ;;
    arm64) KASM_DEB="kasmvncserver_noble_1.4.0_arm64.deb" ;;
esac
wget -q -O /tmp/kasmvnc.deb "https://github.com/kasmtech/KasmVNC/releases/download/v1.4.0/${KASM_DEB}"
dpkg -i /tmp/kasmvnc.deb || apt-get install -f -y
rm /tmp/kasmvnc.deb

# Fix: KasmVNC looks for web files at /usr/local/share/kasmvnc/www but deb installs to /usr/share
mkdir -p /usr/local/share/kasmvnc
ln -sf /usr/share/kasmvnc/www /usr/local/share/kasmvnc/www

# --- Desktop environment ---
apt-get install -y --no-install-recommends \
    xfce4 xfce4-terminal xfce4-whiskermenu-plugin \
    adwaita-icon-theme-full \
    x11-utils x11-xserver-utils

# --- Chromium via Playwright ---
apt-get install -y --no-install-recommends xz-utils

NODE_VERSION=22.16.0
case $(dpkg --print-architecture) in
    amd64) NODE_ARCH=x64 ;;
    arm64) NODE_ARCH=arm64 ;;
esac
curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
    | tar -xJ --strip-components=1 -C /usr/local

npm install -g playwright@1.50.0
npx playwright install chromium
npx playwright install-deps chromium

# Symlink the full chrome binary so "chromium" works for all users
CHROME_BIN=$(find /root/.cache/ms-playwright -path "*/chrome-linux/chrome" -type f | head -1)
if [ -n "$CHROME_BIN" ]; then
    ln -sf "$CHROME_BIN" /usr/local/bin/chromium
fi
# Also make playwright cache accessible to uid 1000
mkdir -p /home/lohar/.cache
cp -r /root/.cache/ms-playwright /home/lohar/.cache/ms-playwright
chown -R 1000:1000 /home/lohar/.cache
LOHAR_CHROME=$(find /home/lohar/.cache/ms-playwright -path "*/chrome-linux/chrome" -type f 2>/dev/null | head -1)
if [ -n "$LOHAR_CHROME" ]; then
    ln -sf "$LOHAR_CHROME" /usr/local/bin/chromium
fi

# --- Fonts ---
apt-get install -y --no-install-recommends \
    fonts-liberation fonts-dejavu-core fonts-noto-color-emoji fontconfig

# --- Agent tooling ---
apt-get install -y --no-install-recommends \
    xdotool xclip xsel scrot imagemagick

# --- Audio (dummy sink so apps dont crash) ---
apt-get install -y --no-install-recommends pulseaudio

# --- DBus (required by Chromium, GTK apps) ---
apt-get install -y --no-install-recommends dbus dbus-x11

# --- Misc ---
apt-get install -y --no-install-recommends xdg-utils procps

# Register chromium-browser as the system browser
update-alternatives --install /usr/bin/x-www-browser x-www-browser /usr/local/bin/chromium-browser 200 2>/dev/null || \
    ln -sf /usr/local/bin/chromium-browser /usr/bin/x-www-browser

fc-cache -f
apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/*
'

# ==========================================================================
# Agent helper scripts
# ==========================================================================

# screenshot: one command, outputs path or base64
cat > "$MOUNT/usr/local/bin/screenshot" << 'BIN'
#!/bin/sh
export DISPLAY="${DISPLAY:-:99}"
if [ "$1" = "--base64" ]; then
    FMT="png"
    [ "$2" = "--format" ] && [ -n "$3" ] && FMT="$3"
    TMP="/tmp/.screenshot.$$.${FMT}"
    scrot -o "$TMP" 2>/dev/null
    if [ "$FMT" = "jpg" ] || [ "$FMT" = "jpeg" ]; then
        convert "$TMP" -quality 60 "${TMP%.*}.jpg" 2>/dev/null && TMP="${TMP%.*}.jpg"
    fi
    base64 -w0 "$TMP"
    rm -f "$TMP" "${TMP%.*}.jpg" 2>/dev/null
else
    OUT="${1:-/tmp/screen.png}"
    scrot -o "$OUT" && echo "$OUT"
fi
BIN
chmod 755 "$MOUNT/usr/local/bin/screenshot"

# chromium wrapper that sets up env and suppresses banners
cat > "$MOUNT/usr/local/bin/chromium-browser" << 'BIN'
#!/bin/sh
export DISPLAY="${DISPLAY:-:99}"
CHROME="$(find /home/lohar/.cache/ms-playwright /root/.cache/ms-playwright -path "*/chrome-linux/chrome" -type f 2>/dev/null | head -1)"
if [ -z "$CHROME" ]; then
    echo "error: chromium not found" >&2
    exit 1
fi
exec "$CHROME" --no-sandbox --disable-gpu --disable-dev-shm-usage \
    --test-type --disable-infobars --no-first-run --no-default-browser-check \
    "$@"
BIN
chmod 755 "$MOUNT/usr/local/bin/chromium-browser"

# active-window: get the title of the focused window
cat > "$MOUNT/usr/local/bin/active-window" << 'BIN'
#!/bin/sh
export DISPLAY="${DISPLAY:-:99}"
xdotool getactivewindow getwindowname 2>/dev/null
BIN
chmod 755 "$MOUNT/usr/local/bin/active-window"

# list-windows: list all open windows
cat > "$MOUNT/usr/local/bin/list-windows" << 'BIN'
#!/bin/sh
export DISPLAY="${DISPLAY:-:99}"
for wid in $(xdotool search --name '' 2>/dev/null); do
    NAME=$(xdotool getwindowname "$wid" 2>/dev/null)
    [ -n "$NAME" ] && echo "$wid $NAME"
done
BIN
chmod 755 "$MOUNT/usr/local/bin/list-windows"

# screen-size: print display resolution
cat > "$MOUNT/usr/local/bin/screen-size" << 'BIN'
#!/bin/sh
export DISPLAY="${DISPLAY:-:99}"
xdpyinfo -display "$DISPLAY" 2>/dev/null | grep dimensions | awk '{print $2}'
BIN
chmod 755 "$MOUNT/usr/local/bin/screen-size"

# ==========================================================================
# Environment: set DISPLAY globally
# ==========================================================================
echo 'export DISPLAY=:99' >> "$MOUNT/etc/environment"
cat > "$MOUNT/etc/profile.d/bhatti-display.sh" << 'PROF'
export DISPLAY=:99
PROF
chmod 644 "$MOUNT/etc/profile.d/bhatti-display.sh"
echo 'export DISPLAY=:99' >> "$MOUNT/home/lohar/.bashrc"
chown 1000:1000 "$MOUNT/home/lohar/.bashrc"

# ==========================================================================
# Chromium .desktop entry for XFCE app menu
# ==========================================================================
mkdir -p "$MOUNT/usr/share/applications"
cat > "$MOUNT/usr/share/applications/chromium.desktop" << 'DESKTOP'
[Desktop Entry]
Name=Chromium
Exec=chromium-browser %U
Icon=web-browser
Type=Application
Categories=Network;WebBrowser;
MimeType=text/html;text/xml;application/xhtml+xml;
DESKTOP


# ==========================================================================
# KasmVNC config — pre-configure so it starts without interactive prompts
# ==========================================================================
mkdir -p "$MOUNT/root/.vnc"

# Create password file with write-enabled root user
chroot "$MOUNT" /bin/bash -c '
printf "password\npassword\n" | kasmvncpasswd -u root -rwo /root/.vnc/kasmpasswd 2>/dev/null || true
'

# ==========================================================================
# Boot profile: start KasmVNC + desktop stack
# ==========================================================================
mkdir -p "$MOUNT/etc/bhatti"
cat > "$MOUNT/etc/bhatti/init.sh" << 'PROFILE'
#!/bin/sh
# Computer tier boot profile — starts KasmVNC + desktop stack.
# Single port: 6080 (HTTP + WebSocket).

# Resolution from env (set via bhatti create --env), default 1280x720
WIDTH="${DISPLAY_WIDTH:-1280}"
HEIGHT="${DISPLAY_HEIGHT:-720}"
DEPTH="${DISPLAY_DEPTH:-24}"

export DISPLAY=:99
rm -f /tmp/.X99-lock /tmp/.X11-unix/X99

# 1. KasmVNC X server (replaces Xvfb + x11vnc + noVNC + websockify)
/usr/bin/Xkasmvnc :99 \
    -geometry "${WIDTH}x${HEIGHT}" -depth "$DEPTH" \
    -websocketPort 6080 \
    -interface 0.0.0.0 \
    -BlacklistTimeout 0 \
    -FreeKeyMappings \
    -disableBasicAuth \
    -AlwaysShared \
    -SecurityTypes None \
    &

# Wait for X
for i in $(seq 1 30); do
    xdpyinfo -display :99 >/dev/null 2>&1 && break
    sleep 0.1
done
if ! xdpyinfo -display :99 >/dev/null 2>&1; then
    echo "bhatti: KasmVNC not ready after 3s" >&2
    exit 1
fi

# 2. System DBus (Chromium needs it)
mkdir -p /run/dbus
dbus-daemon --system --fork 2>/dev/null || true

# 3. Session DBus
eval $(dbus-launch --sh-syntax)

# 4. Dummy audio sink (non-fatal)
pulseaudio --start --exit-idle-time=-1 2>/dev/null || true

# 5. XFCE desktop session
startxfce4 &

# 6. Write runtime env for lohar (so bhatti exec inherits DISPLAY)
mkdir -p /run/bhatti
echo "DISPLAY=:99" > /run/bhatti/env
PROFILE
chmod 755 "$MOUNT/etc/bhatti/init.sh"

echo "==> Computer tier done."
