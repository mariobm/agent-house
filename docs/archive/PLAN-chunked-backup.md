# Content-Defined Chunking Backup

## Problem

A 5GB volume where 50MB changes between backups. The current backup
implementation uploads the entire file (compressed to ~2GB) every time.
With hourly backups that's ~48GB/day of upload for 50MB of actual changes.

## Solution

Content-defined chunking (CDC) with dedup. Split the volume into
variable-size chunks using a rolling hash. Identical chunks (identified
by SHA256) are uploaded once and shared across all backups.

## How CDC works

A rolling hash (Rabin fingerprint) slides over a window of bytes. When
the lowest N bits of the hash are zero, that's a chunk boundary. This
produces ~1MB average chunks with stable boundaries — inserting data in
the middle only affects 1-2 chunks, not all subsequent ones.

We use `github.com/restic/chunker` — the same chunker restic uses.
655 lines, BSD-licensed, battle-tested, pure Go, zero transitive deps.
~100KB binary impact.

### Why restic's Rabin chunker, not buzhash

Rabin fingerprinting treats the byte stream as a polynomial over GF(2)
and computes the hash as the remainder when divided by an irreducible
polynomial. The collision probability is provably bounded.

Buzhash is simpler (~50 lines vs ~650) but has weaker distribution
guarantees. For a backup system where a hash collision means data loss,
the mathematical rigor of Rabin is worth the extra code. And since we're
importing restic's implementation rather than writing our own, the
complexity is not our maintenance burden.

## Architecture

```
pkg/backup/
  backend.go        — existing Backend interface (Upload/Download/List/Delete)
  s3.go             — existing S3 implementation with SigV4
  mem.go            — in-memory backend for tests
  dedup.go          — Backup/Restore/Prune orchestration
  dedup_test.go     — comprehensive tests against mem backend
```

## S3 key layout

```
chunks/{sha256-hex}                             — zstd-compressed chunk data
manifests/{volume-name}/{RFC3339-timestamp}.json — ordered list of chunk hashes
```

Chunks are content-addressed and shared across all volumes and all
backups. A chunk is only deleted during prune when no manifest
references it.

## Data structures

```go
type Manifest struct {
    Volume    string    `json:"volume"`
    Timestamp time.Time `json:"timestamp"`
    Size      int64     `json:"size"`       // original file size
    Chunks    []string  `json:"chunks"`     // ordered SHA256 hashes
}

type BackupStats struct {
    ManifestKey   string
    ChunksTotal   int
    ChunksNew     int        // actually uploaded
    ChunksReused  int        // already existed in S3
    BytesUploaded int64      // compressed bytes sent
    Duration      time.Duration
}
```

## Flows

### Backup

```
1. Acquire per-volume mutex (prevents cron + manual overlap)
2. Open volume file
3. Create restic Chunker(file, polynomial, min=512KB, max=8MB)
4. For each chunk:
   a. SHA256 the chunk → hash
   b. HEAD s3://chunks/{hash} — exists?
   c. If yes: record hash, skip upload (dedup)
   d. If no: zstd compress → PUT s3://chunks/{hash}
5. Upload manifest to s3://manifests/{volume}/{timestamp}.json
6. Release mutex
7. Return stats
```

### Restore

```
1. Download manifest JSON
2. Create output file
3. For each chunk hash in order:
   a. GET s3://chunks/{hash}
   b. zstd decompress
   c. SHA256 verify (detect corruption)
   d. Write to output file
4. Verify total file size matches manifest
```

### Prune

```
1. List all manifests for the volume
2. Sort by timestamp, keep newest N
3. Delete old manifest files
4. Collect all chunk hashes referenced by ALL remaining manifests
   across ALL volumes (a chunk may be shared)
5. Delete unreferenced chunks
```

## Locking

In-process `sync.Mutex` per volume name inside the server. Prevents
concurrent backup of the same volume (cron fires while manual backup
still running).

No S3-level locking — single bhatti daemon per host. If multi-node is
added later, S3-based locking (write a lock file, check for stale
locks) can be added without changing the chunking logic.

## Expected performance

For a 5GB volume with 50MB of changes:

| Operation | First backup | Subsequent |
|-----------|-------------|------------|
| Chunks processed | ~5000 | ~5000 |
| Chunks uploaded | ~5000 | ~50 |
| Data uploaded | ~2GB (compressed) | ~20MB |
| Time | ~30s | ~5s |
| S3 requests | ~5000 PUTs | ~5000 HEADs + ~50 PUTs |

## Test plan (24 tests)

### Chunker validation (7)

| Test | What it validates |
|------|-------------------|
| Deterministic output | Same input always produces same chunks |
| Boundary stability | Insert at offset 0 only changes first chunk(s) |
| Min/max size bounds | No chunk < 512KB, none > 8MB |
| Empty input | No chunks produced |
| Small input | Input < min size → one chunk |
| Large input | 100MB random data, reassemble and byte-compare |
| All-zeros | Pathological input doesn't degenerate |

### Backup/restore (9)

| Test | What it validates |
|------|-------------------|
| Round-trip | backup → restore → byte-compare identical |
| Dedup | Second backup of same file → zero new chunks |
| Partial change | 1MB change in 50MB file → 1-2 new chunks |
| Append | Data added at end → only new tail chunks |
| Corrupt chunk | SHA256 mismatch on restore → error |
| Missing chunk | Deleted chunk → clear error on restore |
| Empty file | Round-trips correctly |
| Large file | 500MB, chunk count ~500 (reasonable for 1MB avg) |
| Manifest correctness | Size and chunk count match |

### Prune (4)

| Test | What it validates |
|------|-------------------|
| Keep N | Correct manifests survive |
| Delete unreferenced | Old chunks cleaned up |
| Shared chunks | Cross-volume chunks survive prune |
| Zero backups | No-op, no errors |

### Concurrency (2)

| Test | What it validates |
|------|-------------------|
| Two volumes concurrent | No races, chunks shared |
| Backup + prune same volume | Mutex prevents overlap |

### API integration (2)

| Test | What it validates |
|------|-------------------|
| POST /volumes/{name}/backups | Triggers chunked backup |
| POST /volumes/{name}/backups/restore | Reassembles correctly |

All tests use the in-memory backend — no S3 needed, runs in CI.

## What changes from current implementation

- `performVolumeBackup` calls `dedup.Backup()` instead of full-file
  copy + compress + upload
- `performVolumeRestore` calls `dedup.Restore()` instead of full-file
  download + decompress
- `volume_backups` table stores `ManifestKey` (the manifest S3 path)
  instead of a single object key
- Per-volume `sync.Mutex` added to server for backup serialization

The API surface (CLI commands, HTTP endpoints) stays the same.

## New dependency

`github.com/restic/chunker` — BSD license, pure Go, zero transitive
deps. ~100KB binary impact on a 20MB binary.

## What we're NOT building

- **Encryption** — S3 server-side encryption handles this
- **Pack files** — restic packs small chunks into single objects to
  reduce PUTs. We skip this; Hetzner doesn't charge per-request
- **Index files** — restic maintains indexes for fast chunk lookups.
  We use HEAD requests. Fine at our scale (thousands of chunks)
- **Multi-node locking** — in-process mutex is sufficient for single
  daemon. S3-based locking deferred to multi-node

## Cost at Hetzner

With hourly backups of a 5GB volume (50MB changes/hour):
- Storage: ~2GB compressed (shared chunks) = well under 1TB included
- Traffic: ~20MB/backup × 24/day × 30 = ~14.4GB/month = negligible
- **Total: $7.99/month base price, no overage**
