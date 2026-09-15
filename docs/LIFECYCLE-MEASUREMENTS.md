# Lifecycle measurements, 2026-09-15

## Result

The replicated path has a measurable readiness delay. In a diagnostic run, an
independent `/bin/true` succeeded 3.035 seconds after the bridge socket appeared,
but the normal create path finished at 5.740 seconds after that point. About
2.7 seconds of that run was avoidable waiting, not necessary guest boot work.
This warrants investigation before investing in a prepared VM pool.

The same Ubuntu image creates quickly with local storage. The initial measurements below preceded the readiness fix. The follow-up section
records the change and new results; storage guarantees and production deployments
remain unchanged.

## Method

- Host: agent_house, Intel i5-12500, Linux 7.0.0-22-generic, x86_64 KVM, 64 GiB RAM; about 58 GiB available before testing.
- One disposable VM at a time: 1 vCPU, 2048 MiB RAM, the installed Ubuntu developer
  image, 16-GiB logical disk. The installed image filename begins `7685ce65`;
  the volume service's verified raw-image digest begins `232e11a1`.
- Isolated root-run daemon and volume supervisor; production services and existing
  VMs untouched. Dedicated NBD device outside the production device list. No Cloud
  tenant cgroup/throttle configuration applied to this fixture.
- Runtime source based on v0.3.2 plus opt-in timing logs; release-build daemon and
  volume service, packaged v0.3.2 VMM and CLI, existing fork pin `4f247dda`.
- Existing remote base and local image, warm host page cache. The first supervisor
  run lacks its in-memory digest cache. No global cache dropping or image download.
- Five sequential samples per storage mode, excluding the initial digest-cache
  miss and later diagnostic probes. Every sample creates, opens/exits a real CLI
  PTY shell, stops, starts, opens/exits a shell again, then deletes its own VM.
- Shell-ready requires a prompt followed by a command/response marker through the
  CLI's WebSocket session path. It is not merely the session-create HTTP receipt.

The create/start portion is measured by HTTP on host loopback; the subsequent
shell portion uses the released CLI on that host. These are **not Mac-to-Cloud
end-to-end timings**. That authenticated test is deferred at the user's request
because macOS Keychain approval requires access to the Mac. Cloud routing,
admission, network latency and client authentication are not isolated here.

## Baseline

Nearest-rank p95 with five samples equals the slowest observed sample. This is a
small diagnostic sample, not a production latency SLO.

| Operation | Median | p95 | Samples |
| --- | ---: | ---: | ---: |
| Local create + working CLI shell | 0.334 s | 0.347 s | 5 |
| Replicated create + working CLI shell, digest cached | 7.780 s | 7.870 s | 5 |
| Replicated cold start + working CLI shell | 6.863 s | 7.145 s | 5 |
| Local stop/start + working CLI shell, start portion only | 0.948 s | 0.963 s | 5 |
| Replicated create + shell, fresh supervisor digest cache | 17.181 s | n/a | 1 |

Local stop/start uses its existing snapshot restore behavior and is **not** a
RAM-retained pause, nor equivalent to replicated cold start. The replicated cold
starts retain local disk/cache state; they do not test recovery after local eviction.

For replicated runs, median HTTP create was 7.700 s and HTTP start was 6.563 s.
Shell attach/marker across those ten attachments had median 0.144 s and maximum
0.582 s. Local shell attach/marker median was 0.061 s.

## Stage breakdown

These are nested stage timings. Do not add sub-stages to their parent totals.

| Replicated stage | Median | Observations |
| --- | ---: | --- |
| Volume prepare, digest cached | 0.909 s | 5 creates |
| NBD attachment | 0.795 s | 10 create/start operations |
| Spawn worker and prepare gateway | 0.012 s | 10 operations; excludes guest boot |
| Wait for successful guest readiness | 5.723 s | 10 operations, range 5.720–5.724 s |
| Guest network configuration | 0.058 s | 10 operations |
| Bind volume to worker | 0.024 s | 10 operations |

