# Replicated volume service

`ahvm-volumed` is the Linux Rust supervisor for optional replicated disks. It
uses `OwnedDisk`, the local journal and asynchronous S3-compatible replication.
Guest fsync does not wait for R2. The default daemon/CLI storage mode remains
local; no object store is required for self-hosted local disks.

This service replaces the Python qualification adapter. It is included in the
Linux server bundle but **not enabled by the installer**. Quota integration,
checkpoint-aware reclamation and daemon/API selection must land before cloud activation.
The Python adapter remains only for reproducing the earlier qualification gate.

## Configuration and installation

Build with `make volume` on Linux. The binary requires root for NBD configuration,
process identity checks and terminating a verified VM after storage failure.
Install `nbd-client` and load a pool of unused NBD devices, for example:

```bash
sudo modprobe nbd nbds_max=4 max_part=0
```

Use a private, persistent state directory on a local filesystem that supports
fsync, atomic rename and file locks. Do not copy its owner identities to another
host. Configuration is JSON; paths are absolute. Example:

```json
{
  "root": "/var/lib/ahvm-volume",
  "engine_root": "/var/lib/ahvm-rust/sandboxes",
  "credentials": "/etc/ahvm-rust/volume-s3.json",
  "nbd_client": "/usr/sbin/nbd-client",
  "devices": ["/dev/nbd0", "/dev/nbd1"],
  "client_uid": 1001,
  "limits": {
    "max_volume_bytes": 68719476736,
    "max_logical_bytes": 1099511627776,
    "max_journal_bytes": 17179869184,
    "max_cache_bytes": 2147483648
  },
  "image_roots": ["/opt/ahvm-rust/share"],
  "socket_dir": "/run/ahvm-volume"
}
```

Set `engine_root` to the actual `KrucibleConfig.data_dir`, and `client_uid` to the
host daemon's numeric UID. Root is also accepted for administration. The separate
socket directory is root-owned and traversable; only the configured UID/root can
connect to the mode-0600 socket. Assigned NBD devices are also mode 0600 and
owned by that engine UID. Non-root clients must use `image_roots`: root-controlled
image directories/files with no unprivileged write access, so import cannot be
used to read arbitrary root-private files. Journals, credentials and worker sockets remain
private to root. With `client_uid` omitted (zero), `socket_dir` can be omitted and
the socket lives inside the private state directory.

The existing S3 credential format is used: `endpoint`, `region`, `bucket`,
`prefix`, `access_key_id`, `secret_access_key`, optional `session_token`. Keep that
file mode 0600. Credentials are never returned through the engine protocol.
Use a private bucket/prefix and scoped object read/write credentials.

The optional systemd unit is `packaging/rust/ahvm-volume.service`. Its default
binary/config paths are `/opt/ahvm-rust/bin/ahvm-volumed` and
`/etc/ahvm-rust/volume-service.json`. Install/configure the unit explicitly; do not
enable it against devices used by another service. Engine configuration must
point `ReplicatedConfig.socket` at this socket. The public daemon does not yet
expose that selection through its environment or API.

## Capacity admission and accounting

The required `limits` object sets per-volume logical capacity and host totals for
logical disk capacity, journal reservations and clean-cache payload reservations.
All values are bytes. The example allows disks up to 64 GiB, 1 TiB of total logical
capacity, 16 GiB of journal reservations and 2 GiB of cache reservations. The
smallest budget or configured device pool wins. These are operator-selected host
budgets, not per-tenant cloud quotas or measurements of object-store consumption.

Before import performs any remote writes, admission reserves the source image's
logical size under the registry lock and persists it as `logical_bytes` in the
volume record. Sparse image holes still count toward logical capacity. The source
cannot grow past that reservation on an import retry. Each locally resident record also reserves
512 MiB for journals (a 256-MiB log plus its simultaneous compaction replacement)
and 64 MiB of clean-cache payload. These values share the worker's actual bounds.

Failed imports and deleted-but-not-yet-reclaimed records remain charged. Stopped
disks release local journal/cache reservations after safe eviction, but retain
the logical capacity charge for their remote disk. Retries do not charge twice. Deleted volumes release reservations only
after an empty remote chunk listing and successful local journal removal. Failed
cleanup retains the reservation and retries. Raising a budget requires an explicit
config change and service restart. Do not remove owner/journal records to evade the budget. A reservation
write failure freezes further imports and cold reactivation until restart reconstructs the durable
registry. Lowering a budget preserves existing disks and blocks new admission
while over budget; it does not kill workers or discard pending writes.

