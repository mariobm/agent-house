# POC: systemd as PID 1

Get systemd mode booting on raspi-5a, measure everything, compare
against lohar-as-PID-1. No cleanup, no polish — just prove it works
and get numbers.

**Target machine:** `ssh user@100.119.145.44` (raspi-5a, aarch64, Pi 5)

---

## Current State (measured on raspi-5a)

```
ahvm version:     dev
kernel:             6.8.0-1052-raspi (host), vmlinux-arm64 (guest, 6.1.155)
rootfs:             rootfs-minimal-arm64.ext4 (512MB, 161MB used, 291MB free)
firecracker:        jailer mode (uid 10000)
images:             minimal, browser, docker, computer (all arm64)
systemd in rootfs:  NOT INSTALLED (minbase debootstrap)
lohar init time:    9ms (mounts → TCP listen)
```

The minimal rootfs was built with `debootstrap --variant=minbase`
which strips systemd entirely. We need to install it for the POC.

---

## Approach

Build a new rootfs image (`rootfs-systemd-arm64.ext4`) alongside the
existing one. Modify lohar to detect PID 1 vs agent mode. Switch
`init=` in boot args based on image name. Run the bench suite on both.

**Nothing is deleted or replaced.** Existing images, existing code
paths, existing tests — all untouched.

---

## Part 1 — Lohar dual-mode (`cmd/lohar/main.go`)

Add a `runAsAgent()` function. The existing PID 1 path stays exactly
as-is — the new code only runs when `os.Getpid() != 1`.

```go
func main() {
    if os.Getenv("LOHAR_TEST") == "1" {
        runTestMode()
        return
    }

    // NEW: if not PID 1, systemd started us — skip init duties
    if os.Getpid() != 1 {
        runAsAgent()
        return
    }

    // ... existing PID 1 path, completely unchanged ...
}
```

`runAsAgent()` does:
1. Read config drive (/dev/vdb) — same code as PID 1 path
2. Apply token, env, files, volumes — same code
3. Verify networking (kernel ip= already configured eth0)
4. Start vsock + TCP listeners — same code
5. Write /tmp/boot-timing.txt — same code
6. Run boot profile if present — same code
7. Run --init session if configured — same code
8. Block forever — same code

What it skips:
- Mount proc/sys/dev/devpts/tmpfs/shm/cgroup2 (systemd did this)
- Bring up loopback (systemd did this)
- Set hostname (systemd reads /etc/hostname)
- Signal handlers (systemd handles SIGTERM for us)

Cross-compile and deploy:
```bash
# On mac (this repo)
cd /Users/sahil/Projects/ahvm
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o lohar-arm64 ./cmd/lohar/
scp lohar-arm64 user@100.119.145.44:/tmp/
```

---

## Part 2 — Systemd rootfs image (on raspi-5a)

Build directly on the Pi. No cross-compile needed — native arm64.

