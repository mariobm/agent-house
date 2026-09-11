#!/usr/bin/env bash
# Initial client bootstrap trusts HTTPS distribution, as does this script.
# Subsequent ahvm upgrade calls verify signatures with the embedded public key.
set -euo pipefail
main() {
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
    curl --proto '=https' --proto-redir '=https' -fsSL --connect-timeout 15 --max-time 30 https://images.ahvm.app/catalog.json -o "$tmp/catalog.json"
    python3 - "$tmp" "$platform" <<'PY'
import base64,json,sys,urllib.parse
from pathlib import Path
root=Path(sys.argv[1]); raw=(root/'catalog.json').read_bytes()
if len(raw)>262144: raise SystemExit('Oversized manifest')
catalog=json.loads(base64.b64decode(json.loads(raw)['payload'],validate=True))
a=catalog['cli'][sys.argv[2]]
u=urllib.parse.urlparse(a['url'])
if u.scheme!='https' or u.netloc!='github.com' or not u.path.startswith('/mariobm/agent-house/releases/download/'):
 raise SystemExit('Unexpected client download URL')
(root/'artifact.json').write_text(json.dumps(a))
(root/'url').write_text(a['url'])
PY
    curl --proto '=https' --proto-redir '=https' -fsSL --connect-timeout 15 --max-time 600 "$(cat "$tmp/url")" -o "$tmp/ahvm.gz"
    python3 - "$tmp" <<'PY'
from pathlib import Path
import gzip,hashlib,json,sys
p=Path(sys.argv[1]);a=json.loads((p/'artifact.json').read_text())
with (p/'ahvm.gz').open('rb') as f:
 h=hashlib.sha256()
 for chunk in iter(lambda:f.read(131072),b''): h.update(chunk)
 digest=h.hexdigest()
if (p/'ahvm.gz').stat().st_size!=a['size'] or digest!=a['sha256']: raise SystemExit('Client checksum mismatch')
if not 0<a['unpacked_size']<=268435456: raise SystemExit('Invalid client size')
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
    mv -f "$tmp/ahvm" "$dest/ahvm"
    rm -rf -- "$tmp"
    trap - EXIT
    printf '\nInstalled %s/ahvm\n' "$dest"
    case ":$PATH:" in *":$dest:"*) ;; *) printf 'Add this directory to PATH:\n  export PATH="%s:$PATH"\n' "$dest" ;; esac
    printf '\nNext: ahvm host add home --ssh root@YOUR_SERVER_IP --install\n'
}
main "$@"