This is conservative admission accounting, not filesystem preallocation. Keep
headroom for metadata, other applications and filesystem overhead. The cache
budget is not a process RSS cap: dirty buffers, maps, in-flight requests and other
allocations are additional. Obsolete objects on continuously running disks remain until a stopped collection window.
The per-volume journal/backlog limits already enforce write backpressure; no
remote request or host-wide accounting lock is added to guest I/O.

A trusted client can send the existing private protocol a `usage` request:

```json
{"version":1,"operation":"usage","volume_id":"<64-character ID>","sandbox_dir":"/var/lib/ahvm-rust/sandboxes/<id>"}
```

It returns `usage.reservation`, sampled `local_file_bytes` and
`local_allocated_bytes`, `host_reservations`, and configured `limits`. File samples
include retained metadata and temporary journals, can race atomic compaction, and
are informational; admission uses durable reservations. Symlinks/unexpected file
types are rejected. A tombstoned volume can still be inspected using its original
sandbox path. No credentials or remote keys are exposed. This protocol operation
is not yet a public CLI command.

This changes the experimental configuration/record schema: `limits` and recorded
`logical_bytes` are mandatory. Old unaccounted records fail closed rather than
inferring capacity from a mutable source image. Use a fresh qualification root;
there is no silent production migration or enabled service change.

## Lifecycle and recovery

- Each volume is bound permanently to one canonical sandbox directory and its
  persisted volume ID. Every request carries `sandbox_dir`; a copied/different
  engine tree cannot reuse the attachment. After boot, the engine sends `bind`;
  the service checks the worker state, process start time, host UID and raw device before
  recording the VM identity.
- Volume records and device assignments are fsynced before external effects.
  Spawned helpers wait on a pipe until their process identity is persisted. If
  the supervisor dies before committing launch, pipe EOF cancels the helper.
- Workers live independently of the supervisor. Restart adopts healthy workers
  and existing NBD connections; it does not reconnect a running guest's disk.
- On storage/attachment failure, background recovery terminates only a verified
  VM identity, waits for death and checks for other device consumers, then
  reconnects the original journal/owner. Unknown consumers cause refusal and
  retry, never a guessed kill. A running but unresponsive worker gets repeated
  health checks before fencing. Mere remote replication lag/error does not kill
  a locally healthy VM.
- Recovery is per volume with exponential backoff capped at 60 seconds. Healthy
  probes run outside operation locks. One slow volume does not block other
  volumes or the sweep. VM cold boot remains an engine `start` operation; the
  service repairs storage automatically but does not create a replacement VM.
- Import binds a SHA-256 of the source. Unfinished unowned imports can resume;
  every byte is rewritten and verified before publication as ready. Changed
  sources fail. Ready or owned disks are never overwritten by a retry.
- Stop/detach retain remote ownership; safe background eviction removes the
  synchronized journal while keeping the small private owner identity.
  Delete records intent before cleanup. Background reclamation retires the
  remote identity, removes its chunks and then its local owner/journal directory.
  Small local records and remote retirement markers remain to prevent identity
  reuse. No scheduled backups, automatic cross-host takeover or implicit mode
  conversion is added.

The service exclusively locks its configured device pool. One slot is reserved per locally resident,
non-deleted volume. Successful local eviction or logical deletion frees the slot;
starting a cold volume must reserve a free slot again. The pool is bounded to 32 devices, the registry to
1,024 records including tombstones, and concurrent API handlers to 16. These are
service bounds, not tenant quotas. Per-tenant journal/cache/disk accounting is
still required before offering replicated storage to cloud users.

Stopping the systemd unit deliberately leaves storage workers alive, like an
engine restart leaves VMs alive. For maintenance, stop/destroy VMs through the
engine first. Restart the supervisor after replacing its binary; executable
paths are retained so an inode replacement does not leave a `(deleted)` launch
path. Replacing credentials does not update already-running workers; drain and
restart their attachments when rotating credentials.

Legacy Python state directories are rejected. Upgrade/migration from experiments
is explicit. Unknown device bindings and malformed records fail closed instead
of importing a fresh disk. Retain the state directory for operator recovery.

## Validation

