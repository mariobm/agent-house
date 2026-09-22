#!/usr/bin/env bash
# Build a clean image from verified Arch bootstrap and pinned Omarchy sources.
set -euo pipefail
[[ $(uname -s) == Linux && $EUID == 0 ]] || { echo 'Requires root on Linux' >&2; exit 1; }
recipe=$(cd "$(dirname "$0")" && pwd)
image=$(realpath -m "${1:?Usage: build-image.sh NEW.ext4 LINUX_FORGE_BINARY}")
forge=$(realpath "${2:?Supply Linux x86_64 forge binary}")
[[ ! -e $image && ! -L $image && -f $forge ]] || exit 1
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
commit=c668141e9c42b13c80c9ca4ea108e11708c5e8a5
curl -fL --retry 3 -o "$work/source.tar.gz" "https://github.com/omacom/omarchy/archive/$commit.tar.gz"
printf '%s  %s\n' 8cc6b1d9d903c606600395e3b3d80ad9f15bd3ab78f0f19b0f49041b1df82013 "$work/source.tar.gz" | sha256sum -c -
tar -xzf "$work/source.tar.gz" -C "$work"
"$recipe/../arch-desktop/prepare-image.sh" "$work/arch.ext4" "$forge"
"$recipe/prepare-image.sh" "$work/arch.ext4" "$image" "$work/omarchy-$commit"
