# Lifecycle measurements, 2026-09-15

## Result

The replicated path has a measurable readiness delay. In a diagnostic run, an
independent `/bin/true` succeeded 3.035 seconds after the bridge socket appeared,
but the normal create path finished at 5.740 seconds after that point. About
2.7 seconds of that run was avoidable waiting, not necessary guest boot work.
This warrants investigation before investing in a prepared VM pool.

The same Ubuntu image creates quickly with local storage. We have not changed
timeouts, storage guarantees, boot behavior or production deployments in this PR.

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

1. Fix/prove readiness signaling or bounded connection retry so an early failed
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
measurements above. No production upgrade or behavioral optimization is included. All test VMs were
deleted, all nine replicated test volumes reached reclaimed state, and both
isolated services were stopped. The temporary test token was removed.
