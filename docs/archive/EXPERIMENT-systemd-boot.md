# Experiment: systemd as PID 1 — Measured Impact on Pi 5

Goal: get real numbers on every dimension that matters before committing
to or rejecting systemd mode. No guessing, no estimates. A/B comparison
on the same hardware, same kernel, same network, same bench suite.

---

## What we're measuring

| Dimension | Why it matters |
|-----------|---------------|
| Cold boot (create) | The headline number. Currently p50=365ms. |
| Guest-side init time | lohar's 28ms vs systemd userspace. Isolated from host overhead. |
| Snapshot/restore | The unknown. systemd sees a time jump on resume. |
| Warm resume | vCPU unpause. Does systemd react to small time jumps? |
| Exec latency (hot) | Must not regress. Agent protocol is identical. |
| File read/write | Must not regress. |
| Memory overhead | RSS of systemd processes vs lohar-only. |
| Disk usage | Rootfs size difference. |
| Package install | The whole point — does `apt-get install openssh-server` work? |
| Zombie reaping | Does systemd clean up what lohar doesn't? |
| Service supervision | Does `Restart=always` work after crash? |

---

## The two rootfs images

Both built from the **same** minimal tier script, same debootstrap,
same Ubuntu 24.04 Noble base. The only difference is what runs as PID 1.

### Image A: `rootfs-minimal-arm64.ext4` (current, control)

- `init=/usr/local/bin/lohar` on kernel cmdline
- lohar IS PID 1, does all mounts/config/networking
- This is what we ship today. No changes.

### Image B: `rootfs-systemd-arm64.ext4` (experiment)

- `init=/sbin/init` on kernel cmdline (systemd)
- lohar runs as `lohar.service` (Type=simple, Restart=always)
- systemd handles mounts, cgroups, zombie reaping
- Stripped to minimal: only lohar.service + dependencies enabled

---

## Changes required (total: ~4 files, ~80 lines)

### 1. Lohar: agent-only mode (~15 lines in `cmd/lohar/main.go`)

Add a `--agent` flag (or detect `getpid() != 1`) that skips the init
path and only runs the agent listeners + config drive:

```go
func main() {
    if os.Getenv("LOHAR_TEST") == "1" {
        runTestMode()
        return
    }

    // NEW: if not PID 1, skip init duties — systemd already did them
    if os.Getpid() != 1 {
        runAsAgent()
        return
    }

    // ... existing PID 1 init path unchanged ...
}

// NEW function
func runAsAgent() {
    bootStart := time.Now()
    bp := func(name string) {
        fmt.Fprintf(os.Stderr, "lohar: agent +%dms %s\n",
            time.Since(bootStart).Milliseconds(), name)
    }
    bp("start")

    // Config drive: systemd doesn't know about /dev/vdb.
    // Read it here, same as PID 1 mode.
    cfg := loadConfigDrive()
    if cfg != nil {
        if len(cfg.DNS) > 0 {
            applyDNS(cfg.DNS)
        }
        agentToken = cfg.Token
        configEnv = cfg.Env
        writeConfigFiles(cfg.Files)
        // Volumes: systemd mount units or mount here
        mountVolumes(cfg.Volumes)
        syscall.Unmount("/run/bhatti/config", 0)
        os.RemoveAll("/run/bhatti/config")
        bp("config_applied")
    }

    // Networking: kernel ip= already configured eth0.
    // systemd-networkd may have also touched it. Verify it's up.
    setupNetworking()
    bp("network_verified")

    // Listeners — identical to PID 1 mode
    lnControl, err := listenVsock(proto.VsockPortControl)
    if err == nil {
        go acceptLoop(lnControl, handleControlConnection)
    }
    lnForward, err := listenVsock(proto.VsockPortForward)
    if err == nil {
        go acceptLoop(lnForward, handleForwardConnection)
    }
    tcpControl, _ := net.Listen("tcp", fmt.Sprintf("0.0.0.0:%d", proto.VsockPortControl))
    if tcpControl != nil {
        go acceptLoop(tcpControl, handleControlConnection)
    }
    tcpForward, _ := net.Listen("tcp", fmt.Sprintf("0.0.0.0:%d", proto.VsockPortForward))
    if tcpForward != nil {
        go acceptLoop(tcpForward, handleForwardConnection)
    }
    bp("tcp_listen")

    os.WriteFile("/tmp/boot-timing.txt",
        []byte(fmt.Sprintf("agent-mode\n+%dms ready\n",
            time.Since(bootStart).Milliseconds())), 0644)
    fmt.Fprintln(os.Stderr, "lohar: ready (agent mode)")

    // Boot profile + init script (same as PID 1 mode)
    if _, err := os.Stat("/etc/bhatti/init.sh"); err == nil {
        ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
        cmd := exec.CommandContext(ctx, "/bin/sh", "/etc/bhatti/init.sh")
        cmd.Stdout = os.Stderr
        cmd.Stderr = os.Stderr
        cmd.Env = buildEnv(map[string]string{"HOME": "/root"})
        cmd.Run()
        cancel()
    }
    if data, err := os.ReadFile("/run/bhatti/env"); err == nil {
        if configEnv == nil { configEnv = make(map[string]string) }
        for _, line := range strings.Split(string(data), "\n") {
            if k, v, ok := strings.Cut(line, "="); ok && k != "" {
                configEnv[k] = v
            }
        }
    }
    if cfg != nil && cfg.Init != "" {
        go runInitSession(cfg.Init, cfg.User)
    }

    select {} // block forever, same as PID 1 mode
}
```