The first image digest verification took 9.386 s; a second fresh-supervisor
diagnostic measured 9.239 s. This is scanning an already-local image, not
downloading it. Existing cache hits avoid the scan.

Additional instrumentation split the readiness stage:

1. First probe failed after 5.201 s.
2. Existing retry loop slept 0.500 s.
3. Next probe succeeded in 0.023 s.

An independent probe started 2.5 s after the bridge socket appeared and succeeded
at 3.035 s, while normal create finished at 5.740 s. This diagnostic changes probe
traffic, so it is excluded from baseline samples. It proves earlier readiness in
that run, not that a replacement algorithm has already saved 2.7 seconds.

The fork's vsock reaper has a five-second timeout
(`libkrucible/src/devices/src/virtio/vsock/reaper.rs`). That is a plausible source
of the first failed connection's lifetime, not yet a proven causal trace. Next
investigation must distinguish bridge accept, guest listener readiness, connection
rejection and delayed cleanup. Do not simply lower ordinary exec timeouts.

The cache also fetched 21 shared-base metadata objects in each of two observed
worker lifetimes, totaling 1.719 s and 1.558 s respectively. Local image data does
not eliminate remote reads of the immutable block maps. These totals include
the worker lifetime through shell/stop and can overlap other work; they are not
an additional amount to add to the boot stage. Some diagnostic recovery attempts
logged `commit budget exhausted before publication`; the measured operations
completed, and all disposable volumes were subsequently reclaimed. They are
another reason not to treat these few runs as a reliability qualification.

## Recommended order from the evidence

1. Addressed by the readiness fix below: bounded connection retry so an early failed
   connection cannot hide an already usable guest. Preserve real `/bin/true`
   success as the gate, failure deadlines, and no repeated execution of user work.
2. Cache/prewarm verified immutable base block maps across worker lifetimes.
   Bound cache memory/disk and retain integrity and image-version checks.
3. Move first-use image verification to explicit image preparation or supervisor
   warm-up; do not remove validation or trust a filename as proof of content.
4. Re-measure attachment overhead and remote publication stages after those fixes.
5. Continue real pause/resume and the bounded pool experiment from the lifecycle
   plan, comparing against this improved cold/create baseline.

There is no measurement here supporting replacement of SQLite or libkrun.
The fast local path is evidence that the existing stack can start this image
quickly. It does not imply replicated storage can reach the same number unchanged.

## Reproduction and diagnostics

Set `AHVM_TIMINGS=1` **before starting** an isolated daemon and volume supervisor.
The flag is cached on first use. It emits JSON timing lines on stderr and lets
volume workers inherit the supervisor's diagnostic stderr. Normal runs remain
disabled. IDs, process IDs, stage duration and success are logged, never tokens,
command payloads, output contents or credential paths. Successful timing writes
are best effort; logging errors do not fail lifecycle operations.

```bash
python3 scripts/measure-lifecycle.py \
  --endpoint http://127.0.0.1:28183 \
  --token-file /path/to/private/qualification.token \
  --cli /path/to/ahvm --mode replicated --samples 5
```

Use `--mode local` for the comparison. The script uses unique disposable names,
has bounded samples, exits its shells and deletes only its own VM in cleanup.
No raw results or credentials belong in the repository. Match engine IDs to
volume IDs using the script's result rows; detailed worker events identify their
process. Stage order and individual durations are available, but there is not yet
a distributed trace spanning Cloud and host clocks.

Validation: engine unit tests (50 passed), focused cache tests (2 passed), clippy
with warnings denied, formatting/diff checks, Linux release builds and the live
measurements above. These validations describe the initial measurement commit. All initial test VMs were
deleted, all nine replicated test volumes reached reclaimed state, and both
isolated services were stopped. The temporary test token was removed.


## Readiness fix in the same PR

