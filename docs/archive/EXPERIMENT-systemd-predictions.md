# Experiment Predictions: systemd as PID 1

Written before running the experiment. Compare against actual results
to calibrate understanding.

---

## Phase 1: Boot timing

### Current baseline (measured)

```
Host-side (rootfs copy → FC API calls):    ~130ms
Kernel boot:                                ~125ms
Lohar init (mounts → TCP listen):           ~28ms
WaitReady probing:                          ~80ms
────────────────────────────────────────────────
Total create p50:                           365ms
```

The WaitReady probe pattern uses escalating timeouts: 100ms → 100ms →
100ms → 250ms, with 10ms sleep between. Currently the agent is ready
~55ms after InstanceStart. The first or second probe catches it.

### With systemd

Host-side and kernel are identical — same rootfs copy, same binary,
same kernel image. The only change is what happens after the kernel
starts PID 1.

```
systemd PID 1 parses units:                ~15-25ms
systemd mounts proc/sys/dev/tmpfs/cgroup:  ~10-15ms
  (batched, parallel — faster than lohar's sequential mounts)
systemd reaches basic.target:              ~10-20ms
systemd forks lohar.service:               ~5-10ms
lohar runAsAgent() config drive + listen:  ~15-25ms
────────────────────────────────────────────────
Total PID 1 to agent ready:                ~55-95ms
```

vs lohar's current 28ms. Delta: **+30-65ms inside the VM.**

But the WaitReady impact isn't just the delta. It depends on where the
agent-ready moment falls in the probe cycle:

```
Current:   agent ready at ~55ms after InstanceStart
           → caught by probe 2 (fires at ~110ms, agent already listening)

systemd:   agent ready at ~85-120ms after InstanceStart
           → still caught by probe 2 or 3
           → worst case: probe 2 gets RST, probe 3 catches at ~220ms
```

### Prediction

| Metric | Current | Predicted systemd | Delta |
|--------|---------|-------------------|-------|
| Create p50 | 365ms | **420-470ms** | +55-105ms |
| Create p95 | ~417ms | **480-550ms** | +65-135ms |
| Guest init time | 28ms | **70-95ms** | +42-67ms |
| systemd-analyze kernel | N/A | **~80-100ms** | — |
| systemd-analyze userspace | N/A | **~50-90ms** | — |
| systemd-analyze total | N/A | **~140-180ms** | — |

**Confidence: medium-high.** The main uncertainty is which WaitReady
probe bucket catches the agent. If it slips from probe 2 to probe 3,
that's an extra 110ms step. If it stays in probe 2's window, the delta
is just the raw init time difference.

**What would surprise me:** If create p50 exceeds 550ms, something
unexpected is happening — maybe a systemd generator we didn't mask is
running, or journald startup is blocking the target. That would be
investigatable by running `systemd-analyze blame` inside the VM.

---

## Phase 2: Snapshot/restore

### Why I expect this to work

Firecracker snapshot/restore saves and restores the full CPU state
including the TSC (Time Stamp Counter). When the VM resumes:

- **CLOCK_MONOTONIC continues from the snapshot value.** It does NOT
  jump. systemd's internal timers (service watchdogs, restart intervals,
  rate-limiting) all use CLOCK_MONOTONIC. They see continuity, not a gap.

- **CLOCK_REALTIME jumps forward** (kvmclock syncs with host). Calendar
  timers would fire. But we've masked every timer unit in the rootfs
  (apt-daily, fstrim, man-db, motd-news, e2scrub). No calendar timers
  exist to misfire.

- **lohar.service has `WatchdogSec=0`.** Even if the monotonic clock
  had a hiccup, the watchdog won't kill lohar.

- **journald** sees the realtime jump and might write a few log entries
  ("system resumed" or similar). With `Storage=volatile` and
  `RuntimeMaxUse=8M`, this is bounded and harmless.

### Prediction

| Test | Expected result |
|------|----------------|
| Background process survives snapshot/restore | **Yes** — memory snapshot preserves process state regardless of init |
| DNS works after restore | **Yes** — static resolv.conf (or configured resolved) persists |
| Network works after restore | **Yes** — virtio-net survives, kernel TCP stack intact |
| systemctl is-active lohar | **active** — lohar was running when snapshotted |
| systemctl is-system-running | **running** or **degraded** — degraded if a masked unit tried to start |
| journalctl shows errors | **No errors**, maybe a "system resumed" informational line |
| Restore latency delta vs Image A | **<10ms** — the restore path doesn't re-run PID 1, it restores memory |