```bash
ssh user@100.119.145.44

# 1. Copy the existing minimal rootfs as our base
sudo cp /var/lib/ahvm/images/rootfs-minimal-arm64.ext4 \
        /var/lib/ahvm/images/rootfs-systemd-arm64.ext4

# 2. Grow it to 1GB (systemd + deps need space, plus room for apt-get later)
sudo truncate -s 1G /var/lib/ahvm/images/rootfs-systemd-arm64.ext4
sudo e2fsck -f /var/lib/ahvm/images/rootfs-systemd-arm64.ext4
sudo resize2fs /var/lib/ahvm/images/rootfs-systemd-arm64.ext4

# 3. Mount and install systemd
sudo mkdir -p /mnt/systemd-rootfs
sudo mount /var/lib/ahvm/images/rootfs-systemd-arm64.ext4 /mnt/systemd-rootfs

# Bind-mount for chroot
sudo mount --bind /proc /mnt/systemd-rootfs/proc
sudo mount --bind /sys /mnt/systemd-rootfs/sys
sudo mount --bind /dev /mnt/systemd-rootfs/dev
sudo mount --bind /dev/pts /mnt/systemd-rootfs/dev/pts

# 4. Install systemd + dbus (required by systemd)
sudo chroot /mnt/systemd-rootfs /bin/bash -c '
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends systemd systemd-sysv dbus

# Create lohar.service
cat > /etc/systemd/system/lohar.service << UNIT
[Unit]
Description=AHVM Guest Agent
After=sysinit.target
DefaultDependencies=no

[Service]
Type=simple
ExecStart=/usr/local/bin/lohar
Restart=always
RestartSec=1
RefuseManualStop=yes
WatchdogSec=0

[Install]
WantedBy=multi-user.target
UNIT

systemctl enable lohar.service

# Mask everything unnecessary
systemctl mask systemd-resolved.service
systemctl mask systemd-networkd.service
systemctl mask systemd-timesyncd.service
systemctl mask systemd-logind.service
systemctl mask systemd-udevd.service
systemctl mask e2scrub_reap.timer
systemctl mask apt-daily.timer
systemctl mask apt-daily-upgrade.timer
systemctl mask fstrim.timer
systemctl mask motd-news.timer
systemctl mask man-db.timer 2>/dev/null || true
systemctl mask serial-getty@.service
systemctl mask console-getty.service
systemctl mask getty@.service

# Pin-exclude systemd-resolved from apt
# Prevents openssh-server from pulling it in and breaking DNS
cat > /etc/apt/preferences.d/no-resolved << PIN
Package: systemd-resolved
Pin: release *
Pin-Priority: -1
PIN

# Journal: volatile only, small cap
mkdir -p /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/ahvm.conf << JCONF
[Journal]
Storage=volatile
RuntimeMaxUse=8M
JCONF

# Disable watchdogs (snapshot/restore causes time jumps)
mkdir -p /etc/systemd/system.conf.d
cat > /etc/systemd/system.conf.d/ahvm.conf << SCONF
[Manager]
RuntimeWatchdogSec=0
ShutdownWatchdogSec=0
DefaultTimeoutStartSec=10s
DefaultTimeoutStopSec=5s
SCONF

apt-get clean
rm -rf /var/lib/apt/lists/*
'

# 5. Inject the new dual-mode lohar
sudo cp /tmp/lohar-arm64 /mnt/systemd-rootfs/usr/local/bin/lohar
sudo chmod 755 /mnt/systemd-rootfs/usr/local/bin/lohar

# 6. Unmount
sudo umount /mnt/systemd-rootfs/dev/pts
sudo umount /mnt/systemd-rootfs/dev
sudo umount /mnt/systemd-rootfs/sys
sudo umount /mnt/systemd-rootfs/proc
sudo umount /mnt/systemd-rootfs

# 7. Write lohar hash stamp so the engine doesn't try to re-inject
sha256sum /tmp/lohar-arm64 | awk '{print $1}' | \
    sudo tee /var/lib/ahvm/images/rootfs-systemd-arm64.ext4.lohar-sha256
```

---

## Part 3 — Engine: init= switch (`pkg/engine/firecracker/create.go`)

Minimal change — detect "systemd" in the image path and switch init:

```go
initBin := "/usr/local/bin/lohar"
if strings.Contains(rootfsPath, "systemd") {
    initBin = "/sbin/init"
}

bootArgs := fmt.Sprintf(
    "reboot=k panic=1 pci=off 8250.nr_uarts=0 init=%s quiet loglevel=0 ip=...",
    initBin, guestIP, userNet.GatewayIP)
```

Cross-compile and deploy:
```bash
# On mac
GOOS=linux GOARCH=arm64 CGO_ENABLED=0 go build -o ahvm-arm64 ./cmd/ahvm/
scp ahvm-arm64 user@100.119.145.44:/tmp/

# On Pi
ssh user@100.119.145.44
sudo systemctl stop ahvm
sudo cp /tmp/ahvm-arm64 /usr/local/bin/ahvm
sudo systemctl start ahvm
```

---

## Part 4 — Smoke test

```bash
ssh user@100.119.145.44

# A. Does systemd mode boot?
ahvm create --name smoke-systemd --image systemd --cpus 1 --memory 2048

# B. Is lohar responsive?
ahvm exec smoke-systemd -- echo "hello from systemd mode"

# C. What does boot timing look like?
ahvm file read smoke-systemd /tmp/boot-timing.txt

# D. Is systemd running?
ahvm exec smoke-systemd -- systemctl is-system-running
ahvm exec smoke-systemd -- systemctl status lohar.service
ahvm exec smoke-systemd -- systemd-analyze

# E. What's masked?
ahvm exec smoke-systemd -- systemctl list-units --state=running

# F. DNS works?
ahvm exec smoke-systemd -- cat /etc/resolv.conf
ahvm exec smoke-systemd -- curl -s ifconfig.me

# G. Compare with lohar PID 1
ahvm create --name smoke-lohar --cpus 1 --memory 2048
ahvm file read smoke-lohar /tmp/boot-timing.txt
ahvm exec smoke-lohar -- echo "hello from lohar PID 1"

# Clean up
ahvm destroy smoke-systemd -y
ahvm destroy smoke-lohar -y
```

