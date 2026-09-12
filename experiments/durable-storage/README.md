# Guest disk attachment spike

This is the first part of storage phase 3, not the completed durable backend.
The existing VMM boots an ordinary local Ubuntu root and attaches a **64-MiB raw
ext4 data disk backed by R2** through a private Unix NBD socket and `/dev/nbdN`.
No fork changes, CLI flags, production service changes or release are included.

## Attachment decision

| Option | Findings | Decision for this spike |
| --- | --- | --- |
| NBD | Available on `agent_house`; imago already accepts host block devices. The current Linux VMM's write-back path calls `flush()` then `sync()`/`fsync()` for guest flush requests. The live test below exercises that chain. | Use a single local connection and explicit device attachment. |
| ublk | Driver available on the host, with io_uring queues and recovery facilities. Needs another control/queue implementation and kernel capability qualification. | Reconsider if NBD overhead becomes material; no comparative throughput claim yet. |
| Direct VMM storage adapter | Would require a new integration into libkrucible/imago and a stable service interface. | Defer until the disk service semantics and ownership model are settled. |

The NBD implementation processes requests sequentially. Ordinary writes remain
volatile until FLUSH or FUA succeeds; those acknowledgements wait for the remote
volume commit. Errors return NBD EIO, and invalid ranges/flags return EINVAL.
Unknown/oversized protocol frames cannot allocate unbounded memory. The default
maximum request is 32 MiB for EXPORT_NAME compatibility; the gate limits normal
kernel requests to 1 MiB. INFO/GO also reports the limits explicitly.

Only read, write, flush and FUA are supported. The NBD server does not advertise
trim, write-zeroes or multiple connections. The VMM still has its existing
virtio feature set: its discard/zeroing paths are **not qualified by this gate**
and must be reviewed before product integration. The gate disables mkfs discard.

No listener is exposed over TCP. The example requires a private socket directory,
serves one connection, never overwrites an existing socket and does not reconnect
another client to an existing volume handle. Restart means a fresh process and a
fresh handle opened from the R2 head. This is not ownership fencing or live
failover. Credential files are never mounted into the guest.

## Run on a disposable Linux/KVM test host

Prerequisites: existing AHVM VMM/libkrun bundle, Ubuntu dev image, the private R2
configuration from [phase 2](../../docs/DURABLE-STORAGE-R2.md), and an unused NBD
device. The helper runs as root for device attachment and firewall injection;
the storage process drops to a checked-unused numeric UID. It creates no account.

```bash
sudo apt-get install nbd-client
sudo modprobe nbd nbds_max=4 max_part=0
cargo build --manifest-path rust/Cargo.toml --locked \
  -p ahvm-volume --example nbd_serve
sudo python3 experiments/durable-storage/kvm-data-disk.py \
  --config "$HOME/.config/ahvm-volume/r2.json" \
  --server "$PWD/rust/target/debug/examples/nbd_serve" \
  --vmm /opt/ahvm-rust/bin/ahvm-vmm \
  --lib /opt/ahvm-rust/lib \
  --image /opt/ahvm-rust/share/base.ext4 \
  --device /dev/nbd0
```

The helper reserves its test device with a lock and refuses an attached device
or an already-used sidecar UID. It creates exactly one VM at a time, with 1 CPU
and 1 GiB RAM. The finalizer removes both IPv4/IPv6 fault rules, terminates compute
and storage, disconnects the NBD device, and deletes its credential copy and local
root overlay. Small logs remain in the printed temporary directory. R2 objects
remain under the printed **exact fixture prefix** for explicit cleanup; this is
not a GC implementation. Do not interrupt the helper with SIGKILL: like any root
fault-injection tool, its finalizer must run to remove injected rules.

## What passed on agent_house

- Guest ext4 formatting/mounting over the complete VMM → host NBD → Rust storage
  → R2 path. Both host NBD and guest-facing VMM use write-back flush semantics.
- Git init/add/commit; pip installation of a small local wheel onto the data disk;
  three SQLite transactions with `synchronous=FULL`; file fsync and filesystem
  sync. This is a small correctness workload, not a large apt/network benchmark.
- SIGKILL of both VMM and storage process, NBD disconnect, and removal of the
  local root overlay. New processes and a new root overlay then mount the same
  R2 data disk. Git fsck/content, the installed Python package, file marker and
  SQLite integrity/rows all pass. No local data-disk files or RAM snapshots exist.
- Deny outbound TCP/443 for **only the sidecar UID**, in both IPv4 and IPv6.
  The guest can write, but its subsequent `os.fsync` raises an I/O error. The gate
  requires a nonzero deny counter; the final run recorded 46 denied packets.
- Qualification cleanup verified all five test prefixes (including failed trial
  runs) empty in R2; no test worker, NBD attachment, sidecar credential copy or
  firewall rule remains. The four existing AHVM services remain active.
- 25 crate tests pass on macOS and Linux; Clippy and formatting pass. Tests cover
  flush/FUA failure, ordering, bounds, unsupported flags and legacy request sizes.

The two issues found during qualification were a too-small legacy NBD request
limit and an IPv4-only outage rule that left the actual IPv6 connection working.
Both are covered in the current transport/gate. It would be incorrect to call
an outage test successful without proving the fault was applied.

## Still required in phase 3

The disk map is still the bounded phase-1 JSON map. Reads fetch chunks directly;
there is no clean cache, indexed metadata, local write log or background upload.
Dirty data is bounded by the small logical volume, not yet by production
backpressure. An entire commit also needs a budget/batching policy before large
disks; individual S3 request deadlines alone are insufficient.

Next: indexed maps and bounded caching/upload batches, then a full durable root
disk and repeatable recovery under write load. Qualify root boot, ext4 ordering,
discard/zeroing and larger workloads before choosing production limits. Complete
ownership fencing separately in phase 4. Do not enable this example for customers.

References: [NBD protocol](https://github.com/NetworkBlockDevice/nbd/blob/master/doc/proto.md),
[ublk kernel documentation](https://docs.kernel.org/block/ublk.html),
[storage plan](../../docs/DURABLE-STORAGE-PLAN.md).
