#!/usr/bin/env bash
# One 2-vCPU / 4-GiB VM, private host Unix socket, bounded lifetime.
set -euo pipefail
DESKTOP_IMAGE=$(realpath "${1:?Usage: AHVM_VMM_BIN=... run.sh PREPARED.ext4 OUTPUT_DIR}")
DESKTOP_OUTPUT=$(realpath -m "${2:?Supply an output directory outside the repository}")
DESKTOP_VMM=$(realpath "${AHVM_VMM_BIN:?Set AHVM_VMM_BIN to the experimental GPU build}")
DESKTOP_RECIPE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$DESKTOP_OUTPUT"
DESKTOP_RUN=$(mktemp -d)
worker=
cleanup() {
    if [[ -n $worker ]]; then kill "$worker" 2>/dev/null || true; wait "$worker" 2>/dev/null || true; fi
    rm -rf "$DESKTOP_RUN"
}
trap cleanup EXIT
python3 - "$DESKTOP_IMAGE" "$DESKTOP_RUN" <<'PY'
import json,sys
from pathlib import Path
p=Path(sys.argv[2])
(p/'vm.json').write_text(json.dumps(dict(vcpus=2,mem_mib=4096,gpu=True,
    root_disk=sys.argv[1],root_disk_format='raw',pid1=True,exec_path='/init.krun',
    vsock_forward_uds=str(p/'vnc.sock'),log_level=2,
    env=['PATH=/usr/sbin:/usr/bin:/sbin:/bin'])))
PY
LD_LIBRARY_PATH="${LD_LIBRARY_PATH:-/opt/ahvm-rust/lib}" timeout --kill-after=5s 45s \
    "$DESKTOP_VMM" "$DESKTOP_RUN/vm.json" > "$DESKTOP_OUTPUT/console.log" 2>&1 &
worker=$!
# Guest starts the bridge only after the compositor, terminal and capture server.
sleep 7
python3 "$DESKTOP_RECIPE/rfb-probe.py" "$DESKTOP_RUN/vnc.sock" "$DESKTOP_OUTPUT/vnc"
wait "$worker"
worker=
grep -q 'DESKTOP_INPUT_OK' "$DESKTOP_OUTPUT/console.log"
grep -q 'DESKTOP_POINTER_OK' "$DESKTOP_OUTPUT/console.log"
grep -q 'Renderer: virgl (' "$DESKTOP_OUTPUT/console.log"
if grep -Eq 'panicked at|BUG: kernel|llvmpipe|softpipe' "$DESKTOP_OUTPUT/console.log"; then
    echo 'Desktop boot contains a panic or software renderer' >&2
    exit 1
fi
echo 'DESKTOP_CAPTURE_INPUT_OK'
