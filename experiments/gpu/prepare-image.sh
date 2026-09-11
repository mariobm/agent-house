#!/usr/bin/env bash
# Prepare a disposable Ubuntu development image; never modify the source disk.
set -euo pipefail
[[ $(uname -s) == Linux && $EUID == 0 ]] || { echo 'Requires root on Linux' >&2; exit 1; }
GPU_SOURCE=$(realpath "${1:?Usage: prepare-image.sh SOURCE.ext4 NEW.ext4}")
GPU_OUTPUT=$(realpath -m "${2:?Supply a new output path}")
GPU_RECIPE=$(cd "$(dirname "$0")" && pwd)
[[ -f $GPU_SOURCE && ! -e $GPU_OUTPUT && ! -L $GPU_OUTPUT ]] || { echo 'Source must exist and output must be new' >&2; exit 1; }
mkdir -p "$(dirname "$GPU_OUTPUT")"
cp --reflink=auto --sparse=always "$GPU_SOURCE" "$GPU_OUTPUT"
GPU_MOUNT=$(mktemp -d)
trap 'rmdir "$GPU_MOUNT"' EXIT
unshare --mount bash -se -- "$GPU_OUTPUT" "$GPU_MOUNT" "$GPU_RECIPE" <<'INNER'
mount --make-rprivate /
mount -o loop "$1" "$2"
trap 'umount -R "$2"' EXIT
mount -t proc proc "$2/proc"
mount --rbind /dev "$2/dev"
rm -f "$2/etc/resolv.conf"
cp -L /etc/resolv.conf "$2/etc/resolv.conf"
# All probe dependencies are in Ubuntu main; avoid unrelated large indexes.
sed -i 's/^Components:.*/Components: main/' "$2/etc/apt/sources.list.d/ubuntu.sources"
if [[ -n ${AHVM_GPU_APT_MIRROR:-} ]]; then
    python3 - "$2/etc/apt/sources.list.d/ubuntu.sources" "$AHVM_GPU_APT_MIRROR" <<'PYTHON'
from pathlib import Path
import re,sys,urllib.parse
url=urllib.parse.urlparse(sys.argv[2])
if url.scheme!='https' or not url.netloc or any(c.isspace() for c in sys.argv[2]):
    raise SystemExit('Use an HTTPS Ubuntu mirror URL')
p=Path(sys.argv[1]);p.write_text(re.sub(r'^URIs:.*$', 'URIs: '+sys.argv[2], p.read_text(), flags=re.MULTILINE))
PYTHON
fi
chroot "$2" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get -o Acquire::Languages=none update
chroot "$2" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends gcc libc6-dev libegl-dev libgles-dev libgbm-dev libgl1-mesa-dri
cp "$3/render-probe.c" "$2/tmp/render-probe.c"
chroot "$2" cc -Wall -Wextra /tmp/render-probe.c -o /usr/local/bin/render-probe -lEGL -lGLESv2 -lgbm
install -m755 "$3/init.sh" "$2/init.krun"
INNER
