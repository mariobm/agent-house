#!/usr/bin/env bash
# Native Linux x86_64 build; produces a complete Rust runtime + minimal guest.
# Requires Rust + musl target, cc/musl-gcc, clang, pkg-config, libkrunfw.so.5,
# libzstd-dev, patchelf, curl, bzip2, make, readelf, mkfs.ext4 and Python 3.
# AHVM_FW_DIR defaults to /usr/local/lib64. Output must not already exist.
set -euo pipefail
umask 022
cd "$(dirname "$0")/.."
[[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || { echo 'Currently qualified for Linux x86_64 only' >&2; exit 1; }
OUT=${1:?Usage: package-rust.sh OUTPUT_DIRECTORY}
OUT=$(realpath -m "$OUT")
[[ ! -e $OUT ]] || { echo "Refusing existing output: $OUT" >&2; exit 1; }
FW_DIR=$(realpath "${AHVM_FW_DIR:-/usr/local/lib64}")
[[ -f $FW_DIR/libkrunfw.so.5 ]] || { echo 'Missing libkrunfw.so.5' >&2; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH"
export LIBRARY_PATH="$FW_DIR${LIBRARY_PATH:+:$LIBRARY_PATH}"
export LD_LIBRARY_PATH="$FW_DIR${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export CC_LINUX=${CC_LINUX:-cc}
export BUILD_JOBS=${BUILD_JOBS:-2}
cargo build --manifest-path rust/Cargo.toml --release --locked -j "${BUILD_JOBS:-2}" \
    -p ahvm-cli -p ahvm-daemon -p ahvm-vmm -p ahvm-netd
cargo build --manifest-path rust/Cargo.toml --release --locked -j "${BUILD_JOBS:-2}" \
    -p ahvm-forge --target x86_64-unknown-linux-musl
mkdir -p "$(dirname "$OUT")"
STAGE=$(mktemp -d "${OUT}.build.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE"/{bin,lib,share,packaging}
for name in ahvm ahvm-daemon ahvm-vmm ahvm-netd; do
    install -m755 "rust/target/release/$name" "$STAGE/bin/$name"
    patchelf --set-rpath '$ORIGIN/../lib' "$STAGE/bin/$name"
done
install -m755 rust/target/x86_64-unknown-linux-musl/release/ahvm-forge "$STAGE/bin/ahvm-forge"
# dlopen dependency: ldd cannot discover the bundled guest firmware.
install -m755 "$FW_DIR/libkrunfw.so.5" "$STAGE/lib/libkrunfw.so.5"
patchelf --set-rpath '$ORIGIN' "$STAGE/lib/libkrunfw.so.5"
# Resolve the complete native closure, excluding the host's glibc/loader.
# ldd is used only on binaries we just built, never on a downloaded executable.
python3 - "$STAGE" <<'PY'
from pathlib import Path
import subprocess,sys,shutil
root=Path(sys.argv[1]); pending=list((root/'bin').iterdir())+list((root/'lib').iterdir()); seen=set()
excluded={'libc.so.6','libm.so.6','libpthread.so.0','libdl.so.2','librt.so.1','ld-linux-x86-64.so.2'}
while pending:
    binary=pending.pop()
    if binary.name in seen: continue
    seen.add(binary.name)
    out=subprocess.run(['ldd',str(binary)],text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT).stdout
    if 'not found' in out: raise SystemExit(out)
    for line in out.splitlines():
        parts=line.split()
        if len(parts)<3 or parts[1]!='=>' or not parts[2].startswith('/'): continue
        name,src=parts[0],Path(parts[2])
        if name in excluded: continue
        dest=root/'lib'/name
        if not dest.exists():
            shutil.copy2(src,dest)
            subprocess.run(['patchelf','--set-rpath','$ORIGIN',str(dest)],check=True)
            pending.append(dest)
PY
FORGE_BIN="$STAGE/bin/ahvm-forge" scripts/rust-guest-rootfs.sh "$STAGE/share/base.ext4"
chmod 644 "$STAGE/share/base.ext4"
cp packaging/rust/ahvm-rust.service.in "$STAGE/packaging/"
cp scripts/install-rust.sh "$STAGE/install.sh"
cp docs/RUST-INSTALL.md "$STAGE/README.md"
cp docs/NETWORK-ACCESS.md docs/NETWORK-QUALIFICATION.md docs/FILE-UPLOADS.md LICENSE "$STAGE/"
printf 'platform=linux-x86_64\nglibc=%s\nsource=%s\nfork=%s\n' \
    "$(getconf GNU_LIBC_VERSION)" "${AHVM_SOURCE_REV:-$(git rev-parse HEAD 2>/dev/null || echo source-archive)}" \
    "${AHVM_FORK_REV:-$(git -C libkrucible rev-parse HEAD 2>/dev/null || echo source-archive)}" > "$STAGE/BUILD.txt"
(cd "$STAGE" && find bin lib share packaging -type f -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS && sha256sum install.sh README.md NETWORK-ACCESS.md NETWORK-QUALIFICATION.md FILE-UPLOADS.md LICENSE BUILD.txt >> SHA256SUMS)
chmod 755 "$STAGE"
mv "$STAGE" "$OUT"
echo "Built $OUT. Install with: sudo $OUT/install.sh $OUT"
