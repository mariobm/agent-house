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
# Tunables (set via `bhatti create --env KEY=value,KEY=value`):
#   DISPLAY_WIDTH    default 1280
#   DISPLAY_HEIGHT   default 720
#   DISPLAY_DEPTH    default 24
#   KASM_FRAMERATE   default 60  (max frames/sec the encoder emits; matches upstream)
#   KASM_THREADS     default $((nproc-1))  (encoder thread count; auto leaves 1 vCPU for desktop)
#
# Beyond these, edit /etc/kasmvnc/kasmvnc.yaml inside the sandbox; KasmVNC reloads
# its config on the next session. See https://github.com/kasmtech/KasmVNC/wiki for
# the full option set.
#
# Init model (per PLAN-tiers-systemd.md): the desktop stack is managed by
# lohar's systemctl shim as four units, replacing the monolithic init.sh:
#   * kasmvnc-firstboot.service  — oneshot, generates per-sandbox password
#                                  + computes default KASM_THREADS
#   * kasmvnc.service            — the X server + RFB→WebSocket gateway
#   * xfce-session.service       — XFCE desktop session
#   * bhatti-display-env.service — exposes DISPLAY=:99 to `bhatti exec`
#
# Deliberately NOT started: dbus-daemon (long-lived inotify watches +
# epoll sets don't survive snapshot/restore cleanly on ARM64 — see
# PLAN-systemd-rc.md for the underlying reason) and pulseaudio (not used
# by any documented agent flow). The dbus packages stay installed so
# libdbus keeps linking; we just don't run a system bus at boot.
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

# --- DBus libs (NO daemon — see init-model comment at top) ---
# We keep the dbus/dbus-x11 packages so libdbus links continue to work for
# anything XFCE/Chromium has compiled against, but we do NOT start a
# system bus at boot. Per the plan: long-lived dbus state (inotify,
# epoll) doesnt survive Firecracker snapshot/restore cleanly on ARM64.
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
# Agent helper scripts (unchanged from pre-v1.11.9 — these are
# user-facing binaries, not boot-path code)
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
# Environment: set DISPLAY globally (for interactive shells / PAM)
#
# The systemd-managed services get DISPLAY from their own Environment=
# directives; these files cover ssh-like interactive sessions and the
# `bhatti exec` path (which also picks up DISPLAY via /run/bhatti/env
# written by bhatti-display-env.service below).
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
# Systemd units (replacing the legacy /etc/bhatti/init.sh)
#
# Activation graph at boot:
#   kasmvnc-firstboot.service ──┐
#                               ├──> kasmvnc.service ──> xfce-session.service
#                               │
#                               └──> (none — leaf)
#   bhatti-display-env.service   (no After=, runs in first wave)
#
# kasmvnc-firstboot generates per-sandbox credentials (idempotent via
# ConditionPathExists=!) and computes the KASM_THREADS default by
# appending to /run/bhatti/config-env, but ONLY if the user didn't pass
# their own value via `bhatti create --env`. User-supplied values appear
# in /run/bhatti/config-env before firstboot writes its append, so the
# grep guard preserves user precedence.
# ==========================================================================

# --- kasmvnc-firstboot.service ---
# ConditionPathExists=! means: skip with no error if the file already
# exists. On snapshot/resume the password is already there → skipped.
# RemainAfterExit=yes so other units' After=kasmvnc-firstboot.service
# treats it as active once it has run.
cat > "$MOUNT/etc/systemd/system/kasmvnc-firstboot.service" << 'UNIT'
[Unit]
Description=Generate per-sandbox VNC credentials + compute KASM_THREADS default
ConditionPathExists=!/root/.kasmpasswd

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c '\
    PW=$(tr -dc "A-HJ-NP-Za-km-z2-9" </dev/urandom 2>/dev/null | head -c 16); \
    [ -z "$PW" ] && PW=$(date +%s%N | sha256sum | head -c 16); \
    mkdir -p /root/.vnc; \
    printf "%s\n%s\n" "$PW" "$PW" | kasmvncpasswd -u kasm_user -wo /root/.kasmpasswd >/dev/null 2>&1; \
    chmod 600 /root/.kasmpasswd; \
    printf "username: kasm_user\npassword: %s\n" "$PW" > /root/.vnc/cleartext; \
    chmod 600 /root/.vnc/cleartext; \
    mkdir -p /run/bhatti; \
    if ! grep -q "^KASM_THREADS=" /run/bhatti/config-env 2>/dev/null; then \
        NPROC=$(nproc 2>/dev/null || echo 1); \
        THREADS=$([ "$NPROC" -gt 1 ] && echo $((NPROC - 1)) || echo 1); \
        echo "KASM_THREADS=$THREADS" >> /run/bhatti/config-env; \
    fi'

