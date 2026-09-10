#!/usr/bin/env bash
# AHVM early-access installer. Served at https://ahvm.app/install.sh.
# The function is invoked only after the whole script has been received.
set -euo pipefail
main() {
    umask 022
    local prefix=/opt/ahvm-rust config=/etc/ahvm-rust data=/var/lib/ahvm-rust
    local unit=ahvm-rust run_user=ahvm-rust bin_dir=/usr/local/bin no_start=0
    local manifest_url=${AHVM_MANIFEST_URL:-https://ahvm.app/releases/early-access.json}
    while (($#)); do
        case "$1" in
            --prefix) prefix=${2:?}; shift 2 ;;
            --config-dir) config=${2:?}; shift 2 ;;
            --data-dir) data=${2:?}; shift 2 ;;
            --bin-dir) bin_dir=${2:?}; shift 2 ;;
            --unit-name) unit=${2:?}; shift 2 ;;
            --user) run_user=${2:?}; shift 2 ;;
            --no-start) no_start=1; shift ;;
            --help|-h) printf 'Usage: curl -fsSL https://ahvm.app/install.sh | bash\nOptions: --prefix PATH --config-dir PATH --data-dir PATH --bin-dir PATH --unit-name NAME --user NAME --no-start\nFresh Linux x86_64/KVM/systemd installation; requires glibc 2.35+.\n'; return ;;
            *) printf 'Unknown option: %s\n' "$1" >&2; return 1 ;;
        esac
    done
    [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || { echo 'This server build requires Linux x86_64. See https://ahvm.app/docs/ for platform support.' >&2; return 1; }
    for tool in curl python3 tar sha256sum systemctl getconf; do
        command -v "$tool" >/dev/null || { printf 'Missing prerequisite: %s\n' "$tool" >&2; return 1; }
    done
    [[ -c /dev/kvm && -d /run/systemd/system ]] || { echo 'Requires a Linux host with /dev/kvm and systemd running.' >&2; return 1; }
    python3 - "$(getconf GNU_LIBC_VERSION)" <<'PY'
import sys
try:
    name, version = sys.argv[1].split()
    assert name == 'glibc' and tuple(map(int,version.split('.'))) >= (2,35)
except (ValueError, AssertionError):
    raise SystemExit('This build requires glibc 2.35+ (Ubuntu 22.04 / Debian 12 or newer).')
PY
    local path
    for path in "$prefix" "$config" "$data" "$bin_dir"; do
        [[ $path =~ ^/[a-zA-Z0-9_./-]+$ && $path != *'/../'* && $path != */.. && $path != / ]] || { echo 'Use absolute paths without spaces or traversal.' >&2; return 1; }
    done
    [[ $unit =~ ^[a-zA-Z0-9_-]+$ && $run_user =~ ^[a-z_][a-z0-9_-]*$ ]] || { echo 'Invalid service/user name.' >&2; return 1; }
    for path in "$prefix" "$config" "$data" "/etc/systemd/system/$unit.service" "$bin_dir/ahvm"; do
        [[ ! -e $path && ! -L $path ]] || { printf 'Refusing existing installation path: %s\n' "$path" >&2; return 1; }
    done
    local tmp
    tmp=$(mktemp -d)
    local cleanup
    printf -v cleanup '%q' "$tmp"
    trap "rm -rf -- $cleanup" EXIT
    # Localhost is only for the automated installer test harness.
    fetch() {
        case "$1" in
            https://*) curl --proto '=https' --proto-redir '=https' --tlsv1.2 -fsSL --connect-timeout 15 --max-time 600 "$1" -o "$2" ;;
            http://127.0.0.1:*) curl --proto '=http' --max-redirs 0 -fsS --connect-timeout 5 --max-time 600 "$1" -o "$2" ;;
            *) echo 'Downloads require HTTPS.' >&2; return 1 ;;
        esac
    }
    echo 'AHVM / early access — downloading a verified Linux server bundle.'
    fetch "$manifest_url" "$tmp/manifest.json"
    python3 - "$tmp/manifest.json" "$tmp" <<'PY'
import json,re,sys
from pathlib import Path
p=Path(sys.argv[1]); dest=Path(sys.argv[2])
if p.stat().st_size>16384: raise SystemExit('Invalid release manifest size')
m=json.loads(p.read_text())
if m.get('platform')!='linux-x86_64': raise SystemExit('Unsupported release platform')
if not re.fullmatch(r'[0-9a-f]{64}',m.get('sha256','')): raise SystemExit('Invalid SHA-256 digest')
if not re.fullmatch(r'[a-zA-Z0-9._-]+',m.get('version','')): raise SystemExit('Invalid release version')
if m.get('repository') != 'mariobm/agent-house': raise SystemExit('Unexpected release repository')
if not re.fullmatch(r'v[0-9a-zA-Z._-]+',m.get('tag','')): raise SystemExit('Invalid release tag')
if not re.fullmatch(r'ahvm-[0-9a-zA-Z._-]+\.tar\.gz',m.get('asset','')): raise SystemExit('Invalid release asset')
(dest/'tag').write_text(m['tag'])
(dest/'asset').write_text(m['asset'])
(dest/'bundle.sha256').write_text(m['sha256']+'  bundle.tar.gz\n')
print('Build:',m['version'])
PY
    fetch "https://github.com/mariobm/agent-house/releases/download/$(cat "$tmp/tag")/$(cat "$tmp/asset")" "$tmp/bundle.tar.gz"
    (cd "$tmp" && sha256sum -c bundle.sha256)
    mkdir "$tmp/bundle"
    # Reject traversal, links and special files before extraction. Our release
    # archive intentionally contains only regular files and directories.
    python3 - "$tmp/bundle.tar.gz" <<'PY'
import sys,tarfile
from pathlib import PurePosixPath
with tarfile.open(sys.argv[1],'r:gz') as t:
    for member in t:
        p=PurePosixPath(member.name)
        if p.is_absolute() or '..' in p.parts or not (member.isfile() or member.isdir()):
            raise SystemExit('Unsafe archive entry: '+member.name)
PY
    tar -xzf "$tmp/bundle.tar.gz" -C "$tmp/bundle" --no-same-owner --no-same-permissions
    (cd "$tmp/bundle" && sha256sum --strict -c SHA256SUMS)
    local elevate=()
    if (( EUID != 0 )); then
        command -v sudo >/dev/null || { echo 'Run as root or install sudo.' >&2; return 1; }
        elevate=(sudo)
        echo 'Installation requires sudo to create the service and its protected configuration.'
        sudo -v
    fi
    # install-rust.sh repeats fresh-path checks after privilege elevation.
    "${elevate[@]}" bash "$tmp/bundle/install.sh" "$tmp/bundle" --prefix "$prefix" --config-dir "$config" --data-dir "$data" --unit-name "$unit" --user "$run_user" --no-start
    "${elevate[@]}" mkdir -p "$bin_dir"
    "${elevate[@]}" ln -s "$prefix/bin/ahvm" "$bin_dir/ahvm"
    if (( ! no_start )); then
        "${elevate[@]}" systemctl enable --now "$unit"
    fi
    printf '\nInstalled AHVM. Try:\n  sudo %s/ahvm --token-file %s/admin.token health\n\nDocs: https://ahvm.app/docs/\n' "$bin_dir" "$config"
    rm -rf -- "$tmp"
    trap - EXIT
}
main "$@"
