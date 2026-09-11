#!/usr/bin/env bash
# Build a private experimental desktop image; never publish it automatically.
set -euo pipefail
DESKTOP_RECIPE=$(cd "$(dirname "$0")" && pwd)
DESKTOP_IMAGE=$(realpath -m "${1:?Usage: prepare-image.sh NEW.ext4 LINUX_FORGE_BINARY}")
DESKTOP_FORGE=$(realpath "${2:?Supply a Linux x86_64 forge binary}")
[[ -f $DESKTOP_FORGE ]] || exit 1
"$DESKTOP_RECIPE/../../experiments/gpu/desktop/prepare-image.sh" "$DESKTOP_IMAGE"
DESKTOP_MOUNT=$(mktemp -d)
trap 'rmdir "$DESKTOP_MOUNT"' EXIT
unshare --mount bash -se -- "$DESKTOP_IMAGE" "$DESKTOP_MOUNT" "$DESKTOP_RECIPE" "$DESKTOP_FORGE" <<'INNER'
mount --make-rprivate /
mount -o loop "$1" "$2"
trap 'umount -R "$2"' EXIT
install -m755 "$3/init.sh" "$2/init.krun"
install -m755 "$3/session.sh" "$2/usr/local/bin/desktop-session"
install -m755 "$3/relay.py" "$2/usr/local/bin/desktop-relay"
install -m755 "$4" "$2/usr/local/bin/ahvm-forge"
install -m644 "$3/hyprland.lua" "$2/home/desktop/hyprland.lua"
INNER
