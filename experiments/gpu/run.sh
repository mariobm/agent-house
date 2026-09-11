#!/usr/bin/env bash
# Run one standalone 2-vCPU / 4-GiB probe VM, outside the daemon.
set -euo pipefail
GPU_IMAGE=$(realpath "${1:?Usage: AHVM_VMM_BIN=/path/ahvm-vmm run.sh PREPARED.ext4}")
GPU_VMM=$(realpath "${AHVM_VMM_BIN:?Set AHVM_VMM_BIN to the experimental GPU build}")
GPU_RUN=$(mktemp -d)
trap 'rm -rf "$GPU_RUN"' EXIT
python3 - "$GPU_IMAGE" "$GPU_RUN/vm.json" <<'PY'
import json,sys
from pathlib import Path
Path(sys.argv[2]).write_text(json.dumps({
    'vcpus':2,'mem_mib':4096,'gpu':True,'log_level':2,
    'root_disk':sys.argv[1],'root_disk_format':'raw','pid1':True,
    'exec_path':'/init.krun','env':['PATH=/usr/sbin:/usr/bin:/sbin:/bin']
}))
PY
LD_LIBRARY_PATH="${LD_LIBRARY_PATH:-/opt/ahvm-rust/lib}" timeout --kill-after=5s 60s "$GPU_VMM" "$GPU_RUN/vm.json" 2>&1 | tee "$GPU_RUN/console.log"
grep -q 'GPU_RENDER_OK ' "$GPU_RUN/console.log"
grep -q 'GPU_PROBE_EXIT=0' "$GPU_RUN/console.log"