`cargo test -p ahvm-volume` covers the launch gate (EOF before commit cannot
execute a helper), process/boot identity, per-volume lock independence, sandbox
binding refusal, durable deletion intent and resumed imports. The engine's
replicated lifecycle test accepts `AHVM_VOLUME_AUTORECOVERY=1` with
`experiments/durable-storage/volumed-recovery-hook.py` for the isolated KVM gate.
The hook also creates a tiny raw second volume to verify its worker/data survive
recovery of the VM's storage; it does not create another VM.

### Verified on agent_house (2026-09-13)

The KVM/R2 gate passed in **95.97 seconds**, including 512-MiB image import, with
one 1-vCPU/1-GiB VM. Supervisor SIGKILL preserved the VM; storage-worker SIGKILL
triggered verified VM termination and automatic disk recovery. The second tiny
raw volume retained its worker PID and data. Engine cold restart recovered the
file, and engine adoption/destroy completed. No second VM was created.

A separate non-root test exercised prepare, attach, actual block writes/fsync,
remote sync, detach and delete as UID 65534. An unrelated UID could not connect;
private state and an image outside the configured roots were denied. Delete
revoked device ownership back to root.

Linux: 51 engine + 67 volume unit tests and the gated-launch process test.
macOS: 44 engine + 59 volume unit tests and 49 daemon tests. Clippy with warnings
denied, formatting, Python compilation, shell syntax and systemd unit verification
pass. The full GPU/server release bundle was not rebuilt or published; packaging
now includes the optional binary/unit. No installed daemon/default changed.

All five test R2 prefixes were deleted and verified empty. Test journals, device
attachments and copied credentials were removed. All four installed AHVM services
remained active; no release was published and no cloud storage mode was enabled.

### Capacity accounting validation

On agent_house, 75 volume unit tests plus the gated-launch process test pass;
macOS has 59 volume unit tests. Clippy with warnings denied and formatting pass.
The added regressions cover concurrent one-slot admission, retry/retained-delete
accounting reconstructed from records, each independent budget, changed source
size, uncertain record publication, lower budgets preserving existing identity,
usage sampling and rejection of unaccounted old records. This slice needed no
VMs or R2 objects and did not modify installed services. The earlier 95.97-second
KVM result above belongs to the supervisor qualification, not this test run.

The Linux suite separates six root-only service tests from ordinary unit tests.
Run `make test-volume-root` to compile as your normal user and execute only those
six tests with root privileges. CI runs this target explicitly after `make test`;
no cloud access or VMs are needed. Production root-directory checks are unchanged.
The ordinary Linux volume suite runs 69 tests, and the privileged step runs six;
both must pass. Earlier agent_house validation ran the combined suite as root,
which did not expose the unprivileged CI fixture mismatch.


## Deleted-volume reclamation

Deletion acknowledges local intent and detachment; it does not wait for R2.
The supervisor picks up deleted records automatically. It permanently retires the
remote head with a conditional write before removing any chunks. A current owned
volume requires the private owner lock and matching remote identity; another live
owner is refused. Incomplete or ready-but-not-enrolled imports are also covered.
Retirement intentionally discards unsynced writes to a disk the user deleted.

Format 5 is a strict permanent retirement marker. Older indexed/owned readers
reject it, and normal create cannot overwrite its existing head. Keep this tiny
marker indefinitely, along with the local deletion record. Never delete it as
part of chunk cleanup. Unknown formats/fields fail closed. This collector supports
volumes without checkpoints only; adding checkpoints requires a new compatible
reference/format design before this path may delete checkpoint-backed data.

One collector runs at a time, deleting at most 16 chunks per pass. Each S3
request is bounded to three seconds. It lists only the exact volume chunk prefix,
validates every returned key, and deletes individual objects. Each pass restarts
at the first page of the shrinking listing, so interrupted cleanup cannot skip
objects via an obsolete pagination offset. Failures retry with the supervisor's
bounded backoff. Completion requires a subsequent empty listing. Only then are
local journals removed/fsynced and `reclaimed` persisted, releasing reservations.
`usage` exposes `reclamation_complete`; the retained metadata remains measurable.

Successful partial passes retry after one second. Reclaimed records are checked
hourly (and after supervisor restart), collecting any late orphan uploads from
requests issued before the old worker died. No running volume is scanned or
paused. This housekeeping does not create backups. Retirement markers/records
still count toward the existing 1,024-record registry bound; compacting that
registry and reference-aware reclamation for live disks remain follow-ups.

### Reclamation qualification

