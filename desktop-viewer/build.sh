#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
npm --prefix web ci --ignore-scripts
npm --prefix web run build
cargo build --release --locked
mkdir -p dist
cp target/release/ahvm-desktop dist/ahvm-desktop
cp web/node_modules/@novnc/novnc/LICENSE.txt dist/noVNC-LICENSE.txt
cp web/node_modules/@novnc/novnc/AUTHORS dist/noVNC-AUTHORS
cp web/node_modules/@novnc/novnc/vendor/pako/LICENSE dist/pako-LICENSE
if [[ -f web/dist/client.js.LEGAL.txt ]]; then cp web/dist/client.js.LEGAL.txt dist/JavaScript-NOTICES.txt; fi
cp ../LICENSE dist/AHVM-LICENSE
if [[ $(uname -s) == Darwin ]]; then
    bundle='dist/AHVM Desktop.app/Contents'
    mkdir -p "$bundle/MacOS" "$bundle/Resources"
    cp target/release/ahvm-desktop "$bundle/MacOS/ahvm-desktop"
    cp dist/*LICENSE* dist/*AUTHORS "$bundle/Resources/"
    python3 - "$bundle/Info.plist" <<'PY'
import plistlib,sys
with open(sys.argv[1],'wb') as f:
    plistlib.dump(dict(CFBundleExecutable='ahvm-desktop',CFBundleIdentifier='app.ahvm.desktop',
        CFBundleName='AHVM Desktop',CFBundleDisplayName='AHVM Desktop',CFBundlePackageType='APPL',
        CFBundleShortVersionString='0.2.1',CFBundleVersion='1',NSHighResolutionCapable=True),f)
PY
fi
