# Prepared project image lifecycle

`project-images-v1` adds an explicit clean Ubuntu builder and a private immutable
project image. It does not export an existing user VM. New chats use a fresh
private writable volume and guest. The build and fresh image checkout both use
`/workspace/project`, preserving absolute virtual-environment and dependency
paths. A quota-reused VM uses a separate checkout under `/workspace/chats/...`
and runs setup for that path; dependencies are not copied between paths.

All routes below require the private node administrator. Their provenance,
revocation attestation, image selection and retention references come from the
Cloud control plane. Guest projections cannot select a source VM or publish an
image. The complete shared HTTP example is
`docs/fixtures/project-images-v1.json`.

## Admission and capture

`GET /v1/admin/project-image-profile` reports backing storage capability, the
original Ubuntu base digest, estimated base/candidate geometry, aggregate logical
quota/usage/remaining and physical staging headroom. Capability is unavailable
unless the root volume service validates its private image configuration and
trusted daemon UID outside the worker allocation range. Public health advertises
`project-images-v1` only after the backing service confirms this capability.

`POST /v1/admin/project-builders/{build_uuid}` takes `metadata`, `cpus` and
`memory_mb`. Frozen metadata binds schema 1, tenant/project/build UUIDs, original
Ubuntu base SHA, source commit, installation revision and both optional hook
SHAs. `canonical_workdir` must be `/workspace/project`. The node creates only a
fresh pinned Ubuntu VM and journals its purpose before lifecycle admission.
`GET` of the same route returns its lifecycle receipt and geometry. Replaying an
identical request fills a missing lifecycle-admission receipt after a crash;
changed requests conflict. Capture geometry comes from the actual builder disk,
not a guest-supplied size or the initial profile estimate.

Cloud resolves the exact source and bounded hook contents before creation,
revokes its metadata-read token, clones the frozen commit through a separate
transient lease, and confirms revocation/removal before running any hook.
The managed environment controller uses `preparationMode: "install_only"` and a
fresh child run UUID. It never starts Pi, OpenCode, a provider, or `start.sh`.

`POST /v1/admin/project-images/{build_uuid}/capture` takes `builder_id`,
`install_run_id` and positive `clone_credentials_revoked`. It returns a durable
202 receipt with phase `preparing` or `capturing`; `GET` observes without waking
or modifying the source VM. The node independently checks the same fresh
builder's terminal managed installation, protected root install receipt,
canonical checkout/revision/hook/boot bindings and absence of startup claims.
A trusted guest action removes native run/provider/clone credential state,
strips Git credential configuration, and preserves only the validated canonical
installation cache. No previous guest identity or native chat/session state is
retained. It clears machine/SSH identity and installs a small trusted boot action
that generates fresh identity in each new writable guest.

After journaled sealing, the source rejects new guest work, managed runs and
start/thaw. Status and explicit deletion remain available. The node confirms a
cold stop and fully synced disk before the root volume service exports it. No
untrusted guest filesystem is mounted on the host. The service verifies the
export's reported SHA/generation, ext4 declared extent, size and alignment, then
appends one 64-KiB block containing bounded canonical frozen provenance outside
that ext4 extent. A final full-image SHA owns the unchanged content-addressed
base format. The unique build ID in the trusted trailer gives each generation
its own registered private catalog. A guest filesystem resize can consume that
extra space only in its own writable copy.

## Use, retention and cleanup

A lifecycle `Create` uses a `project_image` selection with `image_id`, exact
`digest`, `tenant_id` and `project_id`. It is mutually exclusive with named image
selection, requires node admin, and uses replicated storage. One transaction
validates owner, tenant, project and generation and saves the VM pin before any
import. The requesting worker receives only its exact readonly image leaf in its
broker jail. The host image parent is root-owned mode 0750 with the trusted
daemon group; final leaves are 0444. Other worker identities cannot traverse the
host directory, and existing broker/Landlock policy grants no image-root-wide
access.

Delete the completed builder before allocating its fresh image validation VM.
Validation uses a separate child environment controller with
`preparationMode: "start_only"`; it skips installation and executes start in the
new guest. Successful preparation proves finite script completion, not future
service health. Only after validation does Cloud atomically set its current and
previous image pointers and mirror node retention. A failed rebuild preserves
the previous successful image.

`PUT /v1/admin/project-images/retention/{tenant}/{project}` takes a monotonic
`generation`, current/previous IDs, `active_build_ids` and explicit
`release_build_ids`. Older generations and changed same-generation retries
conflict. Current/previous references change atomically; omission never releases
a newly admitted candidate pin. Candidate pins remain through ambiguous pointer
activation or mirror results. Explicit releases name immutable terminal build
IDs. VM pins also survive pending creation, daemon restart and unconfirmed disk
retirement; missing sandbox rows alone cannot release them.

Each candidate or retained private image is charged its full logical bytes,
including the trailer, against aggregate node disk quota alongside ordinary
replicated and legacy volumes. Failed and deleting images remain charged until
verified retirement. Export admission reserves worst-case logical staging bytes
plus 512 MiB of filesystem headroom, including pending reservations. Total image
size must fit the existing 64-GiB storage limit. The 512 MiB is temporary physical
headroom, not an additional persistent logical charge.

`DELETE /v1/admin/project-images/{image_id}` refuses current, previous, active
build, VM/create or capture references. A pending deletion returns 202; complete
retirement returns JSON with both `logical_released` and
`reclamation_complete` true. Root storage cleanup only operates on registered
private image SHAs and waits for dependent volume records to finish reclamation.
Operator images and arbitrary object prefixes cannot be selected. Even a failed
preflight with no storage candidate receives a durable retired intent, fencing
late captures before its node reservation is released.

## Recovery limits and cost

Lost capture replies and node restarts reconcile the same immutable intent.
A proven sealed source can finish stop/sync without re-executing guest setup.
A crash before the node records successful sealing fails closed and requires a
new explicit build. Transport ambiguity retains source/build pins and charge;
bounded observation retries cannot turn uncertainty into success or cleanup.
A definitive quiescent failure is durably sealed, preventing delayed duplicate
capture requests from starting later. A volume-supervisor restart may seal an
incomplete export/import as failed; explicit retry uses a new build UUID. A
previous active image remains available.

The first implementation materializes a bounded sparse disk and scans/hashes the
entire logical image. Existing base namespaces are per content SHA, so each new
generation imports its full nonzero disk data into a separate private catalog.
Rebuilds are not cheap package-only deltas. There are no recursive parent chains,
periodic snapshots, personal VM exports or mutable cross-tenant disk references.
