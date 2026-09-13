# Replicated volume service

`ahvm-volumed` is the Linux Rust supervisor for optional replicated disks. It
uses `OwnedDisk`, the local journal and asynchronous S3-compatible replication.
Guest fsync does not wait for R2. The default daemon/CLI storage mode remains
local; no object store is required for self-hosted local disks.

This service replaces the Python qualification adapter. It is included in the
Linux server bundle but **not enabled by the installer**. Quota integration,
remote reclamation and daemon/API selection must land before cloud activation.
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
cannot grow past that reservation on an import retry. Each record also reserves
512 MiB for journals (a 256-MiB log plus its simultaneous compaction replacement)
and 64 MiB of clean-cache payload. These values share the worker's actual bounds.

Failed imports, stopped disks and deleted-but-retained records remain charged.
Retries do not charge twice. Until reclamation lands, repeated create/delete can
exhaust a budget; raising it requires an explicit config change and service
restart. Do not remove owner/journal records to evade the budget. A reservation
write failure freezes further imports until restart reconstructs the durable
registry. Lowering a budget preserves existing disks and blocks new admission
while over budget; it does not kill workers or discard pending writes.

This is conservative admission accounting, not filesystem preallocation. Keep
headroom for metadata, other applications and filesystem overhead. The cache
budget is not a process RSS cap: dirty buffers, maps, in-flight requests and other
allocations are additional. Remote historical objects remain unbounded until GC.
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
- Stop/detach retain remote ownership and the private journal for restart.
  Delete records a tombstone before cleanup. **Remote objects, tombstones and
  owner directories are retained** until the accounting/GC phase. No scheduled
  backups, automatic cross-host takeover or implicit mode conversion is added.

The service exclusively locks its configured device pool. Currently one slot is
reserved per non-deleted volume, including stopped volumes; successful logical
delete frees the device slot. The pool is bounded to 32 devices, the registry to
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