**Confidence: high, now backed by Firecracker's own docs.** Found the
answer directly in `docs/snapshotting/snapshot-support.md` (updated
literally last week with VMClock info):

> *"a microVM that is restored from snapshot will be resumed with the
> guest OS wall-clock continuing from the moment of the snapshot
> creation. For this reason, the wall-clock should be updated to the
> current time, on the guest-side."*

This confirms:

1. **The wall-clock (CLOCK_REALTIME) freezes at snapshot time and
   does NOT auto-advance on restore.** The guest wakes up thinking
   it's still the time when it was snapshotted. This is BETTER for
   systemd than a sudden jump — systemd sees zero time discontinuity
   on resume. Calendar timers won't fire because the clock hasn't
   advanced.

2. **The clock_realtime option in LoadSnapshot** can optionally advance
   the clock on restore (x86_64 only, host Linux ≥ 5.16). We do NOT
   use this — our `/snapshot/load` payload doesn't include
   `clock_realtime`. So the clock stays frozen.

3. **On x86_64 with kvm-clock**, Firecracker injects a KVM-clock
   notification when pausing (warm transition). On ARM64 (Pi 5),
   kvm-clock is not used — the kernel uses the ARM generic timer
   (arch_timer), which reads the physical counter directly. This
   counter DOES advance while vCPUs are paused (it's a hardware
   timer, not a vCPU instruction counter). So on ARM64 warm resume,
   CLOCK_MONOTONIC will jump by the pause duration.

4. **VMClock device** (new, merged in Linux v7.0) provides userspace
   notification of clock disruptions via `vm_generation_counter`. Our
   kernel (6.1) doesn't have this. Not a concern for the experiment.

**What this means for systemd:**

- **Cold restore (snapshot/load):** Wall-clock frozen. systemd sees
  zero time change. No timer storm, no watchdog trigger, no issues.
  This is the safest case.

- **Warm resume on x86_64 (kvm-clock):** Firecracker sends a clock
  notification on pause. The guest adjusts. systemd may see a
  CLOCK_REALTIME update but CLOCK_MONOTONIC behavior depends on
  kvm-clock implementation. Low risk — kvm-clock is designed for
  this.

- **Warm resume on ARM64 (Pi 5, arch_timer):** The physical counter
  advances during pause. CLOCK_MONOTONIC jumps by 30-300 seconds.
  systemd will notice. Timers based on CLOCK_MONOTONIC will see the
  gap. Since we've masked all timer units and set WatchdogSec=0,
  the only impact is systemd's internal accounting (e.g., rate
  limiting logs). **This is the thing to watch in the experiment.**

**What would surprise me:** If `systemctl is-system-running` returns
"maintenance" (emergency mode) after restore. That would indicate
systemd detected a critical failure during resume.

---

## Phase 3: Thermal warm→hot

### The mechanics

Warm = vCPUs paused via Firecracker's `PATCH /vm {"state":"Paused"}`.
The TSC doesn't advance while vCPUs are paused. When resumed, the TSC
picks up from its paused value.

The pause duration for warm is typically 30-300 seconds. This creates
a CLOCK_REALTIME jump of that magnitude. CLOCK_MONOTONIC may or may not
jump depending on kvmclock behavior.

### Prediction

| Test | Expected result |
|------|----------------|
| Exec after warm resume | **Works, same latency as Image A** |
| systemctl is-system-running | **running** |
| Any service restarts triggered | **No** — no watchdogs, masked timers |

**Confidence: high, with one ARM64 caveat.** On the Pi 5, the ARM
generic timer (arch_timer) is a hardware counter that advances even
while vCPUs are paused. So CLOCK_MONOTONIC WILL jump by the pause
duration (30-300s for warm). systemd will see this. Since we've masked
all timers and watchdogs, the likely impact is zero — but this is the
second thing to watch in the experiment (after cold restore).