Key: the PID 1 path is **completely untouched**. Zero risk to existing
behavior. The new code only runs when `getpid() != 1`.

### 2. Rootfs build: systemd variant (~30 lines, new script)

New file: `scripts/tiers/systemd-minimal.sh`

```bash
#!/bin/bash
# systemd-minimal: minimal tier but with systemd as PID 1.
# Builds alongside (not replacing) the normal minimal tier.
set -euo pipefail

# Start from the same minimal base
"$SCRIPT_DIR/tiers/minimal.sh"

echo "==> Configuring systemd mode..."

# 1. Install lohar.service
cat > "$MOUNT/etc/systemd/system/lohar.service" << 'UNIT'
[Unit]
Description=Bhatti Guest Agent
After=network.target
# Start as early as possible — before multi-user.target is fully reached
DefaultDependencies=no
After=sysinit.target

[Service]
Type=simple
ExecStart=/usr/local/bin/lohar
Restart=always
RestartSec=1
# Agent must not be stopped by users
RefuseManualStop=yes
# No watchdog — snapshot/restore causes time jumps
WatchdogSec=0
# Minimal resource limits
MemoryMax=64M

[Install]
WantedBy=multi-user.target
UNIT

# 2. Enable lohar.service
chroot "$MOUNT" systemctl enable lohar.service

# 3. Disable everything we don't need.
#    This is the critical step for boot time.
chroot "$MOUNT" /bin/bash -c '
# Disable services that add boot latency and are unnecessary in a microVM
systemctl mask systemd-resolved.service    # we manage DNS via config drive
systemctl mask systemd-networkd.service    # kernel ip= handles networking
systemctl mask systemd-timesyncd.service   # no NTP needed, time jumps expected
systemctl mask systemd-logind.service      # no login sessions
systemctl mask systemd-udevd.service       # fixed device topology in FC
systemctl mask e2scrub_reap.timer          # no periodic fsck
systemctl mask apt-daily.timer             # no background apt
systemctl mask apt-daily-upgrade.timer
systemctl mask fstrim.timer
systemctl mask motd-news.timer
systemctl mask man-db.timer 2>/dev/null || true

# Disable all getty/console login
systemctl mask serial-getty@.service
systemctl mask console-getty.service
systemctl mask getty@.service

# Reduce journal to memory-only, no persistent log
mkdir -p /etc/systemd/journald.conf.d
cat > /etc/systemd/journald.conf.d/bhatti.conf << JCONF
[Journal]
Storage=volatile
RuntimeMaxUse=8M
JCONF

# Reduce systemd's own overhead
mkdir -p /etc/systemd/system.conf.d
cat > /etc/systemd/system.conf.d/bhatti.conf << SCONF
[Manager]
RuntimeWatchdogSec=0
ShutdownWatchdogSec=0
DefaultTimeoutStartSec=10s
DefaultTimeoutStopSec=5s
SCONF
'

# 4. Hostname and hosts (systemd will set from /etc/hostname)
echo "bhatti" > "$MOUNT/etc/hostname"

echo "==> systemd-minimal tier done."
```