If smoke test passes, proceed to measurements.

---

## Part 5 — Measurements

### 5a. Boot timing (20 creates each, sequential, destroy between)

```bash
echo "=== LOHAR PID 1 ===" | tee boot-lohar.txt
for i in $(seq 1 20); do
    START=$(date +%s%N)
    ahvm create --name "bl-$i" --cpus 1 --memory 2048
    END=$(date +%s%N)
    MS=$(( (END - START) / 1000000 ))
    GUEST=$(ahvm file read "bl-$i" /tmp/boot-timing.txt 2>/dev/null)
    echo "create $i: ${MS}ms | guest: $GUEST" | tee -a boot-lohar.txt
    ahvm destroy "bl-$i" -y
    sleep 1
done

echo "=== SYSTEMD ===" | tee boot-systemd.txt
for i in $(seq 1 20); do
    START=$(date +%s%N)
    ahvm create --name "bs-$i" --image systemd --cpus 1 --memory 2048
    END=$(date +%s%N)
    MS=$(( (END - START) / 1000000 ))
    GUEST=$(ahvm file read "bs-$i" /tmp/boot-timing.txt 2>/dev/null)
    ANALYZE=$(ahvm exec "bs-$i" -- systemd-analyze 2>/dev/null | head -1)
    echo "create $i: ${MS}ms | guest: $GUEST | $ANALYZE" | tee -a boot-systemd.txt
    ahvm destroy "bs-$i" -y
    sleep 1
done
```

### 5b. Snapshot/restore

```bash
for mode in "" "--image systemd"; do
    tag=$([ -z "$mode" ] && echo "lohar" || echo "systemd")
    echo "=== $tag snapshot test ===" | tee snap-$tag.txt

    ahvm create --name "snap-$tag" --cpus 1 --memory 2048 $mode
    ahvm exec "snap-$tag" -- sh -c 'nohup sh -c "while true; do date >> /tmp/tick.log; sleep 1; done" &'
    sleep 3
    ahvm exec "snap-$tag" -- wc -l /tmp/tick.log

    ahvm stop "snap-$tag"
    sleep 5

    START=$(date +%s%N)
    ahvm start "snap-$tag"
    END=$(date +%s%N)
    MS=$(( (END - START) / 1000000 ))
    echo "resume: ${MS}ms" | tee -a snap-$tag.txt

    ahvm exec "snap-$tag" -- cat /etc/resolv.conf | tee -a snap-$tag.txt
    ahvm exec "snap-$tag" -- wc -l /tmp/tick.log | tee -a snap-$tag.txt
    [ "$tag" = "systemd" ] && ahvm exec "snap-$tag" -- systemctl is-system-running 2>&1 | tee -a snap-$tag.txt
    [ "$tag" = "systemd" ] && ahvm exec "snap-$tag" -- journalctl --no-pager -n 10 2>&1 | tee -a snap-$tag.txt

    ahvm destroy "snap-$tag" -y
done
```

### 5c. Warm resume

```bash
for mode in "" "--image systemd"; do
    tag=$([ -z "$mode" ] && echo "lohar" || echo "systemd")
    echo "=== $tag warm test ===" | tee warm-$tag.txt

    ahvm create --name "warm-$tag" --cpus 1 --memory 2048 $mode
    ahvm exec "warm-$tag" -- echo "warmup"
    echo "waiting 35s for warm transition..."
    sleep 35

    START=$(date +%s%N)
    ahvm exec "warm-$tag" -- echo "after warm"
    END=$(date +%s%N)
    MS=$(( (END - START) / 1000000 ))
    echo "warm resume+exec: ${MS}ms" | tee -a warm-$tag.txt

    [ "$tag" = "systemd" ] && ahvm exec "warm-$tag" -- systemctl is-system-running 2>&1 | tee -a warm-$tag.txt

    ahvm destroy "warm-$tag" -y
done
```

