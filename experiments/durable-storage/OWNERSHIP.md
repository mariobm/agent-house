# Exclusive writer ownership

Phase 4 ownership core, isolated from the engine's existing qualification adapter.
No installed service or public storage mode changes. This is explicit handoff,
not automatic failover. Service integration and supervision remain next.

## Protocol

Format 4 wraps the indexed format-2/3 map in an ownership envelope. The owner ID,
monotonic epoch and disk map are published together using the same conditional
head replacement. A separate ownership object would leave a race between checking
the owner and publishing data; this format avoids that two-object transaction.
Immutable chunks/pages and their hashes are unchanged. Legacy readers reject the
new envelope. No production volume is migrated by this change.

`OwnedDisk::enroll` is an explicit **offline-only** bootstrap for an existing
indexed image. All legacy writers must be stopped before enrollment; they do not
implement the new I/O guard. Enrollment is idempotent and cannot take an owned
volume away from its writer. Normal `OwnedDisk::open` refuses unenrolled images.

Acquisition increments the epoch and installs a random owner identity only if the
head is unowned. Competing acquisitions have one CAS winner. The identity is
fsynced in a private `owner.json` (including directory entries) before claiming; a separate file lock prevents
two local processes using the same owner directory. A lost claim response can be
reconciled by restarting with that same identity. The directory is bound to the
volume ID. Retain it with the local journal; never clone the ownership directory
to another host or run two services with copies of one identity.

Service-process death does not grant another owner permission to take over. A
restart with the original private directory resumes the claim and replays locally
synced writes. Another host must use a fresh identity after explicit release.
There is no expiration timer, clock synchronization assumption or force flag.
Loss of the owning host/identity requires an operator-fencing recovery procedure,
which is not implemented here; it must not become an automatic takeover shortcut.

## Handoff and I/O

The supervising service must stop its VM **before** calling `release`. The wrapper
excludes concurrent I/O, drains the journal to R2, closes local I/O admission, and
conditionally clears ownership. A failed drain leaves the owner active so the
caller can retry. Once release publication begins, local read/write/flush and
replication are permanently disabled for every clone of that handle, even if the
reply is lost. Retrying release reconciles the remote head and never clears a
later owner's claim. Released identities cannot be reopened to regain ownership.

All guest access must use `OwnedDisk`; its inner `LocalDisk` and store are private.
Normal read/write/fsync use a shared admission guard. Replication can upload while
a foreground write/fsync progresses. Guest fsync still syncs only the local
journal: ownership adds no R2 round trip to that path. Release is deliberately
exclusive and may wait for an in-flight upload. Immutable speculative read-ahead
may finish after release, but cannot publish or serve new guest requests.

The protocol protects cooperating trusted storage services against stale writers;
it is not an authorization boundary against a host deliberately using its raw
bucket credentials to overwrite the head. The next service integration must bind
ownership to one host/service identity, enforce VM termination before release,
and supervise NBD/worker lifetimes. These primitives alone do not claim that an
old VM has been terminated or that the engine already enforces multi-host fencing.

## Qualification

Build the independent-process probe and run it with the existing private R2
qualification config:

```bash
cargo build --release --manifest-path rust/Cargo.toml --locked \
  -p ahvm-volume --example ownership_probe
python3 experiments/durable-storage/ownership-gate.py \
  --config /path/to/private/r2.json \
  --probe rust/target/release/examples/ownership_probe
```

It creates one disposable 1-MiB volume, claims it, writes and fsyncs locally without
replicating, rejects a second owner, SIGKILLs the first process, rejects takeover
again, resumes the original identity/journal, drains/releases, and verifies data
from a fresh owner's directory. The released old identity is refused. No VMs or
NBD devices are created. Local identity/journal fixtures are removed automatically;
the printed R2 prefix requires explicit cleanup, as with the earlier gates.

This sequence passed against real R2 on `agent_house`. Deterministic tests also
cover concurrent acquisition, lost claim/release replies, stale-handle I/O,
volume/identity mismatch, explicit legacy enrollment, offline fsync and blocked
takeover, plus local writes during a stalled upload and subsequent drain of those
newer writes. Format-4 support remains opt-in until service integration is ready.

58 volume tests pass on macOS and Linux. Clippy with warnings denied, formatting
and Python compilation pass. The qualification prefix was deleted and verified
empty; local identity/journal and credential copies were removed. Installed
services remained active and no VM was created for this protocol gate.