`reclaim_probe` creates two fresh one-chunk images in a private qualification
prefix, refuses retirement of a live owner, reopens the transport after a partial
cleanup pass, verifies peer reads and refuses reuse of the retired identity.
The real R2 probe passed in **8.91 seconds** with both chunk sets empty. Two small
retirement markers were intentionally retained. No VMs or installed services were
changed. This measures a small cleanup probe, not bulk deletion throughput.

Deterministic tests cover interrupted deletes, lost retirement replies, late
orphan uploads, malformed markers, failed imports, discarded pending writes,
foreign listing keys, permission/delete failures, and releasing admission budgets
only after both remote and local cleanup. The privileged runner discovers the
root-only tests automatically, including the new cleanup/admission regression.

An isolated `ahvm-volumed` smoke on agent_house imported a 128-KiB image, accepted
delete, automatically removed its remote chunks/local owner directory and released
the capacity reservation in **3.52 seconds**. A repeated delete succeeded. It used
no VM; the temporary NBD module, service, image and copied credential were removed.
All four installed AHVM services remained active. This probe leaves one additional
small retirement marker, for three qualification markers total.

Linux validation: 79 ordinary volume tests, seven explicit root-only tests and the
launch test pass. Clippy with warnings denied and formatting pass. macOS tests
cover the portable protocol and S3 adapter; no new guest I/O behavior is claimed.


## Obsolete blocks on stopped disks

The supervisor now collects obsolete blocks from **existing, stopped disks**.
It does not pause running VMs for collection. A volume becomes eligible only after
an explicit successful detach; prepare alone never schedules collection, so
initial creation is not delayed by housekeeping. Attach disables eligibility and
resets the scan cursor. Existing service records default to ineligible until their
next explicit detach.

Each pass proves the VM is dead, NBD detached and the device unused, then opens
the same private owner/journal under the exclusive owner lock. Pending journal
writes refuse collection. The collector never treats unreplicated data as
throwaway cache and does not release remote ownership between passes. The pass
holds the sandbox operation lock and excludes guest I/O and publication through
the owned-disk guard. It marks the full validated current root, metadata pages and
data references before deleting any unreferenced object. Missing/corrupt pages,
changed ownership or unknown checkpoint metadata fail closed.

Passes scan at most 128 ordered keys using an exclusive `start-after` key. The
last successfully scanned key is persisted; interrupted work rechecks references
before retrying. An empty page completes the cycle. A marked reference set is
cached only for the exact volume/head revision, avoiding repeated metadata GETs
across batches. A publication invalidates it. The set uses a sorted vector of
32-byte hashes, at most 1,049,600 entries (about 32.03 MiB) for the maximum 64-GiB
disk; it is dropped on a mutating foreground operation or cycle completion. It is
disposable and never serialized as authority to delete data. Partial cycles retry
after one second. On completion the supervisor attempts local eviction. Cold
disks skip maintenance until reactivated; failed eviction retries with backoff.
The private `usage` reply includes `last_collection_unix` for the last completed
cycle. The remote disk keeps its logical capacity reservation.

Foreground mutations cancel an admitted collector between bounded store requests.
They wait up to ten seconds for it to yield before returning a retryable busy
error. Competing ordinary lifecycle operations still fail fast. Read-only polling
does not continually cancel collection. One collector runs at a time across both
deleted and stopped disks. No per-I/O accounting work is added to running guests.
A large first metadata walk and per-object deletes can still be slow; bulk delete
and metadata throughput optimization remain separate work.

This is not online collection for VMs that never stop, checkpoint creation or
checkpoint expiry. Local eviction follows a completed collection cycle as below. Checkpoint metadata requires a format/reference
extension that this strict collector understands before it can be enabled. Age
alone never authorizes deletion. Here, exclusive offline ownership and an empty
backlog provide the safety boundary instead of a wall-clock grace period.

### Stopped-disk qualification

On agent_house, `live_reclaim_probe` overwrote a one-chunk disk, rejected collection
while writes were pending, resumed collection after reopening its owner, and
reduced **four immutable objects to two**. Current and peer reads remained correct.
The small R2 collection probe took **3.74 seconds**. Its test disks were subsequently
deleted through the retired-volume collector; two tiny retirement markers remain.

An isolated supervisor probe imported a 128-KiB image, verified no collection raced
prepare, explicitly detached it, and observed automatic collection in **3.83
seconds**, with the current disk and reservation retained. Deletion then cleaned
its data and released the reservation. No VM was created. Temporary service/NBD
module/files/copied credentials were removed, and installed services were unchanged.
This probe retains one additional tiny retirement marker.