### 3. Engine: boot args switch (~5 lines in `create.go`)

For the experiment, we just need a way to pick which init to use.
Simplest: check if the rootfs image name contains "systemd":

```go
initBin := "/usr/local/bin/lohar"
if strings.Contains(filepath.Base(spec.BaseImage), "systemd") ||
   strings.Contains(filepath.Base(e.cfg.FirecrackerRootfs), "systemd") {
    initBin = "/sbin/init"
}

bootArgs := fmt.Sprintf(
    "reboot=k panic=1 pci=off 8250.nr_uarts=0 init=%s quiet loglevel=0 ip=%s::%s:255.255.255.0::eth0:off:1.1.1.1:8.8.8.8:",
    initBin, guestIP, userNet.GatewayIP)
```

For the experiment, we can also just hardcode it behind an env var:
`BHATTI_SYSTEMD=1 bhatti serve`.

### 4. Build the experiment rootfs on the Pi

```bash
# On the Pi, build both images side by side
cd /var/lib/bhatti

# Image A already exists: rootfs-minimal-arm64.ext4

# Image B: build systemd variant (uses same base, adds systemd config)
sudo SIZE_MB=1024 ./scripts/build-tier.sh systemd-minimal arm64 ./lohar
sudo cp dist/rootfs-systemd-minimal-arm64.ext4 images/
```

The systemd image needs to be bigger (1024 MB vs 512 MB) because
systemd's journal and tmpfiles use some space at runtime.

---

## Test matrix (run on Pi 5, sequential, destroy between creates)

### Phase 1: Boot timing (the headline number)

```bash
# === Baseline (Image A, lohar PID 1) ===
echo "=== BASELINE: lohar PID 1 ===" | tee results-a.txt
for i in $(seq 1 20); do
    name="bench-a-$i"
    START=$(python3 -c 'import time; print(time.monotonic_ns())')
    bhatti create --name "$name" --cpus 1 --memory 2048
    END=$(python3 -c 'import time; print(time.monotonic_ns())')
    MS=$(python3 -c "print(($END - $START) / 1_000_000)")
    echo "create $i: ${MS}ms" | tee -a results-a.txt

    # Grab guest-side boot timing
    bhatti file read "$name" /tmp/boot-timing.txt 2>/dev/null | tee -a results-a.txt
    
    # Grab systemd-analyze if available
    bhatti exec "$name" -- systemd-analyze 2>/dev/null | tee -a results-a.txt || echo "(no systemd)" | tee -a results-a.txt

    bhatti destroy "$name"
    sleep 1
done

# === Experiment (Image B, systemd PID 1) ===
echo "=== EXPERIMENT: systemd PID 1 ===" | tee results-b.txt
for i in $(seq 1 20); do
    name="bench-b-$i"
    START=$(python3 -c 'import time; print(time.monotonic_ns())')
    bhatti create --name "$name" --cpus 1 --memory 2048 --image systemd-minimal
    END=$(python3 -c 'import time; print(time.monotonic_ns())')
    MS=$(python3 -c "print(($END - $START) / 1_000_000)")
    echo "create $i: ${MS}ms" | tee -a results-b.txt

    bhatti file read "$name" /tmp/boot-timing.txt 2>/dev/null | tee -a results-b.txt
    bhatti exec "$name" -- systemd-analyze 2>/dev/null | tee -a results-b.txt
    
    bhatti destroy "$name"
    sleep 1
done
```

**What we learn:** Exact p50/p95 create time delta. Guest-side
`systemd-analyze` shows kernel vs userspace split. `/tmp/boot-timing.txt`
shows when lohar reached ready inside the VM.

### Phase 2: Snapshot/restore (the risk)

