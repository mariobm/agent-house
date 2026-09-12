# Indexed root-disk experiment

This continues phase 3. It does not change the installed engine, local mode,
CLI, production storage, or release artifacts. Format 1 remains supported by
its original experimental implementation; there is no automatic conversion.

## Format and durability

Format 2 stores a bounded JSON head with hashes of immutable index pages. Each
64-KiB page maps 1,024 data chunks (64 MiB of logical disk). Each data chunk is
64 KiB; absent entries are explicit zero holes. Pages encode their position,
validate reserved bytes, and cannot reference slots past the logical disk.
Reads verify both lengths and SHA-256 hashes; missing objects are errors.

The current logical size limit is 64 GiB, with at most 1,024 page references
and a 128-KiB head limit. This is an implementation bound, not a tested product
capacity. Opening fetches only the head; pages and data load on demand.

Commits upload immutable data and modified pages before conditionally replacing
the head. Upload failure leaves the previous head and dirty data intact.
Ambiguous publication or a conflicting writer poisons the handle and requires
reopen. Conditional publication alone is not ownership fencing (phase 4).
Imports publish `ready: false` first and only become openable after the final
commit. Interrupted imports need explicit cleanup and a new volume ID.

## Bounded work

- 64-MiB verified FIFO cache in the example, scoped by volume and hash. Mutable
  heads are never cached. No persistent cache is needed to reopen a volume.
- 64-MiB dirty-data limit and 32-MiB request limit. Filling the dirty buffer
  forces a commit before accepting more data; failure applies backpressure.
- One serialized writer; periodic commits every second with five-second retry
  delay after failure. Foreground FLUSH/FUA waits for remote publication.
- At most eight concurrent object uploads. On a read miss, up to eight adjacent
  chunks are fetched concurrently. Cache hits skip read-ahead; speculative
  failures do not turn a successful requested read into missing data.
- A 30-second commit admission budget, checked between operations and before
  publication. The example uses three-second HTTP request timeouts. This is not
  a hard 30-second cancellation deadline: an admitted operation may finish later,
  and immutable-object collision verification may require another request.

The cache and dirty limits are not a total RSS limit: metadata, staged writes,
HTTP buffers and worker stacks also consume memory. Background commits serialize
with reads and writes, so slow R2 requests can stall guest I/O. Cold reads and
small synchronous transactions need performance qualification before rollout.

Ordinary writes are volatile until a remote commit succeeds. There is **no local
write-ahead log yet**. A log could preserve unsynced data across a sidecar restart,
but cannot replace the remote flush barrier for host-loss durability. That plan
item remains explicit; this experiment does not promise unsynced-write recovery.

## Reproduce on a disposable Linux/KVM host

Use the private S3 config described in [phase 2](../../docs/DURABLE-STORAGE-R2.md).
Prepare a small Alpine root with a statically linked x86_64-musl forge. The helper
requires a new output directory, verifies the base archive against its published
checksum, and installs Git, Python/pip and ext4 tools. Package versions follow
Alpine's repository; this is a qualification fixture, not a reproducible release
image. It executes the downloaded userland via chroot and requires root.

```bash
sudo sh experiments/durable-storage/prepare-root.sh \
  /tmp/ahvm-indexed-root "$PWD/rust/target/x86_64-unknown-linux-musl/release/ahvm-forge"
cargo build --manifest-path rust/Cargo.toml --locked \
  -p ahvm-volume --example indexed_nbd
sudo modprobe nbd nbds_max=4 max_part=0
sudo python3 experiments/durable-storage/kvm-data-disk.py \
  --indexed-root \
  --config "$HOME/.config/ahvm-volume/r2.json" \
  --server "$PWD/rust/target/debug/examples/indexed_nbd" \
  --vmm /opt/ahvm-rust/bin/ahvm-vmm --lib /opt/ahvm-rust/lib \
  --image /tmp/ahvm-indexed-root/root.ext4 --device /dev/nbd0
```

The gate imports the root into a unique private R2 volume and removes its import
copy before boot. The VMM receives only the NBD device as its root; no local root
overlay or RAM snapshot is used. One 1-CPU/1-GiB guest runs Git, a local wheel
install and synchronous SQLite transactions, then both compute and storage are
SIGKILLed. New processes recover the same remote root with an empty RAM cache.
The final outage test denies only the storage UID's TLS traffic and requires a
failed fsync plus nonzero firewall counters. Cleanup disconnects NBD before
waiting for a VMM that may be stuck in device I/O, and always removes credential
copies. As with the earlier gate, fixture objects require explicit prefix cleanup.

## Remaining before product integration

Qualify virtio discard/write-zeroes behavior, recovery under sustained write
load, production-size roots, cache pressure, request costs and latency. There is
no GC, local WAL, installer integration, writer lease, automatic failover, or
customer-facing durable mode in this change. Unreferenced immutable objects
accumulate until fixture cleanup; phase 5 will provide safe reclamation.

## Qualification result (agent_house, 2026-09-12)

- A 512-MiB logical Alpine root (about 91 MiB allocated in the source image)
  booted entirely through NBD/R2 with 1 CPU and 1 GiB RAM.
- Git commit, local-wheel installation and three synchronous SQLite transactions
  completed and synced. SIGKILL of both processes followed by fresh processes
  and an empty storage cache recovered Git content/fsck, the package, file marker
  and SQLite integrity/rows.
- The cold workload took **87.50 seconds** with bounded read-ahead. The earlier
  serial-read trial exceeded its 180-second timeout. This is one small debug-build
  qualification run, not a throughput benchmark or a product latency target.
- Blocking both IPv4/IPv6 TLS for only the storage UID produced an fsync error;
  the firewall recorded **109 denied packets**. No false durable acknowledgement.
- **35 tests** pass on macOS and Linux; Clippy with warnings denied and crate
  formatting pass. Tests include publication ordering, stale/ambiguous writers,
  incomplete imports, corruption, cache bounds, dirty-buffer backpressure,
  background/foreground commit ordering and speculative read failures.
- Test compute and storage stopped, NBD detached/module unloaded, injected rules
  and copied credentials removed. Existing AHVM services remained active.