Tests cover stale reference-cache invalidation, missing metadata, unknown
checkpoint fields, pending journal refusal, zeroed disks, failed-delete/restart
recovery, cursor advancement past retained objects, peer isolation, cancellation
and foreground waiting. These timings qualify small scenarios, not bulk storage
throughput or a VM boot-time promise.

Validation for this continuation: Linux has 88 ordinary volume tests, seven
explicit root-only tests and the launch test passing. Formatting and clippy with
warnings denied pass. CI also runs the ordinary suite unprivileged, followed by
the privileged service tests, so root-only coverage is not silently skipped.


## Idle local eviction

After an explicit detach (including a daemon idle stop), the supervisor completes
stopped-disk collection, confirms remote sync, and removes the local journal.
There is no second idle timer here: the daemon decides when an inactive VM stops.
No running VM is interrupted for eviction. Sync/ownership/cleanup failures keep
local reservations and retry; pending writes are never treated as disposable cache.

The owner file lock covers remote sync and journal removal. A durable local intent
allows interrupted removal to finish before journal replay on restart. The small
owner identity remains on this host, so eviction does not release ownership or
permit another host to take over. The current disk remains in R2 indefinitely
until explicitly deleted; eviction is not checkpoint expiration or a backup.

Only after local deletion is durable does the record become `evicted`. That frees
its NBD slot, 512-MiB journal reservation and 64-MiB clean-cache reservation. Remote
logical capacity stays charged. The old device path is historical and cannot be
used by cold detach/delete/inspect operations to affect a subsequent slot owner.
The private `usage` response exposes `local_evicted` and the remaining reservations.

Attach reserves a free device and local budgets under the registry lock, persists
the reservation, then lazily opens the remote disk using the retained identity.
It does not reimport the source image. Insufficient local capacity returns a
retryable admission error with the remote disk unchanged. A private sync request
also requires local readmission. Cold disks do not repeatedly recreate journals
for background collection. No public storage flag or cloud default is enabled yet.

### Local eviction qualification

On agent_house, an isolated R2 probe used two 128-KiB disks and one NBD slot, with
no VMs. It wrote and remotely synced a marker through the real block device,
evicted the first disk, reused the slot for the second, and refused first-disk
readmission while capacity was occupied. Both disks became cold, the supervisor
restarted, the original image was removed, and the first disk lazily reopened with
its marker intact. Cold deletion then reclaimed both disks and released every
reservation. Two tiny remote retirement markers remain; test chunks were reclaimed.

Regression coverage includes repeated eviction/reopen, exclusive owner refusal,
remote failure retaining pending journal data, crash recovery from partially
removed journal files, local capacity readmission without double charging, and
cold deletion. Linux: 91 ordinary tests, eight explicit root-only tests and the
launch test pass. macOS: 79 volume tests pass. Formatting and clippy with warnings
denied pass. Temporary service files, credentials and NBD module were removed;
installed AHVM services stayed active. This is a small correctness probe, not a
large-disk eviction or boot-time benchmark.

## Accounting retirement handshake

The trusted daemon can send `retire` with `volume_id`, the original `sandbox_dir`
and `logical_bytes`. The reply's `reclamation_complete` is required: `false` means
delete intent was recorded; only `true` confirms local and remote reclamation.
Timeouts, omitted confirmation and mismatched identities never release tenant quota.
The engine validates lifecycle ownership before issuing this host-only operation.

For a never-registered identity, the supervisor records an already-deleted, cold
record without an image import, journal/cache reservation or NBD attachment. Its
logical size is validated and held until cleanup. The original sandbox directory
may be absent, but must be directly under the configured engine root. Remote
ownership checks still apply: missing local ownership cannot authorize deletion
of another host's owned disk. Small local/remote tombstones remain permanently.
Existing records must match the original sandbox binding and logical size.

Set `AHVM_VOLUME_SOCKET` on the daemon to this service's private socket for
reconciliation. No installer enables this service or replicated creation yet.
The daemon's separate deletion reconciler cannot stall the thermal idle sweep.

This continuation passes 53 Linux engine tests, 91 ordinary volume tests and ten
explicit root-only tests; the daemon has 25 unit and 24 HTTP tests passing, and
15 store tests pass. macOS engine tests (46), daemon/store regressions and volume
tests (79) pass. Formatting and clippy with warnings denied pass. The small live
R2/daemon handshake is recorded in the durable-storage plan; no VM or installed
service was changed.