For comparison: laptop sleep/resume produces the same CLOCK_MONOTONIC
jump, and systemd handles it routinely. The difference is that on real
hardware, NTP corrects the realtime clock immediately. In a Firecracker
VM, there's no NTP — the Firecracker FAQ recommends PTP via kvm-clock,
but on ARM64 this isn't available in the same way.

---

## Phase 4: Memory overhead

### What's running in systemd mode

With our stripped config:
- `systemd` (PID 1): ~8-12 MB RSS
- `systemd-journald`: ~6-10 MB RSS (volatile, 8MB cap)
- `dbus-daemon`: ~3-5 MB RSS (systemd needs it for IPC)
- `lohar`: ~10-15 MB RSS (same as today)

Masked (NOT running): resolved, networkd, timesyncd, logind, udevd,
all gettys, all timers.

### Prediction

| Metric | Image A (lohar PID 1) | Image B (systemd) | Delta |
|--------|----------------------|-------------------|-------|
| Total system RSS | ~15-20 MB | ~35-45 MB | **+18-27 MB** |
| Free memory (2048MB VM) | ~2020 MB | ~1995 MB | **-25 MB (~1.2%)** |
| Process count | ~3-5 | ~8-12 | +5-7 |

**Confidence: high.** These are well-documented systemd memory profiles.
The journald cap at 8MB is the main variable — it might use less if
there's little logging.

**What would surprise me:** If RSS exceeds 60 MB total. That would
mean an unmasked service is running (udevd, networkd, or similar).
Check with `systemctl list-units --state=running`.

---

## Phase 5: Package install

### The resolved question

This is the one prediction that depends on a design choice we make in
the rootfs build.

**If we mask resolved and pin-exclude it from apt:**
systemd-resolved is never installed. `/etc/resolv.conf` stays as our
static file. All packages that depend on `systemd` (not `systemd-resolved`)
install fine. sshd starts. postgresql starts. DNS works via static config.

**If we enable resolved with configured DNS:**
resolved runs, manages `/etc/resolv.conf`, provides DNS caching. Better
long-term but more moving parts for the experiment. The config drive
DNS settings would need to configure resolved instead of writing a
static file.

**For the experiment, I recommend: mask resolved, pin-exclude it.**
Simplest path to the numbers. We can enable it later if we ship.

### Prediction

| Package | Image A (lohar PID 1) | Image B (systemd, resolved masked) |
|---------|----------------------|-------------------------------------|
| `openssh-server` | ❌ DNS breaks (resolved postinst clobbers resolv.conf) | **✅ installs, sshd starts, DNS intact** |
| `postgresql` | ⚠️ installs, pg doesn't start (systemctl fails) | **✅ installs, pg starts, pg_isready works** |
| `nginx` | ⚠️ installs, nginx doesn't start | **✅ installs, nginx starts, curl localhost works** |
| `redis-server` | ⚠️ installs, redis doesn't start | **✅ installs, redis starts, redis-cli ping works** |

**Wait — correction on openssh-server with Image B:**

Even with resolved masked, `apt-get install openssh-server` might pull
`systemd-resolved` as a *Recommends* dependency. If installed, its
postinst creates the `/etc/resolv.conf → ../run/systemd/resolve/stub-resolv.conf`
symlink regardless of whether the service is masked.

To prevent this, the rootfs needs one of:
```bash
# Option 1: apt pin to block resolved entirely
echo 'Package: systemd-resolved
Pin: release *
Pin-Priority: -1' > /etc/apt/preferences.d/no-resolved

# Option 2: install with --no-install-recommends
apt-get install --no-install-recommends openssh-server

# Option 3: mark resolv.conf as immutable
chattr +i /etc/resolv.conf
```

With the apt pin in place (done in rootfs build), openssh-server
installs without pulling resolved. sshd starts via systemctl. DNS stays
as our static file. **This is the expected happy path.**

**Confidence: medium-high.** The apt pin is the key — without it,
resolved's postinst will still break DNS even with systemd running.
This needs to be verified in the experiment.

---

## Phase 6: Exec + file regression

### Why there should be zero regression

The hot path is:
```
host CLI → HTTP API → TCP to guest → lohar agent → fork+exec → collect output → TCP back
```