### 5d. Memory

```bash
for mode in "" "--image systemd"; do
    tag=$([ -z "$mode" ] && echo "lohar" || echo "systemd")
    echo "=== $tag memory ===" | tee mem-$tag.txt

    ahvm create --name "mem-$tag" --cpus 1 --memory 2048 $mode
    ahvm exec "mem-$tag" -- free -m | tee -a mem-$tag.txt
    ahvm exec "mem-$tag" -- ps -eo pid,rss,comm --sort=-rss | head -15 | tee -a mem-$tag.txt

    ahvm destroy "mem-$tag" -y
done
```

### 5e. Package install (the payoff)

```bash
for mode in "" "--image systemd"; do
    tag=$([ -z "$mode" ] && echo "lohar" || echo "systemd")
    echo "=== $tag packages ===" | tee pkg-$tag.txt

    ahvm create --name "pkg-$tag" --cpus 1 --memory 2048 --disk-size 4096 $mode
    ahvm exec "pkg-$tag" -- sudo apt-get update -qq 2>&1 | tail -3

    # openssh-server (the issue #12 case)
    ahvm exec "pkg-$tag" -- sudo apt-get install -y --no-install-recommends openssh-server 2>&1 | tail -5 | tee -a pkg-$tag.txt
    ahvm exec "pkg-$tag" -- cat /etc/resolv.conf | tee -a pkg-$tag.txt
    echo "dns: $(ahvm exec "pkg-$tag" -- curl -sf ifconfig.me 2>&1 || echo FAILED)" | tee -a pkg-$tag.txt
    [ "$tag" = "systemd" ] && echo "sshd: $(ahvm exec "pkg-$tag" -- sudo systemctl is-active ssh 2>&1)" | tee -a pkg-$tag.txt

    # postgresql
    ahvm exec "pkg-$tag" -- sudo apt-get install -y --no-install-recommends postgresql 2>&1 | tail -5 | tee -a pkg-$tag.txt
    echo "pg: $(ahvm exec "pkg-$tag" -- sudo pg_isready 2>&1)" | tee -a pkg-$tag.txt

    # nginx
    ahvm exec "pkg-$tag" -- sudo apt-get install -y --no-install-recommends nginx 2>&1 | tail -5 | tee -a pkg-$tag.txt
    echo "nginx: $(ahvm exec "pkg-$tag" -- curl -sf localhost 2>&1 | head -1 || echo FAILED)" | tee -a pkg-$tag.txt

    ahvm destroy "pkg-$tag" -y
done
```

### 5f. Exec regression (bench suite)

```bash
# Image A
ahvm create --name perf-bench --cpus 2 --memory 2048
bash bench/run.sh 20
cp -r bench/results bench/results-lohar
ahvm destroy perf-bench -y

# Image B
ahvm create --name perf-bench --image systemd --cpus 2 --memory 2048
bash bench/run.sh 20
cp -r bench/results bench/results-systemd
ahvm destroy perf-bench -y
```

---

## Implementation Order

```
1. Edit cmd/lohar/main.go — add runAsAgent()         [mac, 15 min]
2. Cross-compile lohar, scp to Pi                     [mac, 2 min]
3. Edit create.go — init= switch                      [mac, 5 min]
4. Cross-compile ahvm, scp to Pi                    [mac, 2 min]
5. Build systemd rootfs on Pi (chroot + apt)          [Pi, 15 min]
6. Restart ahvm on Pi                               [Pi, 1 min]
7. Smoke test (Part 4)                                [Pi, 5 min]
8. Boot timing measurements (Part 5a)                 [Pi, 15 min]
9. Snapshot/restore test (Part 5b)                    [Pi, 10 min]
10. Warm resume test (Part 5c)                         [Pi, 5 min]
11. Memory comparison (Part 5d)                        [Pi, 5 min]
12. Package install test (Part 5e)                     [Pi, 15 min]
13. Full bench suite both images (Part 5f)             [Pi, 30 min]
14. Compile results, update predictions doc            [mac, 15 min]
```

Total: ~2.5 hours. All reversible — the systemd rootfs is a new file,
lohar's PID 1 path is untouched, the init= switch is 3 lines.
