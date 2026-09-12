# Local durability with eventual R2 replication

Owner-approved direction, 2026-09-13. This is the new durable-backend target,
still isolated from the installed engine/CLI. Self-hosted local mode is unchanged.
The earlier strict remote-flush experiment remains available for comparison.

## Contract

| Operation/failure | Behavior |
| --- | --- |
| Ordinary write | Appends to the local journal; not yet a durability acknowledgement. |
| Guest fsync / disk FLUSH / FUA | Syncs the host journal. Does **not** wait for R2. |
| Remote-sync barrier | Publishes all writes captured at the barrier to R2, or returns an error. Later concurrent writes may remain pending. |
| Storage/VM process crash, host disk retained | Replays the local journal against the remote generation. |
| Permanent host-disk loss | Recovers the last remote generation. Newer locally synced data can be lost. |
| R2 outage | Local fsync can succeed; remote sync fails. Uncached reads can still fail, and a full dirty backlog rejects new writes. |
| Local journal I/O failure | Fails local fsync/writes; never acknowledges data through a failed or unlinked journal descriptor. |

This is eventual **durability**: the active writer reads its own writes immediately.
Replication runs periodically, but there is no guaranteed one-second data-loss
window. Outages and slow uploads extend the lag. Planned migration/checkpoint or
any action promising remote recovery must explicitly drain to R2 before proceeding.
A restarted sidecar currently needs R2 access to open/reconcile its base generation.
This prototype does not yet provide a persistent cache of clean remote blocks.

## Implementation

`LocalDisk` journals full 64-KiB chunk images with monotonic write sequences and
SHA-256 checksums covering record metadata and data. Its header is checksummed too.
Local flush calls `sync_all`; journal creation/replacement also syncs the containing
directory. A stable separate lock file prevents two processes opening the same
journal. The directory and files must be private; symlinks are rejected.

A bounded dirty map supplies immediate read-your-writes. The replication worker
captures immutable chunk versions, releases the foreground mutex, then uploads
and publishes the remote generation. New writes can proceed while that upload
is stalled. After publication it removes only versions covered by the commit,
retaining newer writes. Compaction atomically replaces the journal with the latest
pending versions; any compaction error poisons local I/O until reopen.

Format **3** adds a journal identity/sequence watermark to the format-2 root/page
layout. That watermark reconciles the crash between remote publication and local
journal compaction, including lost publication responses. A conflicting owner
fails closed. This is not complete multi-host ownership fencing; phase 4 still
needs leases/fencing and lifecycle integration. Existing format-2 images can seed
an experimental journal; its first replication publishes format 3. Strict commits
on format 3 are refused. Older readers reject format 3; no product migration ships.

Limits: 64 MiB of distinct pending chunks, 256 MiB journal before compaction, and
32 MiB per request. Compaction can temporarily use another roughly 64 MiB on disk.
RAM also includes captured/staged chunks, index pages and the 64-MiB clean cache;
the dirty/cache limits are not a total RSS quota. Remote replication has a
120-second admission budget and three-second individual HTTP timeouts; the last
admitted operation can finish later. Large/scattered backlogs and request costs
still need qualification. A full backlog applies write backpressure, never data
loss to make room. Immutable unreferenced R2 objects still need explicit cleanup
until GC is implemented.

## Run the qualification

Prepare the small root and private R2 config as described in [the indexed-root
experiment](INDEXED-ROOT.md), then:

```bash
cargo build --release --manifest-path rust/Cargo.toml --locked \
  -p ahvm-volume --example indexed_nbd
sudo modprobe nbd nbds_max=4 max_part=0
sudo python3 experiments/durable-storage/kvm-data-disk.py \
  --indexed-root --eventual --metrics --warm-repeat --stress-cycles 1 \
  --config "$HOME/.config/ahvm-volume/r2.json" \
  --server "$PWD/rust/target/release/examples/indexed_nbd" \
  --vmm /opt/ahvm-rust/bin/ahvm-vmm --lib /opt/ahvm-rust/lib \
  --image /tmp/ahvm-indexed-root/root.ext4 --device /dev/nbd0
```

`indexed_nbd serve CONFIG ID SOCKET JOURNAL_DIR` uses local durability.
`serve-strict CONFIG ID SOCKET` retains the strict experiment. A private Unix
control socket replaces the NBD socket's extension with `.control`. The example
command `indexed_nbd sync CONTROL_SOCKET` requests a remote barrier. Its result
contains local/remote sequence watermarks, pending bytes, local I/O failure and
replication failure status. These are experimental service commands, not released `ahvm` CLI syntax.

The gate keeps the journal for a process restart, then removes it after a remote
barrier to prove independent R2 recovery. Its outage check expects local fsync to
succeed and remote sync to fail. It kills both processes before lifting the fault,
removes the journal, and proves the pending outage write is absent on recovery.
Only one 1-CPU/1-GiB VM runs at a time. Cleanup removes private local files, processes,
NBD attachment and firewall rules; R2 fixtures require explicit prefix cleanup.

## agent_house results (2026-09-13)

| Measurement | Strict experiment | Eventual experiment |
| --- | ---: | ---: |
| Three SQLite FULL transactions, cold workload | 15–19 s | **0.38 s** |
| Entire cold Git/package/SQLite workload | 67–94 s | **36.93 s** |
| Entire warm repeat | 23.25 s | **0.30 s** |

The warm SQLite time rounded to 0.00 s at the gate's two-decimal precision. These
are small release-build observations, not percentiles or a throughput guarantee.
The faster acknowledgement intentionally provides local rather than immediate
remote durability. Cold data fetches still dominate the first run.

Passed: journal-preserving process-crash recovery; explicit remote barrier then
journal removal and Git/package/SQLite recovery; an additional 4-MiB synced-write/
remote-barrier/host-loss cycle; local fsync during R2 outage; failed remote barrier
with seven denied packets; and deliberate loss of the unreplicated outage write
after removing the host journal. No installed services were modified.

48 crate tests pass on macOS and Linux, including stalled-upload admission,
local replay, ambiguous remote publication, newer writes during replication,
foreign-owner conflict, backlog bounds, checksummed/torn journal handling and
poisoning after journal/compaction errors. Clippy with warnings denied, formatting
and Python compilation pass. The qualification R2 prefix was deleted and verified
empty; NBD, journal/credential copies and test processes were removed, and all four
existing AHVM services remained active.
