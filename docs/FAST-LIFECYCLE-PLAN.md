# Fast create, pause and wake

Status: investigation in progress, 2026-09-15.

Caches, automatic shell after create, and resident idle pause are implemented.
Current shipped/implemented policy details are in [IDLE-PAUSE.md](IDLE-PAUSE.md);
the proposed retention/task-lease choices below are not all implemented. The
[prebooted pool experiment](PREBOOT-POOL.md) now has measured results; Cloud
ownership integration and rollout remain next.
This document changes no runtime defaults. Breaking API, CLI and state-format
changes are allowed; preserving user data and tenant isolation remains required.

## Outcome

Make getting a working shell fast, keep active work uninterrupted, pause unused
VMs after 30 seconds, and retain durable disk state independently of compute.
Optimize the complete CLI-to-working-shell path, not just the create response.

## What we learned from Sprites

Sprites documents about 30 seconds of inactivity before a RAM-preserving warm
pause, followed eventually by a cold stop. It advertises 100–500 ms warm wake
and 1–2 s cold wake. These are vendor figures, not AHVM measurements or promises.
Its documentation says TCP connections do not survive a pause.
[Source: lifecycle](https://docs.sprites.dev/concepts/lifecycle/).

Its engineering article describes standard base environments and pools of empty
Sprites prepared ahead of requests. This is the useful create-time idea to test.
[Source: design](https://fly.io/blog/design-and-implementation/).

Checkpoints are separate, intentional filesystem save points, without RAM or
process state. Restoring restarts the environment and terminates sessions.
Sprites also documents automatic checkpoints; we will not copy that policy.
[Source: checkpoints](https://docs.sprites.dev/concepts/checkpoints/).

## AHVM starting point

- Shared immutable base images and private writable blocks already exist. A
  prepared create does not need to copy or upload an entire Ubuntu image.
- Replicated create still prepares storage, attaches NBD, starts the gateway and
  VMM, boots Linux, probes the guest agent and configures networking.
- Replicated start currently cold-boots. Existing VMM PAUSE/RESUME commands serve
  snapshot operations, not an idle lifecycle. Adoption currently resumes paused
  workers, which must distinguish an intentional idle pause from interrupted work.
- The daemon thermal manager currently stops idle VMs; its source default is one
  hour. Setting this to 30 seconds alone would implement the wrong behavior.
- Guest readiness retries currently sleep 500 ms. Image digest verification has
  an in-memory cache that is lost on supervisor restart.
- Historical shared-base qualification measured 8.14 s create and 6.77 s cold
  wake with local image data. Those used an older 1-GiB configuration and are
  context only, not the baseline for the current release.

Starting code: [thermal manager](../rust/ahvm-daemon/src/thermal.rs),
[engine](../rust/ahvm-engine/src/krucible.rs),
[replicated lifecycle](../rust/ahvm-engine/src/krucible/replicated.rs),
[volume service](../rust/ahvm-volume/src/service/mod.rs),
[shared-base qualification](SHARED-BASE-IMAGES.md).
Cloud routing/admission lives in the private ahvm-site repository.

## Proposed lifecycle and defaults

| State | Meaning | What remains allocated |
| --- | --- | --- |
| Running | Serving a shell, operation, task or traffic | CPU, RAM, disk/cache, workers |
| Paused | Guest execution suspended; RAM and processes retained | RAM, disk/cache and necessary workers; low host overhead remains |
| Stopped | Guest and RAM released; next wake boots fresh | Durable disk; local cache retained until separately reclaimed |

Proposed Cloud default: pause after **30 seconds with no activity holds**. Start
with a **15-minute paused retention window**, configurable and subject to the
host's accounted warm-memory budget. This retention value is an initial tuning
choice. A paused VM may become stopped earlier under capacity pressure, but only
through the verified storage-safe stop path. Pressure must never silently evict
unreplicated dirty data. Do not advertise a paused VM as consuming no resources.

Keep self-hosted automatic pause opt-in initially, supporting both local and
replicated disks. Cloud and self-hosted use the same lifecycle implementation.
Expose the effective policy in status/admin views and document it on the website.
Do not change desktop defaults until its pause/resume qualification passes.

Activity rules:

- A healthy connected shell holds the VM awake, even with no typing or output.
  Desktop attachments receive the same protection when enabled.
- Running execs, file transfers and other guest operations hold activity for
  their actual duration. A disconnected caller must not release a hold while
  admitted backend work still runs.
- Background agents need a renewable task lease, released on completion and
  bounded by expiry after a dead client. Silent work is not detectable from
  output alone. Define CLI/API support before enabling aggressive auto-pause.
- Active preview traffic counts as work; specify and test long-lived connection
  handling. A quiet listening service alone does not pin a VM forever.
- Status polling and internal replication must not continually reset idle time.
  Network keepalive detects dead clients; it is not a maximum shell lifetime.
- Activity admission and pause commitment share one atomic state transition.
  Work arriving during pause joins a coalesced wake, without duplicate execution.

Pause is not a backup: no full snapshot or R2 flush on the warm-pause critical
path. Continue bounded asynchronous replication of already accepted disk writes.
Explicit sync retains its existing durability meaning. Local acknowledgement can
still lose pending writes on host loss; fast pause must not imply stronger durability.

## Phase 1: measure the current path

Add correlated, monotonic stage timings across CLI, Cloud, daemon, volume service
and VMM. Capture authentication/routing, admission, base verification, remote
metadata operations, NBD attachment, worker start, guest readiness, networking,
session creation and first usable prompt. Do not log credentials or shell contents.

On agent_house, use one disposable 2-GiB Ubuntu VM at a time. Record image digest,
versions, host load, cache condition and storage mode. Measure local and Cloud
paths, prepared create, restart-cache miss, cold wake and later warm wake/pool hit.
Collect repeat samples and report sample count, median and p95; small-sample p95
is indicative only. Distinguish one-time image import from per-VM creation.

Deliverable: concise Markdown timings and ranked bottlenecks, no large result
files committed. Establish a reproducible baseline before changing architecture.

## Phase 2: real pause/resume and safe activity

1. Spike VMM PAUSE/RESUME on an isolated VM, with a shell process and RAM marker.
   Check CPU usage, clocks/timers, gateway behavior, vsock, NBD and pending I/O.
   A SIGSTOP of the whole worker is not a substitute for device-aware suspension.
2. Add explicit paused state and transition intent to backend, persisted records,
   reconciliation and Cloud. Handle lost replies and daemon restart without
   misclassifying a paused worker as failed or resuming it accidentally.
3. Implement holds/task leases, atomic pause admission and single-flight wake.
   Authorize and reserve capacity before wake; preserve request ordering and avoid
   replaying exec or input after ambiguous transport errors.
4. Wire the configurable 30-second idle policy only after the above passes.
   Implement bounded warm retention and safe cold demotion. Keep cache reclamation
   distinct from guest pause and from object-storage garbage collection.

Acceptance: repeated pause/resume preserves PID and RAM marker; quiet connected
shell and silent held task survive well beyond the idle window; unheld idle VM
pauses near 30 seconds (target within 2 seconds on an unloaded host); parallel
wakes resume once; no deadlock on stop/destroy races; daemon restart preserves
intent. Test pending writes, an unavailable R2 endpoint and storage errors without
losing acknowledged local data through eviction. Run cold-wake file checks too.

Target: host-side warm resume below 500 ms, separately report Cloud prompt-ready
latency. Failure to meet the target triggers profiling, not a weaker readiness check.

## Phase 3: prepared create pool

Try **one** pristine, already-booted Ubuntu VM matching the default 2-GiB profile.
Charge its full resources against host capacity and replenish with low priority.
If full, empty or mismatched, fall back to normal create. Desktop and custom
profiles are initially excluded. Compare pool hits and misses separately.

Claim must atomically transfer an internal VM to exactly one tenant, coordinating
Cloud ownership, daemon records, quotas and storage ownership/fencing. Do not
implement this as a public name change. A pool VM has never held user data or
credentials; a previously used VM is destroyed, never returned to the pool.
Validate unique machine/session identities, entropy, network isolation and image
version. Replenishment failures must not break ordinary create.

Acceptance: simultaneous claims have one winner, crash recovery never double
assigns a VM, wrong profiles bypass the pool, and partial claims are recoverable
without an unaccounted worker. Target under 2 seconds from create to usable shell
on a pool hit. Keep the pool only if measured benefit justifies retained resources.

## Phase 4: improve misses and cold wake

Use Phase 1 evidence to prioritize:

- Replace coarse readiness polling with notification or a faster bounded probe.
- Prewarm verified base-image metadata after restart; preserve integrity checks.
- Measure a boot block working set, then prefetch only useful blocks with bounded
  concurrency. Avoid scanning/downloading the full guest disk on every wake.
- Batch or parallelize independent metadata work while retaining ownership fences,
  atomic admission, remote publication order and bounded resource usage.
- Profile storage/SQLite costs before changing database or block architecture.

Re-measure create misses, cold wake, shell responsiveness and replication lag
under load. Do not trade terminal latency for aggressive background prefetch.
Treat Sprites' advertised 1–2-second cold wake as an aspiration, not an exit gate
until measurements establish what is feasible on our host and current image.

## Phase 5: CLI and product integration

Interactive `ahvm create dev` should attach to a shell once ready. Add `--detach`
for create-only; non-TTY/JSON usage remains noninteractive. Report creation and
connection separately and keep the VM if attachment fails. Explain `exit`, detach,
automatic pause, cold loss of processes and explicit destroy.

Shell/exec/preview should transparently wake an authorized VM. Show meaningful
progress for cold wake, and effective state/policy in CLI and dashboards. Publish
the same defaults in ahvm.app/docs. Re-test OpenCode, scrolling, quiet shells,
long-running tasks, reconnects and local versus Cloud routing.

## Later: intentional checkpoints

Investigate cheap immutable disk-generation checkpoints on the existing shared
base/private-block model. Creation should capture an atomic block-map generation,
not export a whole disk or RAM snapshot. Define crash-consistent versus
application-consistent guarantees, guest writeback and optional quiescing explicitly.
An atomic host block map alone cannot capture guest RAM buffers.

Separate local checkpoint creation from confirmation that all referenced blocks
are durable remotely. Restore is explicit and destructive, restarts processes,
and must fence concurrent writes. Consider restore-as-new first, comments,
read-only browsing and guest-scoped checkpoint access later.

Keep checkpoints on demand, with configurable expiry and pinning. Reuse safe
reachability-based reclamation across current disks, shared bases and retained
checkpoints; deletion does not promise immediate physical object removal.
No scheduled backups or hidden automatic checkpoints are introduced by this plan.

## Delivery and completion

Use separate reviewable PRs for timing, pause primitives, activity/policy, pool,
measured cold-path changes and CLI/dashboard integration. No Jira key was supplied
for this plan; associate actual work items when available rather than inventing one.
Roll out behind capability/configuration gates, first on isolated agent_house
resources. Do not pause or replace the user's active VM during qualification.

Done means recorded before/after prompt-ready results, correct resource accounting,
no idle-induced disconnections during active work, verified warm/cold persistence
contracts, and matching CLI, dashboard and website documentation. Desktop gets a
separate GPU/VNC/systemd qualification before inheriting the 30-second policy.
