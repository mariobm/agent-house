# Replicated storage: engine integration

Phase 4, first PR. The Rust engine now supports a persisted local/replicated
selection and a host volume-service interface. This is an opt-in integration
for qualification, not an installed cloud backend or a released CLI flag.

## Selection and lifecycle

- `KrucibleConfig.default_storage_mode` defaults to `Local`.
- `SandboxSpec.storage_mode = None` selects that default once at creation.
  An explicit `Local` keeps a VM local even when the host default is replicated.
- `sandbox.json` persists the resolved mode and a random 256-bit volume ID.
  Older records without these fields remain local. A changed host default never
  converts a VM. Mode/ID inconsistencies fail to open.
- Replicated mode requires Linux and an explicit `ReplicatedConfig.socket`.
  Missing configuration/service fails explicitly. The engine receives a checked
  raw NBD block-device path, never object-store credentials. It creates no qcow2
  overlay and never silently falls back to the base image.
- Create fsyncs its intent before requesting a volume. A failed or ambiguous
  request retains an accounted failed sandbox with the same volume ID. Preparing
  that ID is idempotent; once preparation is recorded, starts never reimport the
  image. Deleting the failed sandbox is the supported cleanup path.
- Stop runs guest sync, terminates the worker, drains the accepted journal to R2,
  then detaches the block device. Thus **stop is stronger than guest fsync**;
  guest fsync remains local-only. If draining fails, the stopped worker is reported
  failed and the volume/journal stay tracked for retry. No RAM bundle is created.
  Disk state is crash-consistent; guest applications must flush their own state.
- Start reattaches the same volume and cold-boots. Worker-only termination retains
  the journal and disk. Engine restart adopts the surviving worker and checks
  that its device still matches the volume service. The service has its own
  lifetime and must remain available across engine restarts.
- Delete fsyncs an intent, terminates the worker, asks the service to logically
  delete the ID, and only then removes the engine record. Failed cleanup remains
  retryable across engine restart; start refuses a pending delete.
- `SandboxInfo.storage` reports mode, volume ID and optional replication counters.
  Missing counters mean unavailable, not zero backlog. A running VM with an
  unavailable/local-failed service reports Failed. Remote replication failure
  alone does not fail a locally writable VM. Status checks do no R2 I/O themselves.
- `sandbox_capabilities(id)` disables snapshots/forks/migration for replicated
  volumes. Those operations also reject explicitly. Local capabilities remain
  unchanged. The explicit `sync_remote(id)` currently requires a stopped VM;
  live checkpoint/quiescing support belongs to the checkpoint phase.

The daemon/CLI still choose local mode. Their changes in this PR only initialize
new engine fields; HTTP storage selection/status and installation are not exposed
prematurely. Combining the experimental service with the existing cgroup/project
quota configuration is refused until sidecar memory/journal accounting is wired.

## Host service protocol

A trusted private Unix socket accepts one newline-terminated JSON request and
returns one newline-terminated JSON reply per connection. Requests contain:

```json
{"version":1,"operation":"attach","volume_id":"<64 lowercase hex>","image":null}
```

Operations: `prepare` (absolute base-image path), `attach`, `inspect`, `status`,
`sync`, `detach`, `delete`. `inspect` must never replace/reconnect an attachment
under a live VM. `prepare` must never replace an existing or tombstoned volume.
`delete` must be idempotent and retain its deletion intent across lost replies.
Credentials and bucket configuration belong exclusively to the service.

Replies contain `ok`, the identical `volume_id`, and optional `device` and
`status`. Status contains `local_sequence`, `remote_sequence`, `pending_bytes`,
`local_failed`, `replication_failed`. The client bounds replies to 4 KiB, checks
identity/watermarks and rejects non-NBD/symlink/non-block paths. A drain must have
zero backlog, matching sequence counters and no failure flags before success.
Prepare has a 600-second timeout, status three seconds, other operations 300
seconds. Timed-out mutations are ambiguous: retry the same identity, never make
another volume implicitly.

`engine-service.py` is a **single-volume qualification adapter** around the
existing Rust `indexed_nbd` example. It requires root only to attach an unused
NBD device, keeps a private local record, and leaves R2 objects for explicit
fixture cleanup. It refuses service-root reuse; service restart/adoption and
multi-host fencing are not implemented. An incomplete import remains failed
rather than overwriting remote state. This adapter is not a production service.
Its logical deletion does not yet reclaim/tombstone the remote objects.

## Qualification

Use one 1-CPU/1-GiB VM, a small prepared Alpine root and the private qualification
bucket. No installed daemon, default image or systemd unit is modified.

Start the adapter in a terminal, with a new root and unused device:

```bash
sudo modprobe nbd nbds_max=4 max_part=0
sudo python3 experiments/durable-storage/engine-service.py \
  --root /tmp/ahvm-engine-volume \
  --config /path/to/private/r2.json \
  --server /path/to/indexed_nbd --device /dev/nbd0
```

In another terminal, against this branch's Rust engine:

```bash
export AHVM_REPLICATED_TEST=1
export AHVM_REPLICATED_DATA=/tmp/ahvm-engine-vms
export AHVM_VOLUME_SOCKET=/tmp/ahvm-engine-volume/service.sock
export AHVM_VMM_BIN=/opt/ahvm-rust/bin/ahvm-vmm
export AHVM_GUEST_IMAGE=/path/to/small/root.ext4
export LD_LIBRARY_PATH=/opt/ahvm-rust/lib
cargo test --manifest-path rust/Cargo.toml -p ahvm-engine \
  --test kvm_replicated -- --nocapture
```

The test covers implicit host-default selection, create/exec, stop/start without
RAM snapshots, abrupt worker loss, engine adoption with the same PID, changing
the host default without changing the volume, capability rejection and destroy.
After success, stop the adapter and remove its exact qualification R2 prefix and
local journal. If a test fails, terminate its recorded worker before detaching
NBD; do not detach a device underneath an unrelated VM.

## Verified on agent_house (2026-09-13)

The live engine gate passed in 79.83 seconds including importing the 512-MiB
fixture. It used one 1-CPU/1-GiB VM and covered SIGKILL recovery and PID-preserving
engine adoption. The earlier SIGTERM run also passed. These are qualification
runs, not production boot-latency benchmarks.

43 engine unit tests on macOS, 50 on Linux, and the daemon tests locally pass; Clippy with warnings
denied, formatting and Python compilation pass. The new deterministic cases
cover retained identity after failed prepare, no reimport after preparation,
failed-delete restart protection, legacy local defaults, missing configuration,
reply identity/watermark validation, and rejection of a false drain acknowledgement.

Both test R2 prefixes were deleted and verified empty. The adapter, journal and
credential copies and NBD attachment were cleaned up; all four installed AHVM
services remained active.

## Remaining before rollout

Fenced ownership and a supervised service with restart recovery are next. They
must prevent two engines/hosts sharing a writable volume, invalidate old owners,
and account for retained journals/attachments. No automatic failover is enabled.
Also remaining: remote deletion/GC, quotas including the service, persistent clean
cache, full-backlog R2 performance/cost qualification, and product API/install
integration. This PR does not declare the full phase-4 exit gate complete.
