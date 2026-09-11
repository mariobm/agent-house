#!/usr/bin/env bash
# Initial client bootstrap trusts HTTPS distribution, as does this script.
# Subsequent ahvm upgrade calls verify signatures with the embedded public key.
set -euo pipefail
main() {
    if (($#)); then echo "Usage: curl -fsSL https://ahvm.app/install.sh | bash (set AHVM_BIN_DIR to choose a directory)" >&2; return 1; fi
    echo "License terms: https://ahvm.app/license"
    local dest=${AHVM_BIN_DIR:-$HOME/.local/bin} platform tmp
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) platform=darwin-aarch64 ;;
        Darwin-x86_64) platform=darwin-x86_64 ;;
        Linux-x86_64) platform=linux-x86_64 ;;
        *) echo 'No prebuilt AHVM client for this platform.' >&2; return 1 ;;
    esac
    for tool in curl python3 gzip; do command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; return 1; }; done
    mkdir -p "$dest"
    [[ ! -L $dest/ahvm ]] || { echo 'Existing CLI is a symlink; update it with its package manager.' >&2; return 1; }
    tmp=$(mktemp -d "$dest/.ahvm-install.XXXXXX")
    trap "rm -rf -- $(printf '%q' "$tmp")" EXIT
    curl --proto '=https' --proto-redir '=https' -fsSL --connect-timeout 15 --max-time 30 --max-filesize 262144 https://images.ahvm.app/catalog.json -o "$tmp/catalog.json"
    python3 - "$tmp" "$platform" <<'PY'
import base64,json,sys,urllib.parse
from pathlib import Path
root=Path(sys.argv[1]); raw=(root/'catalog.json').read_bytes()
if len(raw)>262144: raise SystemExit('Oversized manifest')
catalog=json.loads(base64.b64decode(json.loads(raw)['payload'],validate=True))
platform=sys.argv[2]
a=catalog.get('client',{}).get(platform)
kind='bundle' if a else 'binary'
a=a or catalog['cli'][platform]
(root/'kind').write_text(kind)
u=urllib.parse.urlparse(a['url'])
if u.scheme!='https' or u.netloc!='github.com' or not u.path.startswith('/mariobm/agent-house/releases/download/'):
 raise SystemExit('Unexpected client download URL')
(root/'artifact.json').write_text(json.dumps(a))
(root/'url').write_text(a['url'])
PY
    curl --proto '=https' --proto-redir '=https' -fsSL --connect-timeout 15 --max-time 600 --max-filesize 268435456 "$(cat "$tmp/url")" -o "$tmp/ahvm.gz"
    python3 - "$tmp" <<'PY'
from pathlib import Path
import gzip,hashlib,json,sys,tarfile,shutil
p=Path(sys.argv[1]);a=json.loads((p/'artifact.json').read_text())
with (p/'ahvm.gz').open('rb') as f:
 h=hashlib.sha256()
 for chunk in iter(lambda:f.read(131072),b''): h.update(chunk)
 digest=h.hexdigest()
if (p/'ahvm.gz').stat().st_size!=a['size'] or digest!=a['sha256']: raise SystemExit('Client checksum mismatch')
if not 0<a['unpacked_size']<=268435456: raise SystemExit('Invalid client size')
if (p/'kind').read_text() == 'bundle':
 allowed={'ahvm','ahvm-desktop','ahvm-desktop-AHVM-LICENSE','ahvm-desktop-noVNC-LICENSE.txt','ahvm-desktop-noVNC-AUTHORS','ahvm-desktop-pako-LICENSE'}
 total=0
 with tarfile.open(p/'ahvm.gz','r|gz') as archive:
  for member in archive:
   if not member.isfile() or member.name not in allowed: raise SystemExit('Unexpected client archive entry')
   total+=member.size
   if total>a['unpacked_size']: raise SystemExit('Oversized client bundle')
   with archive.extractfile(member) as src,(p/member.name).open('xb') as out: shutil.copyfileobj(src,out)
 if total!=a['unpacked_size'] or not (p/'ahvm').is_file(): raise SystemExit('Incomplete client bundle')
else:
 with gzip.open(p/'ahvm.gz','rb') as src,(p/'ahvm').open('wb') as out:
  remaining=a['unpacked_size']
  while remaining:
   block=src.read(min(131072,remaining))
   if not block: raise SystemExit('Incomplete client')
   out.write(block);remaining-=len(block)
  if src.read(1): raise SystemExit('Oversized client')
PY
    chmod 755 "$tmp/ahvm"
    "$tmp/ahvm" --version
    if [[ $(cat "$tmp/kind") == bundle && $platform == darwin-* && ! -f $tmp/ahvm-desktop ]]; then
        echo 'macOS client bundle is missing its viewer.' >&2; return 1
    fi
    for companion in "$tmp"/ahvm-desktop*; do
        [[ -f $companion ]] || continue
        local target="$dest/$(basename "$companion")"
        if [[ -L $target || ( -e $target && ! -f $target ) ]]; then
            echo "Refusing existing companion path: $target" >&2; return 1
        fi
    done
    for companion in "$tmp"/ahvm-desktop*; do
        [[ -f $companion ]] || continue
        [[ $(basename "$companion") != ahvm-desktop ]] || chmod 755 "$companion"
        mv -f "$companion" "$dest/$(basename "$companion")"
    done
    mv -f "$tmp/ahvm" "$dest/ahvm"
    rm -rf -- "$tmp"
    trap - EXIT
    printf '\nInstalled %s/ahvm\n' "$dest"
    case ":$PATH:" in *":$dest:"*) ;; *) printf 'Add this directory to PATH:\n  export PATH="%s:$PATH"\n' "$dest" ;; esac
    printf '\nNext: ahvm host add home --ssh root@YOUR_SERVER_IP --install\n'
}
main "$@"
