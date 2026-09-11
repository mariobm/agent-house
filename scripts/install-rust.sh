#!/usr/bin/env bash
# Fresh installation only. Does not replace the Go installation or existing data.
# Usage: install-rust.sh BUNDLE [--prefix PATH] [--config-dir PATH] [--data-dir PATH]
#        [--unit-name NAME] [--user USER] [--no-start]
set -euo pipefail
umask 077
BUNDLE=${1:?Usage: install-rust.sh BUNDLE [options]}; shift
PREFIX=/opt/ahvm-rust
CONFIG=/etc/ahvm-rust
DATA=/var/lib/ahvm-rust
UNIT=ahvm-rust
RUN_USER=ahvm-rust
START=1
while (($#)); do
    case "$1" in
        --prefix) PREFIX=${2:?}; shift 2 ;;
        --config-dir) CONFIG=${2:?}; shift 2 ;;
        --data-dir) DATA=${2:?}; shift 2 ;;
        --unit-name) UNIT=${2:?}; shift 2 ;;
        --user) RUN_USER=${2:?}; shift 2 ;;
        --no-start) START=0; shift ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done
[[ $(uname -s) == Linux && $(uname -m) == x86_64 && $EUID == 0 ]] || { echo 'Requires root on Linux x86_64' >&2; exit 1; }
[[ $UNIT =~ ^[a-zA-Z0-9_-]+$ && $RUN_USER =~ ^[a-z_][a-z0-9_-]*$ ]] || { echo 'Invalid unit/user name' >&2; exit 1; }
for path in "$PREFIX" "$CONFIG" "$DATA"; do
    [[ $path =~ ^/[a-zA-Z0-9_./-]+$ && $path != *'/../'* && $path != */.. && $path != / ]] || { echo 'Use absolute paths without spaces or traversal' >&2; exit 1; }
    [[ ! -e $path && ! -L $path ]] || { echo "Refusing existing installation/state: $path" >&2; exit 1; }
done
UNIT_FILE=/etc/systemd/system/$UNIT.service
[[ ! -e $UNIT_FILE ]] || { echo "Unit exists: $UNIT_FILE" >&2; exit 1; }
[[ -c /dev/kvm ]] || { echo '/dev/kvm is required' >&2; exit 1; }
getent group kvm >/dev/null || { echo 'kvm group is required' >&2; exit 1; }
command -v python3 >/dev/null
command -v systemctl >/dev/null
BUNDLE=$(realpath "$BUNDLE")
(cd "$BUNDLE" && sha256sum --quiet --strict -c SHA256SUMS)
[[ -x $BUNDLE/bin/ahvm && -s $BUNDLE/share/base.ext4 ]] || { echo 'Incomplete bundle' >&2; exit 1; }
# Resolve DNS from the host; a local stub is reachable by the host gateway.
RESOLVER=${AHVM_DNS_RESOLVER:-$(awk '$1=="nameserver" && $2 ~ /^[0-9.]+$/ {print $2; exit}' /etc/resolv.conf)}
python3 - "$RESOLVER" <<'PY'
import ipaddress,sys
ipaddress.IPv4Address(sys.argv[1])
PY
if ! id "$RUN_USER" >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --shell /usr/sbin/nologin "$RUN_USER"
fi
getent group "$RUN_USER" >/dev/null || { echo 'Service user requires a matching primary group' >&2; exit 1; }
# Grant only the render-node group, never the display/card device group.
if [[ -x $BUNDLE/bin/ahvm-vmm-gpu ]]; then
    for node in /dev/dri/renderD*; do
        [[ -c $node ]] || continue
        group=$(stat -c %G "$node")
        [[ $group != root && $group != UNKNOWN ]] || continue
        usermod -a -G "$group" "$RUN_USER"
    done
fi
install -d -m755 "$PREFIX"
cp -a "$BUNDLE/." "$PREFIX/"
chown -R root:root "$PREFIX"
chmod -R go-w "$PREFIX"
chmod 755 "$PREFIX"
# Catch inaccessible ancestors/images before registering a broken service.
for file in bin/ahvm-daemon bin/ahvm-vmm bin/ahvm-netd lib/libkrunfw.so.5 share/base.ext4; do
    runuser -u "$RUN_USER" -- test -r "$PREFIX/$file" || { echo "Service cannot read $PREFIX/$file" >&2; exit 1; }
done
install -d -m700 "$CONFIG"
install -d -m700 -o "$RUN_USER" -g "$RUN_USER" "$DATA"
python3 - "$CONFIG" "$PREFIX" "$DATA" "$RESOLVER" <<'PY'
from pathlib import Path
import secrets,sys
config,prefix,data,dns=sys.argv[1:]
p=Path(config)
token=secrets.token_hex(32)
(p/'admin.token').write_text(token+'\n'); (p/'admin.token').chmod(0o600)
(p/'private-access.json').write_text('{}\n')
(p/'daemon.env').write_text(f'''AHVM_LISTEN=127.0.0.1:8080
AHVM_DATA_DIR={data}
AHVM_VMM_BIN={prefix}/bin/ahvm-vmm
AHVM_NETD_BIN={prefix}/bin/ahvm-netd
AHVM_BASE_IMAGE={prefix}/share/base.ext4
AHVM_LIB={prefix}/lib
AHVM_DNS_RESOLVER={dns}
AHVM_ADMIN_TOKEN={token}
AHVM_PRIVATE_ACCESS_FILE={config}/private-access.json
AHVM_PREVIEW_LISTEN=127.0.0.1:8081
AHVM_PREVIEW_DOMAIN=preview.localhost
AHVM_MAX_CONCURRENT_OPS=4
AHVM_IDLE_SECS=3600
''')
(p/'daemon.env').chmod(0o600)
PY
# Policy must be readable by the service, but writable only by the administrator.
chown root:"$RUN_USER" "$CONFIG" "$CONFIG/private-access.json"
chmod 750 "$CONFIG"
chmod 640 "$CONFIG/private-access.json"
python3 - "$PREFIX/packaging/ahvm-rust.service.in" "$UNIT_FILE" "$PREFIX" "$CONFIG" "$DATA" "$RUN_USER" <<'PY'
from pathlib import Path
import sys
source,dest,prefix,config,data,user=sys.argv[1:]
s=Path(source).read_text()
for key,val in [('PREFIX',prefix),('CONFIG',config),('DATA',data),('USER',user)]: s=s.replace('@'+key+'@',val)
Path(dest).write_text(s)
PY
systemd-analyze verify "$UNIT_FILE"
systemctl daemon-reload
if ((START)); then systemctl enable --now "$UNIT.service"; fi
printf 'Installed. CLI: %s/bin/ahvm\nToken file: %s/admin.token (root-only)\nConfig: %s/daemon.env\nService: %s.service\n' "$PREFIX" "$CONFIG" "$CONFIG" "$UNIT"
