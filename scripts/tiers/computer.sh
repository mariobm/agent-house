#!/bin/bash
# Computer tier: minimal + full desktop (KasmVNC + window manager + Chromium).
# Sources minimal.sh first.
# Called by build-tier.sh with $MOUNT, $ARCH, $DEB_ARCH, $AGENT, $SCRIPT_DIR set.
#
# Gives a complete graphical Linux desktop inside a Firecracker microVM.
# Access via KasmVNC web client on port 6080.
#
# First-time usage:
#   bhatti create --name desktop --image computer --cpus 2 --memory 4096 --disk-size 8192
#   bhatti publish desktop -p 6080
#   bhatti exec desktop -- vnc-creds      # ← prints username + per-sandbox password
#   # Open the published URL in your browser, log in with those creds.
#
# For AI agents (DISPLAY is pre-set for uid 1000 via /run/bhatti/env):
#   bhatti exec desktop -- screenshot                       # → /tmp/screen.png
#   bhatti exec desktop -- screenshot --base64              # → base64 PNG to stdout
#   bhatti exec desktop -- xdotool mousemove 640 360 click 1
#   bhatti exec desktop -- xdotool type "hello world"
#   bhatti exec desktop -- xdotool key Return
#   bhatti exec desktop -- chromium-browser https://example.com
#
# Tunables (set via `bhatti create --env KEY=value`):
#   DISPLAY_WIDTH    default 1280
#   DISPLAY_HEIGHT   default 720
#   DISPLAY_DEPTH    default 24
#   KASM_FRAMERATE   default 60  (max frames/sec the encoder emits; matches upstream)
#   KASM_THREADS     default $((nproc-1))  (encoder thread count; auto leaves 1 vCPU for desktop)
#
# Beyond these, edit /etc/kasmvnc/kasmvnc.yaml inside the sandbox; KasmVNC reloads
# its config on the next session. See https://github.com/kasmtech/KasmVNC/wiki for
# the full option set.
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
# KasmVNC: NO bake-time password.
# ----------------------------------------------------------------------------
# A random password is generated on first boot in /etc/bhatti/init.sh below,
# hashed into /root/.kasmpasswd (the upstream-default location), and stored
# cleartext in /root/.vnc/cleartext (root:root 0600) so the `vnc-creds`
# helper can print it on demand.
#
# Why first-boot, not bake-time:
#   - Every sandbox gets unique credentials.
#   - The published rootfs image carries no shared secret.
#   - Snapshot/resume preserves the password (file already exists, skip regen).

# ==========================================================================
# Boot profile: start KasmVNC + desktop stack
# ==========================================================================
mkdir -p "$MOUNT/etc/bhatti"
cat > "$MOUNT/etc/bhatti/init.sh" << 'PROFILE'
#!/bin/sh
# Computer tier boot profile — starts KasmVNC + desktop stack.
# Single port: 6080 (HTTP + WebSocket).
#
# Auth: HTTP Basic over the WebSocket. A random password is generated on
# first boot. `bhatti exec <name> -- vnc-creds` prints it.

# Tunables (override via `bhatti create --env KEY=value`)
WIDTH="${DISPLAY_WIDTH:-1280}"
HEIGHT="${DISPLAY_HEIGHT:-720}"
DEPTH="${DISPLAY_DEPTH:-24}"
FRAMERATE="${KASM_FRAMERATE:-60}"
NPROC=$(nproc 2>/dev/null || echo 1)
THREADS="${KASM_THREADS:-$([ "$NPROC" -gt 1 ] && echo $((NPROC - 1)) || echo 1)}"

export DISPLAY=:99
rm -f /tmp/.X99-lock /tmp/.X11-unix/X99

# --- First-boot credential generation -------------------------------------
# Generate a random password the first time this VM boots, then persist it
# (both the kasmpasswd hash KasmVNC reads and a cleartext copy that the
# `vnc-creds` helper can show via sudo). On subsequent boots — including
# resume from snapshot — the file already exists and we skip regen.
if [ ! -f /root/.kasmpasswd ]; then
    PW=$(tr -dc 'A-HJ-NP-Za-km-z2-9' </dev/urandom 2>/dev/null | head -c 16)
    if [ -z "$PW" ]; then
        # Fallback for kernels without /dev/urandom early; should never hit.
        PW=$(date +%s%N | sha256sum | head -c 16)
    fi
    mkdir -p /root/.vnc
    printf '%s\n%s\n' "$PW" "$PW" | kasmvncpasswd -u kasm_user -wo /root/.kasmpasswd >/dev/null 2>&1
    chmod 600 /root/.kasmpasswd
    printf 'username: kasm_user\npassword: %s\n' "$PW" > /root/.vnc/cleartext
    chmod 600 /root/.vnc/cleartext
fi

# 1. KasmVNC X server (replaces Xvfb + x11vnc + noVNC + websockify).
# Note: NO -disableBasicAuth — it would silently 401 every /api/* endpoint
#       (see common/network/websocket.c in KasmVNC source: the `owner` flag
#       is only set inside the basic-auth branch).
/usr/bin/Xkasmvnc :99 \
    -geometry "${WIDTH}x${HEIGHT}" -depth "$DEPTH" \
    -websocketPort 6080 \
    -interface 0.0.0.0 \
    -BlacklistTimeout 0 \
    -FreeKeyMappings \
    -AlwaysShared \
    -FrameRate="$FRAMERATE" \
    -RectThreads="$THREADS" \
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

# ==========================================================================
# vnc-creds helper: prints the per-sandbox username + password.
#
# Reads /root/.vnc/cleartext via sudo (lohar/uid 1000 has passwordless sudo).
# This is the documented first-time-user discovery path:
#   bhatti exec <name> -- vnc-creds          # human-readable
#   bhatti exec <name> -- vnc-creds --json   # machine-readable for agents
# ==========================================================================
cat > "$MOUNT/usr/local/bin/vnc-creds" << 'BIN'
#!/bin/sh
set -eu
FILE=/root/.vnc/cleartext
if [ ! -r "$FILE" ]; then
    # Fall back to sudo for non-root callers (typical: lohar runs exec as uid 1000).
    if ! sudo -n test -r "$FILE" 2>/dev/null; then
        echo "vnc-creds: cannot read $FILE; KasmVNC may still be initializing" >&2
        exit 1
    fi
    USER_LINE=$(sudo -n awk -F': ' '$1=="username"{print $2}' "$FILE")
    PASS_LINE=$(sudo -n awk -F': ' '$1=="password"{print $2}' "$FILE")
else
    USER_LINE=$(awk -F': ' '$1=="username"{print $2}' "$FILE")
    PASS_LINE=$(awk -F': ' '$1=="password"{print $2}' "$FILE")
fi

if [ "${1:-}" = "--json" ]; then
    printf '{"username":"%s","password":"%s"}\n' "$USER_LINE" "$PASS_LINE"
else
    printf 'username: %s\npassword: %s\n' "$USER_LINE" "$PASS_LINE"
    printf '\nUse these creds at the URL printed by `bhatti publish <name> -p 6080`.\n'
fi
BIN
chmod 755 "$MOUNT/usr/local/bin/vnc-creds"

echo "==> Computer tier done."
