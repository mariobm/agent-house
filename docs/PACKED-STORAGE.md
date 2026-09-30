# Packed replicated disk objects

New private replicated disks keep **64-KiB logical blocks**, but a remote object
can contain up to **sixteen changed blocks (1 MiB)**. A sync groups distinct,
nonzero dirty blocks into immutable packs. Updating one block writes that block
and a new index page; it does not rewrite its old pack. Zero blocks need no data
object. Identical changed blocks in one commit share a location.

This reduces object requests when a sync contains several changes. Sixteen
distinct changed blocks within one index page require one data object and one
metadata page, instead of sixteen data objects and one page. A sync with only one
changed block still writes a 64-KiB data object. Packing does not compress data.

## References and publication

Root formats 8/9 describe private disks without/with replication watermarks;
10/11 add a shared base image. A private index page is 128 KiB and describes
1,024 logical blocks. Each entry holds a block SHA-256, an object SHA-256 and an
aligned offset. The reader validates the page, whole object and selected block.
Objects and index pages are uploaded before the compare-and-swap publication of
the new root. Losing publication leaves unreachable objects for later collection,
never a root that points to an incomplete upload.

Shared base catalogs and image imports keep their existing format. New VM roots
pin the verified base and pack only private changes. Private disks initialized through the legacy image-import fallback keep the
legacy format as well. Existing disk roots continue using their existing format; this change does not rewrite existing disks.

## Reads and reclamation

The current reader fetches and verifies the whole pack. A cold isolated read may
therefore transfer up to 1 MiB for a 64-KiB block. The byte-bounded object cache
amortizes that cost for reads of blocks sharing a pack. Private metadata pages are also larger than
the legacy 64-KiB pages. Range reads and compression remain separate improvements.

Offline collection follows object references, including pack locations. If even
one block is still referenced, its complete pack stays live. Exclusive offline
compaction repacks remaining blocks and publishes a new root before marking
references for collection. The supervisor bounds each pass to 4 MiB of source
and rewritten block payload and persists a page/pack cursor between passes.
Collection then removes objects that the current root no longer references.

Foreground wake cancellation reaches immutable uploads, duplicate-object
verification and retry waits through the cache and ownership adapters. The
service finishes any request already in flight (bounded to three seconds), then
yields without starting further uploads. Cancellation observed before the head
CAS leaves the old root unchanged; the adapter checks again immediately before
issuing that CAS. Once the CAS starts, its actual success or uncertain outcome
must be recorded; cancellation cannot undo publication.
Canceled staging is discarded while the old root and journal remain usable.
Running writers and pending replication prevent this maintenance. Historical checkpoint roots are not part
of these formats and cannot be silently ignored by collection.

Deleted disks use signed S3 `DeleteObjects` requests of up to 1,000 keys. Every
per-key error is checked, including errors within HTTP 200 responses. Interrupted
passes relist the shrinking prefix; completion requires an observed empty list.
See [the S3 contract](https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObjects.html).

## Deletion and upgrades

Successful deletion stops the guest, detaches the device, fences local reopening,
discards the old journal and durably releases logical capacity. Remote cleanup
continues under the old immutable volume ID. Recreating a name gets a new ID;
old cleanup and old terminal operation receipts cannot act on the replacement.
Operation status and failure details stay fixed. Later cleanup evidence may
change the resource state to absent only from the original volume ledger.

Upgrade the volume supervisor and worker binaries together, then the daemon and
Cloud API. The retirement reply now requires `logical_released` as well as
`reclamation_complete`; an older supervisor cannot authorize early quota release.
Apply the Cloud operation-receipt migration before starting the new Worker.
Older readers reject packed roots, and older supervisors reject the new registry
fields, so downgrade requires preserving the state and using compatible binaries.
Previously completed receipts without a stored identity snapshot stay unknown;
they must never infer an old operation's result from a replacement VM.

## Qualification

The `packed_probe` example uses a fresh random volume in a dedicated private
qualification prefix. The real R2 gate passed in 25.32 seconds: sixteen blocks
produced one pack and one page; partial-pack collection retained live data;
compaction reclaimed the unused part; reopening preserved every block and the
replication watermark. A 1,000-key S3 delete followed by empty-list confirmation
succeeded in 4.61 seconds. Fixture data objects were removed; one small permanent
retirement marker remains. These are small correctness probes, not full-image
delete or random-read benchmarks. No VM or installed service was changed.
