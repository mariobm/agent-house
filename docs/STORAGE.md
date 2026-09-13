# Storage modes and defaults

These commands require a CLI and daemon built with the replicated-storage-v1 API
feature. They are not in previously published releases. The CLI refuses explicit
storage selection against older servers instead of letting an unknown field be ignored.

## Defaults

Self-hosted "ahvm create dev" uses local storage and the server's selected image,
initially ubuntu-dev. No R2 or S3 account is required. CLI sizing defaults are:

| Image | vCPUs | RAM (MiB) |
| --- | ---: | ---: |
| Ordinary VM | 1 | 512 |
| Ubuntu desktop | 2 | 4096 |
| Omarchy desktop | 4 | 8192 |

Explicit --cpus and --memory override these. Direct daemon API create requests default
to 1 vCPU / 512 MiB regardless of image.

The interactive shell defaults to /bin/bash. Typing "exit" ends that shell session
and returns to your terminal; it does not delete or stop the VM. Ctrl-] detaches
while leaving the session running.

The standard daemon stops idle VMs after 3600 seconds, checked every 60 seconds
(AHVM_IDLE_SECS and AHVM_SWEEP_SECS). Active operations defer stop. An open but
silent terminal does not keep a VM running indefinitely. Wake currently requires
"ahvm start dev".

## Local storage

Local disks remain on the host. For ordinary non-desktop VMs, stop saves a local
memory/disk checkpoint and start resumes it. Desktop VMs use disk-only stop/start.
Checkpoints are not off-host backups; host-disk loss can lose local state.

    ahvm create dev --storage local
    ahvm get dev
    ahvm storage status dev
    ahvm stop dev
    ahvm start dev
    ahvm delete dev

Storage status reports local mode, with null replication state and null
replicated logical capacity. Local disks do not support remote sync.

## Optional replicated storage

This is an experimental, operator-configured Linux capability. The standard
installer does not enable it. Configure the [volume service](VOLUME-SERVICE.md)
and AHVM_VOLUME_SOCKET on a dedicated qualification host. When VM cgroups or the local project-quota broker are configured, the engine
requires explicit confirmation that the volume supervisor and its separate
worker pool have finite CPU, RAM and task limits. Configure those limits rather
than removing resource controls. Cloud deployment and full rollout qualification
remain separate steps.

    ahvm create durable-dev --storage replicated
    ahvm get durable-dev
    ahvm storage status durable-dev
    ahvm stop durable-dev
    ahvm storage sync durable-dev
    ahvm start durable-dev
    ahvm delete durable-dev

The mode cannot change after creation. Tenant logical disk quota is reserved
before importing the image. This is capacity accounting, not a measurement of
R2 bytes or a claim that the entire disk is physically allocated on the host.

Writes and guest fsync commit to a local durable journal. Uploads run in the
background. Permanent host-disk loss can lose writes that have not reached
remote storage. Status reports local_sequence, remote_sequence, pending_bytes,
local_failed and replication_failed. Null replication state means unavailable,
not synchronized. Status requests do not keep an idle VM awake.

Explicit sync currently requires a **stopped replicated VM** and returns success
only after the service confirms no pending writes, equal local/remote sequences
and no reported failure. It fails if remote durability cannot be confirmed.
It does not make a named historical checkpoint or a control-plane backup.

After stop, successful synchronization and reclamation allow local journal/cache
eviction. The remote disk remains. Start cold-boots that disk; RAM, processes and
network connections are not restored. Deleting a VM begins remote cleanup; disk
quota and its sandbox name stay reserved until cleanup is confirmed.

## HTTP API

All routes require the sandbox owner's Bearer token.

- POST /v1/sandboxes: optional storage_mode ("local" or "replicated"); omission
  uses the host default (local for the shipped daemon).
- GET /v1/sandboxes/{id}: includes live storage details. List responses use stored
  metadata only and omit this live field.
- GET /v1/sandboxes/{id}/storage: mode, admitted logical_bytes (or null) and
  replication state (or null).
- POST /v1/sandboxes/{id}/storage/sync: explicit stopped-disk barrier, returning
  the confirmed replication state.

A local/running-disk sync is rejected. Service errors remain errors rather than
successful acknowledgements. The API does not return object-store credentials.

## Cloud rollout

Cloud placement can select replicated storage automatically after the operator
qualifies a host. The choice is saved with each create operation, including when
its HTTP reply is lost. Existing local VMs keep their storage mode; users do not
supply S3 credentials or a Cloud storage flag.

The pilot volume service is installed, but Cloud placement remains local. A real
Ubuntu developer-image create took 594 seconds because the image is imported into
each volume. Reusing a verified base image is a prerequisite for activation.
Automatic wake, Cloud CLI storage commands and dashboard storage visibility remain
follow-ups. Self-hosted defaults are unchanged.
