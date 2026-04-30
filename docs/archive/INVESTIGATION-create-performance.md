# Investigation: Sandbox Create Performance

*Measured on raspi-5a (Pi 5, ARM64, NVMe WD SN530), April 26 2026.*
*bhatti v1.8.4 (dev build with instrumentation), FC v1.14.0, kernel 6.1.155*

---

## Method

Added `slog.Debug` instrumentation to six phases of the create pipeline
(commit `57a232b`). All logging is at Debug level — invisible at default
INFO level. Enabled via `BHATTI_LOG_LEVEL=debug` systemd override.

Phases instrumented:
1. Engine Create path — `create.phase` messages at every step
2. WaitReady — per-attempt timing split into TCP connect vs AUTH vs exec
3. Jailed FC startup — jailer mkdir, hardlink, chown, socket ready
4. Lohar boot timing — writes `/tmp/boot-timing.txt` inside guest
5. Server handler — request parse, spec build, volume resolve, engine call, DB store
6. Mutex contention — timing around engine lock acquisition

Binaries built locally (`go build`) and deployed to Pi via scp. No
release required for iteration.

---

## Finding 1: btrfs reflink eliminates storage bottleneck

Tested on ext4 (Pi's default) vs btrfs (20GB loopback image with
`compress=zstd:1,noatime`). Same NVMe, same data.

```
Phase                ext4        btrfs       Savings
─────────────────    ─────       ─────       ───────
rootfs_copy          320ms        14ms       306ms
lohar_inject         604ms        95ms       509ms
config_drive            9ms        13ms         —
─────────────────    ─────       ─────       ───────
Total create        2131ms      1240ms       891ms
```

rootfs copy drops from 320ms to 14ms (btrfs reflink = instant CoW
clone). lohar injection drops from 604ms to 95ms (loop mount + cp +
umount is faster on btrfs).

Setup:
```bash
sudo fallocate -l 20G /var/lib/bhatti-btrfs.img
sudo mkfs.btrfs -f /var/lib/bhatti-btrfs.img
sudo mount -o loop,noatime,compress=zstd:1 /var/lib/bhatti-btrfs.img /var/lib/bhatti
```

Rollback: unmount btrfs, restore ext4 backup.

---

## Finding 2: WaitReady TCP connect is 1005ms — one SYN retransmit

The TCP connect in WaitReady succeeds on attempt #1 but takes ~1005ms.
Split timing reveals:

```
connect_ms = 1005    (TCP handshake)
auth_ms    = 0       (AUTH frame)
exec_ms    = 18      (exec true + response)
```

The 1005ms is a single TCP SYN retransmission timeout.

### Root cause: first SYN arrives before guest is ready

Timeline (all times relative to host create start):

```
  0ms     Host: create() starts
 14ms     Host: rootfs copy done (btrfs reflink)
106ms     Host: lohar inject done
154ms     Host: config drive done
175ms     Host: FC jailer started, socket ready
206ms     Host: InstanceStart API returns
206ms     Host: WaitReady starts → TCP SYN sent to guest IP
243ms     Guest: kernel first instruction (measured via btime)
324ms     Guest: lohar tcp_listen (kernel 81ms + lohar 4ms)
          ... SYN was already sent at 206ms, dropped (guest not listening) ...
          ... Linux default SYN retransmit timeout: 1 second ...
~1206ms   Host: kernel retransmits SYN
~1211ms   Guest: SYN-ACK → handshake completes
1229ms    Host: WaitReady done, create complete
```

### How this was verified

1. **Guest boot epoch vs host create start:**
   ```
   Guest btime (from /proc/stat):  epoch 1777194284.441s
   Host create start:              epoch 1777194284.198s
   Delta: guest booted 243ms after host create started
   ```

2. **Lohar boot timing** (`/tmp/boot-timing.txt` inside guest):
   ```
   +0ms start
   +0ms mounts_done
   +1ms lo_up
   +3ms config_applied
   +3ms network_done
   +4ms tcp_listen
   ```
   Lohar is ready in 4ms after starting. Kernel boots in 77ms
   (from dmesg). Total: guest TCP-ready at ~81ms after kernel start.

