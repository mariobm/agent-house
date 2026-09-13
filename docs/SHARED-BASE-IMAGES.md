# Shared base images for replicated VMs

A new replicated VM pins an immutable image catalog and initially uploads only
its private disk map. Ubuntu's files are not uploaded again for each VM. Writes
and explicit zeroes affect only that VM. Local storage keeps its existing path.

The base is published once to the private object store. If the original image is
present on the host, the worker reads base data directly from it, verifying every
64-KiB block against the pinned catalog. The host kernel shares cached file data
across VMs. A missing, changed or truncated local image falls back to the verified
remote copy. Guests never receive object-store credentials or host file paths.

This is shared disk data, not a pool of running VMs. New VMs still boot their own
kernel and services. A bounded pool of clean prebooted VMs is a later optimization.

## Operator prewarming

After installing a trusted raw image under the service's configured image roots,
prepare it before admitting user creates:

```bash
sudo /opt/ahvm-cloud/bin/ahvm-volumed warm \
  /etc/ahvm-cloud/volume-service.json \
  /opt/ahvm-rust/share/base.ext4
```

For self-hosted replicated deployments, substitute the installed binary, service
configuration and image paths. This command requires a running volume service.
The request is root-only and performs hashing/upload inside its existing CPU,
RAM and task limits. It creates no VM or NBD attachment. Preparation permits a
one-hour client wait; a disconnected client does not cancel the server's import.
Retrying uses the same content-addressed base. Only one image import runs per
supervisor. Ordinary disk I/O remains in independent worker processes.

The first preparation scans and uploads the image. Subsequent preparation reuses
the completed base; a bounded metadata-identity cache avoids rescanning unchanged
trusted files in the same supervisor process. A supervisor restart clears this
hash cache, so the first prepare/create afterwards scans the local image once.
New replicated creates also prepare missing bases, but operators should prewarm
installed images rather than make the first user wait for that upload.

`local_base_reads` defaults to `true` in the volume-service configuration. Setting
it to `false` disables local image reads for newly launched workers, allowing
remote-only recovery qualification without deleting or renaming installed images.
Existing workers keep their launch configuration. Restore it to `true` for normal
operation. VM disk contents and durability semantics do not depend on this flag.

## Format and deletion

Indexed formats 6 and 7 add an image SHA-256 and immutable catalog hash to the
private disk map. Format 7 also carries the existing replication watermark. The
ownership envelope remains format 4. Formats 2/3 remain readable, and old unfinished
imports retain their original retry path; existing disks are not converted.
Older volume binaries reject the new formats. Do not downgrade with shared-base
VMs present. CLI/daemon clients still use the existing replicated-storage API.

The catalog is a fixed-size, hashed object containing the disk size and immutable
page hashes. Data/page reads validate length, hash and page bounds. Neither VM
creation nor subsequent reads follow the mutable image-import head after pinning
the catalog. There are no chains of VM parents and no tenant-to-tenant sharing.

Shared objects live under `<prefix>/bases/<image-sha>/`, outside every VM's chunk
namespace. Per-VM collectors list and delete only private objects;
they cannot remove a shared image. A zeroed block/page must remain zero even if
the base originally contained data. Missing private data is an error, never a
fallback to the original image. Reverting content to its base value uploads no
duplicate base objects.

Bases are operator-managed and retained, including reusable partial imports.
Automatic unused-base deletion is deliberately not implemented: it needs a
reference-aware catalog spanning every dependent VM. Do not manually remove a
base referenced by any disk. Installing many image versions adds shared storage,
but creating more VMs from one version does not duplicate that image. Volume
logical quotas, journal/cache budgets and process ceilings remain unchanged.

## Qualification

Tests cover clone isolation, partial writes, explicit zeroes, full-page zeroing,
reverts, catalog pinning, corrupt/missing objects, local-cache validation and
fallback, owned-disk replication/eviction/reopen, retirement before enrollment,
and deletion leaving a peer/base intact. The S3 adapter test verifies namespace
separation and a common seven-request limit for ordinary/base read-ahead.
`experiments/durable-storage/qualify-shared-base.py` runs one disposable VM at a
time after prewarming, checking fresh-image isolation, writes, stop/sync, local
eviction, cold start and native remote reclamation. It preserves failed state for
diagnosis. Use a private token file and the node's actual volume-state directory.

On agent_house (Ubuntu developer image, 16-GiB logical, one vCPU / one GiB):

| Operation | Measured time |
| --- | ---: |
| First base publication plus boot | 423.91 s, once per image |
| Prewarm an existing base after supervisor restart | 9.68 s, including local hash scan |
| Create after prewarm with local image reads, final implementation | 8.14 s |
| Cold start after eviction, local image available, final implementation | 6.77 s |
| Eviction wait after explicit stopped sync, final local run | 5.28 s |
| Native reclamation after delete acknowledgement, final local run | 9.55 s |
| Remote-only create, final read-ahead implementation | 14.95 s |
| Remote-only cold start, final read-ahead implementation | 19.01 s |

The remote-only gate disables local image reads on freshly launched workers, so
it proves recovery from shared R2 objects rather than the host file. An early
implementation lacked base/mixed-page read-ahead; qualification exposed that
slowdown and the final implementation restores bounded speculation. Timings are
observations, not latency guarantees. Two earlier local-image cycles also passed
(8.76–9.18 s create, before the mixed-page read-ahead improvement). The marker workload is small; larger private working sets increase collection
work. No prebooted pool is included.

The first guest also passed volume-supervisor restart and daemon adoption. All
subsequent gates used native per-VM cleanup; shared base objects remain for reuse.
No bulk operator deletion is needed for these private overlays.

Cloud's storage policy remains local during qualification. Automatic Cloud wake,
user/admin storage dashboards and a prebooted VM pool are separate follow-ups.
