#!/usr/bin/env bash
# Run in the glibc 2.35 release builder with meson, ninja, EGL/GBM/epoxy dev packages.
set -euo pipefail
prefix=$(realpath -m "${1:?Supply a build prefix}")
if [[ -f $prefix/.virgl-1.2.0 ]]; then exit 0; fi
pkg-config --exists libdrm gbm epoxy
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL --proto '=https' --proto-redir '=https' https://archive.ubuntu.com/ubuntu/pool/main/v/virglrenderer/virglrenderer_1.2.0.orig.tar.bz2 -o "$work/source.tar.bz2"
printf '%s  %s\n' f4f52db11297b52b35c8c2d5bf5e21b7997b52f8bfad99ea2b1c155997cff4ad "$work/source.tar.bz2" | sha256sum -c -
mkdir "$work/src"
tar -xf "$work/source.tar.bz2" -C "$work/src" --strip-components=1
meson setup "$work/build" "$work/src" --prefix="$prefix" --libdir=lib \
    -Dplatforms=egl -Dvenus=false -Dvideo=false -Dtests=false
grep -Eq "^#define ENABLE_GBM( 1)?$" "$work/build/config.h"
grep -Eq "^#define HAVE_EPOXY_EGL_H( 1)?$" "$work/build/config.h"
ninja -C "$work/build" -j "${BUILD_JOBS:-2}"
ninja -C "$work/build" install
install -m644 "$work/src/COPYING" "$prefix/COPYING"
touch "$prefix/.virgl-1.2.0"
