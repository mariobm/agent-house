#!/bin/bash
# Browser tier: minimal + headless Chromium + Playwright.
# Sources minimal.sh first.
# Called by build-tier.sh with $MOUNT, $ARCH, $DEB_ARCH, $AGENT, $SCRIPT_DIR set.
#
# Node.js only — no Python runtime. Users who want Python Playwright
# can `pip install playwright` after boot. The rootfs ships the smallest
# path: Node.js (already needed for npx) + npx playwright install.
#
# Init model (per PLAN-tiers-systemd.md): headless_shell is managed by
# lohar's systemctl shim via headless-chrome.service. Pre-v1.11.9 this
# tier started Chromium by hand out of /etc/bhatti/init.sh; the shim's
# Restart=on-failure now resurrects a crashed Chromium without action.
set -euo pipefail

PLAYWRIGHT_VERSION="1.50.0"
NODE_VERSION="22.16.0"

# Build minimal base first
"$SCRIPT_DIR/tiers/minimal.sh"

echo "==> Installing browser tier packages..."
chroot "$MOUNT" /bin/bash -c "
set -eu
export DEBIAN_FRONTEND=noninteractive
# Enable universe repo (needed by playwright install-deps for fonts, xvfb).
# debootstrap creates /etc/apt/sources.list with the correct mirror for the
# target arch. Don't replace it — just append universe to the existing line.
sed -i 's/ main$/ main universe/' /etc/apt/sources.list
apt-get update -qq

# xz-utils needed to decompress Node.js tarball (.tar.xz)
apt-get install -y --no-install-recommends xz-utils

# Node.js
case \$(dpkg --print-architecture) in
    amd64) NODE_ARCH=x64 ;;
    arm64) NODE_ARCH=arm64 ;;
esac
echo '==> Installing Node.js ${NODE_VERSION}...'
curl -fsSL \"https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-\${NODE_ARCH}.tar.xz\" \\
    | tar -xJ --strip-components=1 -C /usr/local

# Playwright (pinned version) — installs the JS API + CLI
echo '==> Installing Playwright ${PLAYWRIGHT_VERSION}...'
npm install -g playwright@${PLAYWRIGHT_VERSION}

# Install Playwright's headless shell + system deps.
# We use headless_shell (not the full chromium binary) because Chrome 133+
# new headless mode has broken CDP: commands sent to page sessions via
# Target.attachToTarget get no response. headless_shell is the dedicated
# headless binary that Playwright itself uses for launch().
echo '==> Installing Playwright headless shell + deps...'
npx playwright install chromium
npx playwright install-deps chromium

apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/*
"

# --- Resolve the headless_shell path at build time ---
#
# Playwright drops headless_shell into a versioned directory like
#   /root/.cache/ms-playwright/chromium_headless_shell-<rev>/chrome-linux/headless_shell
# The path is stable for the lifetime of this rootfs (we're not running
# `npx playwright install` again at runtime), so resolve it once here and
# bake it into the unit. This is the pattern the plan called for — no
# boot-time `find`.
HEADLESS_SHELL=$(chroot "$MOUNT" find /root/.cache/ms-playwright \
    -path '*/chrome-linux/headless_shell' -type f 2>/dev/null | head -1)
if [ -z "$HEADLESS_SHELL" ]; then
    echo "error: headless_shell not found after Playwright install" >&2
    exit 1
fi
echo "==> headless_shell baked at: $HEADLESS_SHELL"

# --- headless-chrome.service ---
#
# Type=simple: headless_shell stays in the foreground, no PIDFile, no
# notify protocol. The shim considers the unit active immediately after
# fork+exec; readiness (CDP accepting on :9222) is checked by users with
# `curl http://localhost:9222/json/version` exactly as before.
#
# CHROME_REMOTE_PORT (default 9222) and CHROME_FLAGS are env knobs the
# user can pass via `bhatti create --env`. The shell's $VAR expansion
# happens inside the shim's `/bin/sh -c "exec ..."` wrapper, so unset
# CHROME_FLAGS expands cleanly to empty.
cat > "$MOUNT/etc/systemd/system/headless-chrome.service" << UNIT
[Unit]
Description=Headless Chromium with Chrome DevTools Protocol
After=network.target

[Service]
Type=simple
# Defaults; user-supplied values in /run/bhatti/config-env override.
Environment=CHROME_REMOTE_PORT=9222
Environment=CHROME_FLAGS=
EnvironmentFile=-/run/bhatti/config-env
ExecStart=$HEADLESS_SHELL --no-sandbox --disable-gpu --disable-dev-shm-usage --remote-debugging-port=\${CHROME_REMOTE_PORT} --remote-debugging-address=0.0.0.0 \$CHROME_FLAGS
Restart=on-failure
RestartSec=2s

[Install]
WantedBy=multi-user.target
UNIT

ln -sf /etc/systemd/system/headless-chrome.service \
    "$MOUNT/etc/systemd/system/multi-user.target.wants/headless-chrome.service"

# --- Drop the legacy init.sh path entirely ---
rm -f "$MOUNT/etc/bhatti/init.sh"

echo "==> Browser tier done."