3. **Guest dmesg confirms fast kernel boot:**
   ```
   0.000s  Linux version 6.1.155
   0.037s  virtio_blk discovered
   0.074s  IP-Config: Complete (eth0 configured)
   0.076s  EXT4-fs (vda): mounted root
   0.077s  Run /usr/local/bin/lohar as init process
   0.089s  Config drive mounted + unmounted
   ```
   No i8042 on ARM. Carrier wait is only 30ms (vs 520ms on x86).

4. **Post-create TCP connect is fast** (proves guest is reachable):
   ```bash
   # Immediately after create returns:
   timeout 0.1 bash -c "echo | nc -w1 $IP 1024"
   # → SUCCESS in 105ms
   ```

5. **FC process startup is 18ms** (from FC metrics):
   ```
   api_server.process_startup_time_us: 18352
   ```

### The SYN retransmit timer

Linux's initial SYN retransmit timeout is 1 second (controlled by
`/proc/sys/net/ipv4/tcp_syn_retries` and hardcoded initial RTO in
the kernel TCP stack). When the first SYN gets no response (guest
not listening yet), the kernel waits 1 second before retrying.

The first SYN is sent at 206ms (when WaitReady starts). The guest
becomes TCP-ready at ~324ms. The SYN was already sent and dropped.
The retransmit fires at ~1206ms. Total wait: 1005ms.

---

## Finding 3: FC's claimed "< 125ms boot time" measures differently

Firecracker measures boot time using a `--boot-timer` flag and a
special init binary (`resources/overlay/usr/local/bin/init.c`) that
writes byte `123` to a magic MMIO address. FC's `BootTimer` device
(`src/vmm/src/devices/pseudo/boot_timer.rs`) records the timestamp.

This measures: **vCPU first instruction → guest init writes to MMIO**.
It does NOT include:
- FC VMM setup before vCPU starts
- Time for guest networking to become reachable from the host
- Any agent readiness polling

Their measurement is valid for what it measures — raw kernel boot
speed. But it's not comparable to bhatti's create time, which
includes host-side prep, FC launch, kernel boot, agent readiness,
and the TCP handshake.

---

## Summary: where create time goes (Pi 5, btrfs)

```
Phase                Time    What
─────────────────    ─────   ────
rootfs_copy           14ms   btrfs reflink clone
lohar_inject          93ms   loop mount + cp + umount
network               34ms   IP alloc + TAP + ARP flush
config_drive          14ms   mke2fs -d
fc_start (jailer)     21ms   chroot + FC process + socket
fc_api + InstanceStart 31ms  HTTP PUTs + boot API call
                     ─────
Host work total:     207ms

TCP SYN wait:       1005ms   First SYN dropped, 1s retransmit
AUTH + exec:          18ms   Agent responds
                     ─────
WaitReady total:    1023ms

DB store:              5ms   SQLite insert
                     ─────
TOTAL:              1235ms
```

Host-side work is 207ms. The remaining 1023ms is a single TCP SYN
retransmit timeout because the first probe arrives 118ms before the
guest is ready.

---

## What this means for optimization

The bottleneck is not FC, not the kernel, not lohar, not the host-side
prep. It's the timing mismatch: WaitReady sends its first TCP SYN
~118ms before the guest listener is up, and Linux's 1-second SYN
retransmit timer penalizes this by a full second.

Possible approaches (not yet tested):
- Delay WaitReady by ~200ms after InstanceStart (skip the doomed first SYN)
- Use a faster probe method (raw SYN with shorter timeout, or UDP)
- Reduce the host kernel's initial SYN RTO (`ip route` can set per-route RTO)
- Use vsock instead of TCP for the initial probe (if vsock works on cold boot)
- Have FC signal readiness via a different channel (MMIO, ioeventfd)
