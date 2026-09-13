# Portable replicated disk exports

`ahvm-volumed export-remote` creates a standalone raw disk plus a SHA-256 manifest.
It materializes shared-base and private blocks, so recovery does not depend on
the original VM's R2 objects, owner token or base catalog. The source is never
claimed, modified or deleted.

This is an operator recovery building block, not yet a complete Cloud host backup
or a tenant checkpoint API. The Cloud backup guard remains in place until capture
of daemon/D1 metadata and restoration into a fresh namespace are integrated and
qualified with a real guest.

## Capture

Stop the guest and explicitly sync its storage first. Export reads **only the
last published remote state**; pending writes in a host journal are excluded.
An export without that preparation is not an application-consistent backup.

On the Linux host, create a root-owned private output parent, then run:

```sh
sudo install -d -m 0700 /var/lib/ahvm-exports
sudo ahvm-volumed export-remote /etc/ahvm-cloud/volume-s3.json VOLUME_ID \
  /var/lib/ahvm-exports/my-recovery-point
```

Use the internal volume ID from the operator's volume record, not the VM's display
name. The destination must not exist. Credentials stay in the private input file;
output contains the volume ID, logical size and disk SHA-256 only.

The command reads one immutable disk map, checks every data hash, and refuses
success if the remote head changes during capture. Missing or corrupt objects and
output failures fail the attempt. It does not retry against a newer generation.
It allocates one 64-KiB data buffer, a 16-MiB cache and bounded indexed
metadata/transport buffers. The cache reuses metadata pages and provides bounded
read-ahead, avoiding repeated R2 requests while exporting adjacent blocks.
Zero-filled disk regions are sparse holes, but changed/nonzero data still needs
local space. Use a bounded staging filesystem and budget up to the logical disk
size. No unlimited staging allocation, automatic retention or schedule is implied.

Successful output contains `disk.raw` and `complete.json`. The disk and manifest
are fsynced before success is reported. A failed or killed command may leave
partial files: require a successful exit and verify the complete file before
retaining it as a recovery point. Keep it encrypted in the independent backup
repository; it contains guest data. A hash detects corruption, not malicious
replacement of both the disk and its manifest.

## Verify before recovery

Check manifest format `1`, the raw file's exact `logical_bytes`, and its SHA-256
against `sha256` in `complete.json`. Do not restore only metadata pointing at live
R2 objects. The raw disk can be imported into a **new** volume namespace, with new
ownership; never copy the old owner token or overwrite a live disk. Restoring an
entire host additionally needs matching node/control-plane metadata and fencing.

## Validation

Tests cover unchanged source ownership, source retirement, missing/corrupt data,
a source generation changing during capture and output failure. A shared-base test
exports private changes plus zeroed/base regions, removes all source objects and
heads, imports into an empty independent store and verifies the recovered disk's
SHA-256 and contents. This is storage-level qualification; real-guest recovery and
encrypted host-backup integration remain the next step.