The engine now uses a dedicated, fixed `/bin/true` readiness request with a
100-ms attempt deadline during the first second, 250 ms thereafter, and up to
50 ms between attempts. Both connect and the
whole reply share that deadline, capped by the overall configured readiness
budget. A stalled early connection is discarded; a new connection can observe
an already-running guest. Only an ExecResp with exit code zero is accepted.
Normal user exec/session requests are not retried or given shorter timeouts.

Nonblocking reads/writes check the deadline even for a trickled response. This
also avoids a macOS race where setting a receive timeout after peer closure
could return EINVAL despite a buffered successful response. No polling thread
is spawned per attempt, and every discarded probe socket is dropped.

The measurement harness can now take `--volume-root /path/to/isolated/volumes`
to wait for reclamation after deletion. An exploratory faster run exhausted the
fixture's journal reservations because reclamation lagged behind successive
creates; that run stopped before boot on its third create. The comparison below
uses a fresh complete series with this wait, not a selectively completed subset.
Waiting happens outside the timed create/start interval.


| Operation, five samples each | Before median / p95 | After median / p95 |
| --- | ---: | ---: |
| Replicated create + working CLI shell, digest cached | 7.780 / 7.870 s | 3.695 / 4.577 s |
| Replicated cold start + working CLI shell | 6.863 / 7.145 s | 4.829 / 6.082 s |
| Local create + working CLI shell | 0.334 / 0.347 s | 0.395 / 0.403 s |
| Local stop/start + working CLI shell, start portion | 0.948 / 0.963 s | 0.804 / 0.813 s |

In these host-loopback samples, median create-to-shell improved by about **53%**
and cold start-to-shell by about **30%**. This does not remove image hashing on a
fresh supervisor or remote metadata reads. The remaining latency varies instead
of being hidden behind the old near-constant failed-probe delay. These are small
sequential samples, not a guarantee of equivalent public Cloud improvements.

Fix validation: 54 engine unit tests on macOS, all four readiness regression tests
on Linux, five additional repetitions of the stalled-connection regression,
clippy with warnings denied, and the live create/start/shell measurements. Tests
cover stalled first connections, the outer deadline with no listener, unsuccessful
exec replies and a partial reply that must not extend the attempt deadline.

Local create increased by about 61 ms in the final five-sample series, while
local stop/start improved by about 144 ms. The first 250-ms-only probe prototype
had a larger local-create penalty; the final early 100-ms window reduces it.
Further event-driven readiness can avoid this retry-cadence tradeoff. The final
series used the final binary and no concurrent builds. All qualification VMs
were deleted and replicated volumes reclaimed before stopping the isolated services.

## Reusable startup caches

The next change removes repeated work in the volume supervisor and workers:

- Verified image digests are saved in root-only supervisor state, with a maximum
  of 64 entries. A hit requires the same host boot ID, device, inode, size, mtime
  and ctime (including nanoseconds). Image changes or host reboot force a fresh
  verification; ordinary supervisor restarts can reuse it. Hashing also checks
  that the image did not change during the scan. Missing/invalid cache records
  cause a scan, and a cache write failure does not fail image preparation.
- Immutable base metadata is shared between workers through a disposable cache
  under `base-metadata` in the supervisor root. It has 256 direct-mapped 64-KiB
  slots, at most 16 MiB per supervisor, independent of the number of VMs. Every
  hit is checked against its requested SHA-256; collisions, incomplete writes or
  corrupt bytes become remote fetches. No temporary files accumulate on crashes.
  Mutable remote heads and private VM objects are never served from this cache.

These caches do not change replication durability, disk formats, idle policy or
shell protection. A never-prepared image still needs its first full scan; after
host reboot, operators can move that scan out of the first create with the
existing `ahvm-volumed warm CONFIG.json IMAGE.ext4` preparation command.
The metadata cache is image-independent, including larger desktop images, but
replicated Omarchy still needs separate end-to-end desktop qualification.

### Cache qualification on agent_house

Five sequential samples per mode, one 1-CPU / 2-GiB VM at a time, using
host-loopback HTTP and the released v0.3.3 CLI through the same PTY marker test.
The final replicated series ran after builds and tests finished. Reclamation
completed between replicated samples, outside the timing interval.

