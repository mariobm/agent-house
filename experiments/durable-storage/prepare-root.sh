#!/bin/sh
# Disposable Linux x86_64 qualification image, not a product image.
set -eu
if [ "$#" -ne 2 ]; then
    echo "Usage: $0 NEW_OUTPUT_DIRECTORY STATIC_FORGE_BINARY" >&2
    exit 2
fi
root=$(realpath -m "$1")
forge=$(realpath "$2")
[ "$(id -u)" -eq 0 ] || { echo "root required" >&2; exit 1; }
[ -f "$forge" ] || exit 1
mkdir -m 700 "$root"
curl -fsSL https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-minirootfs-3.22.1-x86_64.tar.gz -o "$root/root.tar.gz"
curl -fsSL https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-minirootfs-3.22.1-x86_64.tar.gz.sha256 -o "$root/root.sha256"
cd "$root"
sed 's/alpine-minirootfs-3.22.1-x86_64.tar.gz/root.tar.gz/' root.sha256 | sha256sum -c -
mkdir tree
tar -xzf root.tar.gz -C tree
cp /etc/resolv.conf tree/etc/resolv.conf
chroot tree /sbin/apk add --no-cache python3 py3-pip git e2fsprogs
mkdir -p tree/usr/local/bin tree/workspace tree/proc tree/sys tree/dev tree/run
cp "$forge" tree/usr/local/bin/ahvm-forge
cat > tree/init.krun <<'INIT'
#!/bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/root USER=root LANG=C.UTF-8 TERM=xterm-256color
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /dev/shm /tmp /workspace
mount -t devpts devpts /dev/pts
chmod 1777 /tmp
cd /workspace
exec /usr/local/bin/ahvm-forge
INIT
chmod 755 tree/init.krun
truncate -s 512M root.ext4
mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 -d tree root.ext4
du -h root.ext4