[Install]
WantedBy=multi-user.target
UNIT

# --- kasmvnc.service ---
#
# Type=simple. KasmVNC has no sd_notify support, so the shim considers
# the unit active as soon as fork+exec succeeds. Users who need
# readiness can poll `xdpyinfo -display :99` or the websocket port
# (`curl -I http://127.0.0.1:6080`).
#
# Defaults live in Environment=; EnvironmentFile=- picks up
# `bhatti create --env DISPLAY_WIDTH=…` overrides. Shell-style $VAR
# expansion happens inside the shim's /bin/sh -c wrapper.
cat > "$MOUNT/etc/systemd/system/kasmvnc.service" << 'UNIT'
[Unit]
Description=KasmVNC X server + RFB→WebSocket gateway
After=kasmvnc-firstboot.service
Requires=kasmvnc-firstboot.service

[Service]
Type=simple
Environment=DISPLAY_WIDTH=1280
Environment=DISPLAY_HEIGHT=720
Environment=DISPLAY_DEPTH=24
Environment=KASM_FRAMERATE=60
Environment=KASM_THREADS=1
EnvironmentFile=-/run/bhatti/config-env
ExecStartPre=/bin/sh -c 'rm -f /tmp/.X99-lock /tmp/.X11-unix/X99'
ExecStart=/bin/sh -c '/usr/bin/Xkasmvnc :99 \
    -geometry ${DISPLAY_WIDTH}x${DISPLAY_HEIGHT} -depth ${DISPLAY_DEPTH} \
    -websocketPort 6080 \
    -interface 0.0.0.0 \
    -BlacklistTimeout 0 \
    -FreeKeyMappings \
    -AlwaysShared \
    -SecurityTypes=None \
    -FrameRate=${KASM_FRAMERATE} \
    -RectThreads=${KASM_THREADS}'
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=multi-user.target
UNIT

# --- xfce-session.service ---
#
# Requires=kasmvnc.service so killing kasmvnc cascades (xfce without an
# X server is pointless). After= guarantees ordering.
cat > "$MOUNT/etc/systemd/system/xfce-session.service" << 'UNIT'
[Unit]
Description=XFCE desktop session
After=kasmvnc.service
Requires=kasmvnc.service

[Service]
Type=simple
Environment=DISPLAY=:99
Environment=HOME=/root
ExecStart=/usr/bin/startxfce4
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=multi-user.target
UNIT

# --- bhatti-display-env.service ---
#
# Writes DISPLAY=:99 to /run/bhatti/env so the lohar agent's env-merge
# (cmd/lohar/main.go, post-startEnabledServices) picks it up and exposes
# DISPLAY to every `bhatti exec` invocation. Oneshot with no After=
# dependency — runs in the first activation wave, completes before any
# wave is considered done.
cat > "$MOUNT/etc/systemd/system/bhatti-display-env.service" << 'UNIT'
[Unit]
Description=Expose DISPLAY=:99 to bhatti exec via /run/bhatti/env

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'mkdir -p /run/bhatti && echo DISPLAY=:99 > /run/bhatti/env'

[Install]
WantedBy=multi-user.target
UNIT

# Enable all four via wants/ symlinks (deb-systemd-helper isn't involved
# for our own units — we just create the symlink directly).
for unit in kasmvnc-firstboot.service kasmvnc.service xfce-session.service bhatti-display-env.service; do
    ln -sf "/etc/systemd/system/$unit" \
        "$MOUNT/etc/systemd/system/multi-user.target.wants/$unit"
done

# --- Drop the legacy init.sh path entirely ---
rm -f "$MOUNT/etc/bhatti/init.sh"

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