| Operation | Median | p95 (maximum of five) |
| --- | ---: | ---: |
| Replicated create + working shell | 2.038 s | 2.310 s |
| Replicated cold start + working shell | 3.827 s | 3.937 s |
| Local create + working shell | 0.392 s | 0.402 s |
| Local stop/start + working shell | 0.744 s | 0.802 s |

Compared with the preceding readiness-only series, replicated create-to-shell
fell from 3.695 s to 2.038 s (about 45%); cold start-to-shell fell from 4.829 s
to 3.827 s (about 21%). Local-mode timings remained similar. These are small
host-side samples, not public Cloud latency guarantees. An earlier exploratory
five-sample cache series included a 5.020-s cold start; tail latency remains work.

With entirely empty caches, first create-to-shell took 13.615 s, including
9.245 s hashing the image. After a supervisor restart with saved caches, digest
lookup took 0.090 ms and create-to-shell took 2.117 s. The first worker fetched
20 metadata objects plus one supervisor catalog fetch. Subsequent observed
workers needed no remote base-metadata fetches. New images and host reboots
still incur verification unless explicitly prepared before the user request.

Validation: 94 macOS volume tests and 113 Linux volume tests passed, with clippy
and formatting clean. Regression coverage includes supervisor-restart reuse,
same-size edits with restored mtime, reboot invalidation, corrupt cache fallback,
symlink refusal, bounded disk use, reuse across independent worker caches, and
exclusion of mutable/private objects. A separate live replicated VM passed Bash,
HTTPS, stop/start, persisted-file verification and deletion/reclamation.
Production services and user VMs were not modified for these measurements.

### Remaining improvement sequence

1. Measure and merge the reusable caches above.
2. Interactive create opens a shell by default, with an explicit no-shell option
   for scripts; compare usable-prompt timing rather than API receipt timing.
3. Implement and qualify lightweight idle pause/resume. A connected shell stays
   protected; pausing must remain distinct from disk eviction and cold start.
4. Experiment with a small bounded prebooted Ubuntu pool, with clean VM identity,
   private writable storage and accounted resources before assigning a VM.
5. Qualify desktop images with replicated storage, including desktop reconnect,
   disk persistence and recovery, before enabling them in Cloud.


## Resident idle pause qualification

The reusable caches and interactive create-to-shell changes are merged. Step 3
adds configurable resident pause; [policy and semantics](IDLE-PAUSE.md).

On `agent_house`, one isolated **1-vCPU / 2-GiB** VM at a time, with a five-second
qualification timeout, direct loopback HTTP resume-to-successful-exec measured:

| Storage | Three samples |
| --- | --- |
| Replicated | 33 / 36 / 33 ms |
| Local | 33 / 30 / 38 ms |

Each sample began with observed `paused` state. All retained the same worker PID
and guest boot ID. Both modes also passed daemon restart/adoption while paused,
a quiet connected shell lasting 12 seconds (past the five-second timeout),
explicit stop from paused, start, and disabling future pause. These timings do
not include Cloud routing, client network latency or shell attachment. They do
not measure cold start or imply that paused RAM is freed.

Unit coverage includes atomic idle admission, policy changes racing a pending
pause decision, admin-only policy validation, persistence across reopen,
paused CPU/RAM accounting, adoption preserving intentional pause, and resolving
a lost PAUSE acknowledgement via STATUS. The KVM backend gate now includes
three pause/resume cycles with worker and guest boot identity checks.

All disposable VMs were deleted, replicated volumes reclaimed and isolated
services removed. Production services and user VMs were unchanged. Desktop
pause remains unqualified. The next experiment is a bounded prebooted Ubuntu
pool; desktop replicated-storage qualification follows it.


The next bounded-pool experiment is recorded in [PREBOOT-POOL.md](PREBOOT-POOL.md):
prepared running/paused VM claims reached a real shell in roughly 127/143 ms
on the host. This uses an isolated placement model; production Cloud claims
and network latency are not included.