```bash
# For each image (A and B):
name="snap-test"
bhatti create --name "$name" --cpus 1 --memory 2048 [--image systemd-minimal]

# Start a long-running process
bhatti exec "$name" -- sh -c 'while true; do date >> /tmp/ticker.log; sleep 1; done &'
bhatti exec "$name" -- sh -c 'echo "pre-snapshot" > /tmp/state.txt'

# Verify process is running
bhatti exec "$name" -- ps aux | grep ticker
bhatti exec "$name" -- wc -l /tmp/ticker.log

# Stop (snapshot to disk)
bhatti stop "$name"
sleep 5

# Resume
START=$(python3 -c 'import time; print(time.monotonic_ns())')
bhatti start "$name"
END=$(python3 -c 'import time; print(time.monotonic_ns())')
echo "resume: $(python3 -c "print(($END - $START) / 1_000_000)")ms"

# Verify everything survived
bhatti exec "$name" -- cat /tmp/state.txt          # should say "pre-snapshot"
bhatti exec "$name" -- ps aux | grep ticker         # should still be running
bhatti exec "$name" -- wc -l /tmp/ticker.log        # should have more lines
bhatti exec "$name" -- cat /etc/resolv.conf         # DNS intact?
bhatti exec "$name" -- curl -s ifconfig.me          # network works?

# For Image B only: check systemd state after resume
bhatti exec "$name" -- systemctl is-active lohar
bhatti exec "$name" -- systemctl is-system-running
bhatti exec "$name" -- journalctl --no-pager -n 20  # any errors?

bhatti destroy "$name"
```

**Repeat with longer gap:** Stop, wait 1 hour, start. Check for
timer storms or watchdog kills.

### Phase 3: Thermal lifecycle (warm → hot → cold → hot)

```bash
name="thermal-test"
bhatti create --name "$name" --cpus 1 --memory 2048 [--image systemd-minimal]
bhatti exec "$name" -- echo "warm up"

# Wait for warm transition (35s)
sleep 35

# Exec on warm sandbox — triggers transparent resume
START=$(python3 -c 'import time; print(time.monotonic_ns())')
bhatti exec "$name" -- echo "after warm"
END=$(python3 -c 'import time; print(time.monotonic_ns())')
echo "warm resume+exec: $(python3 -c "print(($END - $START) / 1_000_000)")ms"

# For Image B: check systemd state after warm resume
bhatti exec "$name" -- systemctl is-system-running
bhatti exec "$name" -- journalctl --no-pager -n 10

bhatti destroy "$name"
```

### Phase 4: Memory overhead

```bash
for img in "" "--image systemd-minimal"; do
    name="mem-test"
    bhatti create --name "$name" --cpus 1 --memory 2048 $img
    
    # Total memory used by system processes
    bhatti exec "$name" -- sh -c 'ps aux --sort=-rss | head -20'
    
    # Specific: systemd + journald + resolved
    bhatti exec "$name" -- sh -c 'ps -eo pid,rss,comm | grep -E "systemd|journal|resolv|lohar" | sort -k2 -n -r'
    
    # Free memory
    bhatti exec "$name" -- free -m
    
    bhatti destroy "$name"
done
```

### Phase 5: Package install (the payoff)

```bash
for img in "" "--image systemd-minimal"; do
    name="pkg-test"
    bhatti create --name "$name" --cpus 1 --memory 2048 --disk-size 4096 $img

    echo "--- Testing with: ${img:-lohar PID 1} ---"
    
    # Test 1: openssh-server (the issue #12 case)
    bhatti exec "$name" -- sudo apt-get update -qq
    bhatti exec "$name" -- sudo apt-get install -y openssh-server 2>&1 | tail -5
    bhatti exec "$name" -- cat /etc/resolv.conf        # DNS still works?
    bhatti exec "$name" -- curl -s ifconfig.me          # network still works?
    bhatti exec "$name" -- sudo systemctl is-active ssh 2>/dev/null || echo "ssh not active"
    
    # Test 2: postgresql
    bhatti exec "$name" -- sudo apt-get install -y postgresql 2>&1 | tail -5
    bhatti exec "$name" -- sudo pg_isready 2>/dev/null || echo "pg not ready"
    
    # Test 3: nginx
    bhatti exec "$name" -- sudo apt-get install -y nginx 2>&1 | tail -5
    bhatti exec "$name" -- curl -s localhost 2>/dev/null | head -1 || echo "nginx not serving"
    
    # Test 4: redis
    bhatti exec "$name" -- sudo apt-get install -y redis-server 2>&1 | tail -5
    bhatti exec "$name" -- redis-cli ping 2>/dev/null || echo "redis not responding"
    
    bhatti destroy "$name"
done
```

### Phase 6: Exec + file ops regression (must not regress)

