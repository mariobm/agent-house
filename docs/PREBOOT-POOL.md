# Prebooted Ubuntu pool: qualification and integration decision

Status: **experiment passed; production Cloud integration is not enabled**.

A prepared VM can be assigned without moving its disk, changing its internal VM
ID or booting Linux again. The worthwhile design is one pristine, independently
booted Ubuntu VM, usually paused while waiting, then permanently assigned to one
workspace. Used VMs are destroyed, never recycled into the pool.

## Measurement

Measured on `agent_house`, with an isolated daemon built from the resident-pause
implementation and the released v0.3.3 CLI running on the same host. Each VM used
**1 vCPU, 2 GiB RAM, replicated storage**, and the installed Ubuntu development
image. At most one VM existed at a time. The host load was approximately 0.1.
Image SHA-256:
`7685ce65ef5452f13e86f414a55a2f999c0f5beb3a0d187882e3a5019fdff68c`.

The pool-hit clock starts before a durable SQLite claim and ends when a real CLI
shell executes its readiness marker. Ordinary create includes daemon create and
the same shell attachment. Preparation is excluded only from pool-hit latency
and reported separately. This is a placement model, **not an end-to-end Cloud
create measurement**: Cloud authentication, D1, routing and client RTT remain to
be measured after integration.

| Path | Samples | Usable shell |
| --- | --- | --- |
| First ordinary create after isolated supervisor startup | 1 | 14.530 s |
| Subsequent ordinary creates | 3 | 2.033 / 2.355 / 2.359 s |
| Prepared running VM | 3 | 0.127 / 0.127 / 0.130 s |
| Prepared paused VM | 3 | 0.146 / 0.143 / 0.142 s |

Warmed ordinary create median: **2.355 s**. Prepared running median: **127 ms**.
Prepared paused median: **143 ms**. One additional ordinary sample was collected
after the main nine-VM run to give three warmed ordinary samples.
Pool preparation itself took **1.956–2.325 seconds**, and the local durable claim
cost about **12–18 ms**. These small samples are directional, not production
percentile or tail-latency guarantees. First-use preparation can still be slow;
a pool moves that work ahead of demand rather than eliminating it.

## What passed

- All nine VMs had different volume IDs, boot IDs and guest entropy samples.
  Claiming preserved the existing worker PID and boot ID.
- A user marker written to each used VM was absent from every subsequent fresh
  VM. No used VM was returned to the ready slot.
- The base image contains neither a populated machine ID nor SSH host keys;
  there were no inherited machine/SSH identities to duplicate. Image preparation
  must continue stripping these, or generate unique identities before readiness.
- Eight competing claimers yielded exactly one owner. Retrying a committed claim
  after reopening the database returned the same VM; another owner could not
  claim it. A process exit before commit rolled the partial claim back.
- Competing refills could reserve only one spare. CPU/RAM stayed charged through
  preparation, claim and deletion, until reclamation was confirmed.
- Image, CPU, RAM, network-policy and storage-profile mismatches were pool misses.
- Eight placement-model tests passed on macOS and Linux. The live test used real
  KVM, replicated disks, the daemon and CLI; it did not impersonate Cloud tenants.

The executable tool is in [experiments/preboot-pool](../experiments/preboot-pool/README.md).
It changes only a dedicated isolated daemon, restores its idle policy, deletes
its test VMs and waits for storage reclamation. No raw result files are committed.

## Production implementation next

Implement this in Cloud placement, not as a public rename operation:

1. **One private spare, disabled by default.** Reserve a normal internal VM ID
   under a service-only pool owner before dispatching preparation. Its full guest
   resources and storage must count against existing host capacity. Foreground
   requests take priority over replenishment. Start with Ubuntu, 1 vCPU, 2 GiB,
   replicated disk and one exact network/image profile.
2. **Prepare through the operation journal.** Boot independently from the shared
   immutable image with fresh private writable storage. Check readiness, current
   image generation, worker and network health before publishing the slot.
   Let resident pause retain it cheaply. Missing, stopped, failed, expired or
   mismatched spares must not be presented as ready pool hits.
3. **Claim in the existing Cloud transaction.** Atomically check tenant quota,
   name availability, host policy and request idempotency; transfer the internal
   VM's ownership and bind the create receipt to its stable ID. Move the existing
   host reservation rather than adding a second one. The host's Cloud service
   identity and volume writer do not change; public names are routing aliases.
   No tenant-facing access is granted before that transaction commits.
4. **Fail safely and replenish.** A lost response resolves from the same durable
   claim receipt. An uncommitted claim remains unassigned. Never recycle a
   claimed VM. Invalidate spares when the default image/network profile changes.
   Retain charges for failed/uncertain preparation or deletion until recovery
   proves resource release. Use bounded backoff; a pool miss follows normal
   create and does not wait for replenishment.
5. **Qualify the actual Cloud boundary.** Race different tenant create requests,
   replay/interrupt claims, restart the pool controller and host, exercise quota
   limits, verify old-owner denial and image changes, and measure CLI-to-prompt
   through Cloud. Expose ready/preparing/retiring counts to the administrator.
   Only then enable one spare on the pilot host.

The experimental SQLite ledger is not an authorization service and should not
be bolted beside production ownership. Its state transitions must be integrated
with the existing Cloud transaction and recovery paths. Desktop/custom profiles
remain excluded until their separate qualification.