Every component in this chain is identical between Image A and B. The
agent protocol is the same. The TCP listeners are the same. The exec
syscalls are the same. The file read/write is the same.

The only possible interference: systemd background processes (journald
writing, dbus message passing) competing for CPU or I/O. On a 2-vCPU
VM with 2048 MB, this is negligible.

### Prediction

| Metric | Image A | Image B | Delta |
|--------|---------|---------|-------|
| Exec 'true' p50 | ~25ms | **~25ms** | **<2ms** |
| Exec 'echo' p50 | ~27ms | **~27ms** | **<2ms** |
| File read 1KB p50 | ~5ms | **~5ms** | **<1ms** |
| File write 1KB p50 | ~8ms | **~8ms** | **<1ms** |
| 10 concurrent execs | ~200ms | **~200ms** | **<10ms** |

**Confidence: very high.** The hot path is completely unchanged.

**What would surprise me:** If any exec metric regresses by >5ms at
p50. That would indicate journald or another systemd component is
doing I/O at the wrong time. Fixable by tuning journal settings.

---

## Phase 7: Integration tests

### Prediction

All 15+ integration tests pass. Specific ones to watch:

| Test | Prediction | Risk |
|------|-----------|------|
| TestCreateWithoutTemplate | **Pass** | None — create path works |
| TestEnsureHotExecFromWarm | **Pass** | Low — warm resume is clock-safe |
| TestEnsureHotExecFromCold | **Pass** | Medium — snapshot/restore with systemd |
| TestFileInjection | **Pass** | None — config drive is identical |
| TestInitScript | **Pass** | Low — boot profile might run slightly later |
| TestInitSessionAttach | **Pass** | Low — init session is same mechanism |
| TestCheckpointAndResume | **Pass** | Medium — snapshot with systemd state |
| TestCheckpointWithVolume | **Pass** | Same as above plus volumes |

**Confidence: high overall.** The medium-risk items are the snapshot
tests. If CLOCK_MONOTONIC does jump on Firecracker restore (which I
don't believe it does, but haven't verified), systemd might react.

---

## Summary: what I expect to find

| Dimension | Impact | Ship-blocking? |
|-----------|--------|---------------|
| Boot time | **+60-100ms** (365→425-465ms) | **No.** Under 500ms is fast. |
| Snapshot/restore | **Works** (CLOCK_MONOTONIC is continuous) | Only if it doesn't work. |
| Warm resume | **No impact** | No. |
| Memory | **+20-25 MB** (~1% of 2048MB) | No. |
| Exec/file latency | **No measurable regression** | No. |
| Package install | **Everything works** (with resolved pinned out) | No. |
| Integration tests | **All pass** | Only if they don't. |

**Net assessment:** The experiment should show a ~80ms boot regression
and significant package compatibility improvement, with no regression
in any other dimension. The boot regression is within the margin that
matters for interactive use (nobody notices 80ms) and well within the
margin for AI agent use (agents wait for responses, not boot).

**What I now know from Firecracker's docs (found during research):**

The wall-clock behavior on snapshot/restore is explicitly documented:
the guest clock freezes at snapshot time and does NOT auto-advance.
We don't pass `clock_realtime: true` in our LoadSnapshot call, so the
guest wakes up thinking no time has passed. This is the safest case
for systemd — no time jump at all on cold restore.

The nuance is ARM64 warm resume: the arch_timer hardware counter
advances while vCPUs are paused, so CLOCK_MONOTONIC does jump on
warm→hot. systemd handles this the same way it handles laptop
sleep/resume (which produces the same pattern). With masked timers
and WatchdogSec=0, this should be benign.

**The one thing I could be wrong about:** systemd's early boot being
slower than estimated on ARM64/Pi 5. The 50-90ms userspace estimate
comes from x86_64 Firecracker CI numbers. ARM64 may be different due
to lower single-thread IPC. If systemd-analyze shows >200ms userspace,
something is wrong with our masking — `systemd-analyze blame` will
show the culprit.

**The second thing I could be wrong about:** The ARM64 warm resume
CLOCK_MONOTONIC jump. If systemd reacts to a 30-second monotonic
jump by doing something expensive (re-evaluating all unit
dependencies, restarting rate-limited services), the warm resume
could add latency. The experiment will show this directly.