```bash
# Run the existing bench/run.sh on both images:
# Image A:
bhatti create --name perf-bench --cpus 2 --memory 2048
bash bench/run.sh 30
mv bench/results bench/results-lohar
bhatti destroy perf-bench

# Image B:
bhatti create --name perf-bench --cpus 2 --memory 2048 --image systemd-minimal
bash bench/run.sh 30
mv bench/results bench/results-systemd
bhatti destroy perf-bench

# Diff the results:
for f in bench/results-lohar/*.txt; do
    base=$(basename "$f")
    echo "=== $base ==="
    paste <(sort -n "bench/results-lohar/$base" | awk '{a[NR]=$1} END {printf "lohar p50=%.1f p95=%.1f", a[int(NR*0.5)+1], a[int(NR*0.95)+1]}') \
          <(sort -n "bench/results-systemd/$base" | awk '{a[NR]=$1} END {printf "  systemd p50=%.1f p95=%.1f", a[int(NR*0.5)+1], a[int(NR*0.95)+1]}')
    echo
done
```

### Phase 7: Integration tests (must all pass)

```bash
# Run the existing integration test suite against the systemd image.
# Set the rootfs to the systemd variant:
BHATTI_ROOTFS=/var/lib/bhatti/images/rootfs-systemd-minimal-arm64.ext4 \
    go test ./pkg/engine/firecracker/ -run Integration -v -count=1

# Specifically:
# TestEnsureHotExecFromWarm — warm resume with systemd
# TestEnsureHotExecFromCold — snapshot/restore with systemd
# TestFileInjection — config drive files work
# TestInitScript — boot profile runs
# TestInitSessionAttach — init session attaches
# TestCheckpointAndResume — snapshot round-trip
# TestCheckpointWithVolume — volumes survive snapshot
```

---

## Decision criteria

| Metric | Accept if | Reject if |
|--------|-----------|-----------|
| Create p50 | <550ms (≤50% regression from 365ms) | >600ms |
| Create p95 | <700ms | >800ms |
| Exec 'true' p50 | <5ms regression | >10ms regression |
| File read 1KB p50 | <5ms regression | >10ms regression |
| Snapshot/restore | All services survive, DNS works | Any service dies, DNS breaks |
| Warm resume | Works, <10ms extra latency | systemd timer storm |
| Memory overhead | <80MB extra | >150MB extra |
| Integration tests | All pass | Any fail |
| openssh install | DNS survives, sshd starts | DNS breaks (same as today) |

If all "accept" criteria pass: systemd mode is viable. Ship as
`--image systemd-minimal` first, consider making it the default later.

If any "reject" criterion hits: investigate whether it's fixable
(disable a specific unit, adjust timeouts) before abandoning.

---

## What stays untouched

- **lohar PID 1 code path**: not modified, not deleted. The `if getpid()!=1`
  branch is the only new code. The existing path runs exactly as before.
- **Existing rootfs images**: not modified. The systemd image is a new
  variant built alongside them.
- **Kernel cmdline for existing images**: still `init=/usr/local/bin/lohar`.
  Only the systemd image gets `init=/sbin/init`.
- **All existing tests**: run against the current rootfs first, then
  against the systemd rootfs as a second pass.
- **Config drive format**: identical. The agent reads it the same way
  in both modes.
- **Wire protocol**: identical. The host doesn't know or care what PID 1 is.
- **Thermal manager**: unchanged. It pauses/snapshots the VM the same way.

---

## Execution steps (on the Pi)

```
1. Branch: git checkout -b sahil-systemd-experiment
2. Edit cmd/lohar/main.go — add runAsAgent() function
3. Cross-compile: GOOS=linux GOARCH=arm64 go build -o lohar-arm64 ./cmd/lohar/
4. scp lohar-arm64 to Pi
5. Create scripts/tiers/systemd-minimal.sh
6. Build: sudo SIZE_MB=1024 ./scripts/build-tier.sh systemd-minimal arm64 ./lohar-arm64
7. Copy to images dir
8. Edit create.go — add init= selection (or use env var hack)
9. Rebuild bhatti, restart server
10. Run Phase 1-7 tests
11. Compile results, make decision
```

Estimated time: 3-4 hours (build + test + analysis). No production risk
— everything is a new image alongside existing ones.
