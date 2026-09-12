# Indexed root-disk experiment

The strict remote-flush contract documented here is now the comparison path.
See [local durability with eventual replication](EVENTUAL.md) for the owner-approved
product direction and its faster fsync behavior.

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
- At most eight concurrent object uploads. On a read miss, up to seven adjacent
  chunks are fetched speculatively without waiting for them. There is no queue;
  seven pending reads is a per-cache hard cap. The demanded read proceeds
  independently. Cache hits skip read-ahead. Speculative work can overlap a
  commit (up to fifteen object operations across those two paths), and can finish
  after the cache handle is dropped; the example bounds each request to three
  seconds. No speculative failure can manufacture a successful demanded read.
- Commits skip data and index pages identical to their committed hashes. If all
  writes are no-ops, flush behaves like a clean flush and does not republish.
  This relies on the same immutable-object/reference contract as ordinary reads;
  it does not infer remote durability from an uncommitted cache entry.
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
cargo build --release --manifest-path rust/Cargo.toml --locked \
  -p ahvm-volume --example indexed_nbd
sudo modprobe nbd nbds_max=4 max_part=0
sudo python3 experiments/durable-storage/kvm-data-disk.py \
  --indexed-root --metrics --warm-repeat --stress-cycles 3 \
  --config "$HOME/.config/ahvm-volume/r2.json" \
  --server "$PWD/rust/target/release/examples/indexed_nbd" \
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

`--metrics` enables aggregate object-operation counts and elapsed time in the
sidecar logs. Times sum across concurrent calls, so they are not wall-clock
latency. PUT counts are logical store calls (a duplicate PUT may also verify via
GET internally). No object names, payloads or credentials are logged. The gate
reports Git/package/SQLite/final-sync time separately; total workload time also
includes cold Python startup and imports. Use a release build for comparisons.

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


## Performance investigation (2026-09-13)

The prior 87.50-second result used a debug build. An instrumented release-build
baseline took 94.21 seconds, so compiler optimization did not remove the delay.
Cold R2 reads averaged about 176 ms across 1,002 logical GETs by the end of the
workload. Synchronous commits also required many remote round trips. Stage times
exclude initial Python startup/imports, which account for the remaining total.

The new implementation avoids waiting for speculative reads and skips unchanged
committed chunks/pages. Initial release-build comparison:

| Stage | Baseline | Optimized run |
| --- | ---: | ---: |
| Git init/add/commit | 5.49 s | 4.54 s |
| Local wheel installation | 38.00 s | 26.78 s |
| Three SQLite FULL transactions | 19.00 s | 17.54 s |
| Final sync | 4.49 s | 3.50 s |
| Whole cold workload, including Python startup/imports | 94.21 s | 72.58 s |

These are individual runs, not percentiles or a guaranteed speedup. R2 latency
also varied between runs. The first optimized run issued more logical GETs
(1,093 vs 1,002) while completing sooner: asynchronous speculation trades some
extra reads for latency, so this is not a claim of lower total request cost.
The deterministic tests separately prove the seven-request speculative cap,
nonblocking demanded reads and absence of uploads/publications for no-op writes.

The optional `--warm-repeat` runs the same workload in a new directory while the
cache is warm. `--stress-cycles 3` then rewrites a 4-MiB file with a different
pattern per cycle, fsyncs file and directory, SIGKILLs compute/storage, and checks
every byte after reopening from R2. Only one small VM runs at a time.

Further latency work should target object granularity and commit amplification:
qualified read packs/larger fetches for cold images, and fewer remote operations
for small synced changes. Neither a warm-cache result nor a local-only flush is a
substitute for demonstrating durable cold recovery. Phase 3 still needs the
remaining block-operation qualification before engine integration.

Confirmation run: **67.19 s cold**, then **23.25 s warm**. The warm stages were
Git <0.01 s, package install 4.36 s, SQLite 15.19 s and final sync 3.66 s.
This separates cold-read overhead from durable transaction cost: even a warm
cache does not make these synchronous commits fast enough yet.

Both optimized runs passed initial Git/package/SQLite recovery and the R2 outage
check. The confirmation additionally passed all three 4-MiB rewrite/SIGKILL/
fresh-process recovery cycles. Its outage recorded **163 denied packets** and
fsync failed as required. These are completed-write crash tests, not a claim that
an arbitrary unacknowledged write survives a crash at every instruction.

**37 tests** pass on macOS and Linux; Clippy with warnings denied, formatting and
Python compilation pass. Test compute, NBD module/attachment, injected firewall
rules and copied credentials were removed; all four existing services stayed
active. Qualification uses the experimental sidecar only, not a production
engine rollout.
