# Managed background runs

A node administrator can submit a finite command that continues independently
of the initiating HTTP request or a connected phone. This is the runtime
foundation for Cloud agents; it does not yet expose chat, OpenCode turns or
mobile APIs. The node admin credential must never be sent to a mobile client.

## Lifetime

An accepted run holds the VM awake through quiet model waits and tool work.
The node records the result before releasing that protection. After completion,
**five minutes without guest activity by default** triggers a full stop, preserving the VM
and replicated disk. An attached CLI shell, preview, or another guarded guest
operation still prevents that stop. New guest activity restarts the idle timer.
The ordinary 30-second resident pause may occur during this idle window;
pause alone does not free RAM.

This shorter stop policy belongs only to VMs used by managed runs and persists
across daemon restarts. Unrelated VMs retain `AHVM_IDLE_SECS` (one hour by
default). Restart gives completed managed VMs a fresh configured idle grace because
foreground activity timestamps are not persisted. Successful stop releases the
worker/RAM; safe local replicated-cache reclamation follows separately and can
be delayed by pending writes or remote storage failure. R2 disk data remains.

Cloud's current admission accounting still reserves CPU/RAM for stopped VM
records. Releasing **Cloud admission reservations** requires separate atomic
wake admission; this node change only releases physical resources and daemon
running-resource quota. Never delete a user's VM to release a reservation.

### Configure agent idle stop

The Cloud admin dashboard has a separate **Agent idle stop** setting. Default:
**300 seconds (5 minutes)**; allowed: **60–86400 seconds (1 minute–24 hours)**.
This is the host-wide policy for VMs used by managed runs, including already
completed runs. Active managed jobs and attached CLI sessions still prevent idle
stop. It is not a maximum job runtime and never deletes the VM or its R2 disk.

Self-hosted nodes can set `AHVM_AGENT_IDLE_STOP_SECS` for the initial default.
Node administrators can read `GET /v1/admin/idle-policy` or update one or both
settings with `PUT /v1/admin/idle-policy`:

```json
{"agent_idle_stop_secs": 600}
```

Omitted fields retain their current values; the response returns both
`pause_after_secs` and `agent_idle_stop_secs`. Updates persist atomically in the
daemon database, override environment defaults after restart, and apply on the
next available sweep. Shortening the timeout can stop a VM that is already idle
long enough. A stop already committed before a change finishes normally. The
runtime uses an in-memory value, with no Cloud/database lookup on guest work.

Pause and stop have independent timers measured from the latest guest activity
or run completion. A stop timeout shorter than the pause timeout goes directly
to stop. Ordinary VMs keep their separate `AHVM_IDLE_SECS` policy.

## Node-only API

The node advertises `managed-runs-v1` and `managed-runs-fenced-v1` in
`/v1/healthz`. All three routes
require the node `admin` identity:

| Request | Meaning |
| --- | --- |
| `POST /v1/admin/runs/{id}` | Admit an immutable finite command |
| `GET /v1/admin/runs/{id}` | Observe the durable receipt without waking/touching the VM |
| `POST /v1/admin/runs/{id}/cancel` | Request cancellation, then wait for a confirmed outcome |

Example admission body:

```json
{
  "sandbox_id": "agent-vm",
  "argv": ["/usr/local/bin/ahvm-dev", "/bin/sh", "-c", "sleep 90; printf done > /workspace/result"],
  "max_runtime_secs": 1800,
  "fence_on_failure": true
}
```

Wake the assigned VM first. It must use replicated storage and be running or
resident-paused. Local RAM-snapshot storage is refused because recovery fencing
must terminate the old execution rather than save it for later resumption.
Commands execute with Forge's normal identity; use `ahvm-dev` for the Ubuntu
image's unprivileged developer environment. Arguments are forwarded literally.
Do not submit a permanent `opencode serve` process as a finite run: the later
adapter must wait for the native **turn** outcome and then terminate its finite
controller. Daemonizing work outside that controller is outside this contract.

Finite agent controllers should opt into `fence_on_failure: true`. A nonzero
controller exit, cancellation, or runtime deadline requires a verified cold stop
before the receipt becomes terminal or its activity hold is released. Even an
observed zero exit after cancellation or deadline is fenced: a dead controller
does not prove its child tools stopped. Stop failures retain the unfinished
receipt and hold for retry, including after daemon restart. This exceptional
stop can disconnect an attached CLI shell; clients should check the
`managed-runs-fenced-v1` capability before relying on this guarantee. A clean
zero exit before the deadline keeps the VM running with the normal cooldown.
The optional flag defaults to false; omitted and explicit false preserve the
legacy canonical receipt. Changing the flag for an existing run ID conflicts.

The caller must explicitly provide a runtime budget of 1–86400 seconds. This is
separate from idle policy: exceeding it requests cancellation even if work is
active. Response fields include `phase`, `epoch`, `session_id`, `boot_id`,
`deadline_at`, `finished_at`, `exit_code`, and recovery `detail`. A terminal
phase is `succeeded`, `failed` or `interrupted`. Cancellation acceptance is not
itself a terminal result. Native stdout remains in Forge's bounded scrollback;
these receipts are **not** a durable transcript or chat-event journal.

A run ID binds the VM, owner and canonical command/budget. Identical retries
return the same receipt; changed payloads conflict. There is one unfinished run
per VM, at most 64 on a node, and at most 4096 retained receipts. Exhaustion
refuses new work rather than evicting idempotency records. Receipts currently
live until VM deletion; production retention/archival belongs with the backend
run journal. Do not reuse run IDs or VM identities across deletion.

## Recovery and cancellation

The journal commits launch intent and the guest boot identity before dispatch.
A stable argv marker identifies a session even if its create response was lost.
Controller restart restores activity protection before the idle sweeper starts,
increments the ownership epoch, and reconciles the exact boot/session/command.
It never resubmits an ambiguous command. This prevents automatic duplicate
launch, not arbitrary exactly-once external tool effects.

A runtime deadline or cancel request kills the known session process group and
waits for observed exit. Uncertain launch/result or unconfirmed cancellation gets
60 seconds of reconciliation before attempting a disk-preserving cold stop.
Existing backend RPC deadlines still bound each blocking call; the 60-second
window is not an end-to-end stop deadline. Stop failure retains protection and
retries; it never records completion while execution may remain alive. This
exceptional recovery can disconnect a CLI shell on the same VM. Ordinary job
completion never overrides an attached shell's activity guard.

Explicit VM start/stop/delete conflicts while a run is unfinished. Cancellation
and recovery are serialized with lifecycle changes. Terminal results persist
before activity protection is released. SQLite/remote-storage failure preserves
state for retry, not deletion. No periodic snapshots or backups are introduced.

## Qualification

`python3 scripts/test-managed-runs.py --restart-command '<isolated daemon restart>'`
uses `AHVM_TEST_API` and `AHVM_TEST_TOKEN_FILE`. Run it against a disposable daemon
whose workers survive its restart. It creates one 1-vCPU/2-GiB replicated VM,
checks detached work beyond 30 seconds, daemon adoption, conflicting retries and
lifecycle operations, large output, the actual five-minute stop, cold-wake disk
persistence, and cancellation. It deletes only its generated VM.

Unit tests additionally cover boot changes, lost launch replies, missing native
sessions, failed fencing, stale epochs, restart recovery, and attached-operation
protection. No provider/model requests are used by this gate.

On 26 September 2026, the isolated KVM/R2 gate passed on `agent_house` with
one 1-vCPU/2-GiB Ubuntu VM. The quiet 85-second job survived daemon restart with
the same session, completed after emitting 512 KiB, and the VM automatically
stopped **302.8 seconds after observed completion**. Cold wake read the saved
marker and confirmed one execution. Cancellation and test-VM deletion passed.
Production daemons and existing VMs were not modified. macOS and Linux daemon/
store suites and clippy also passed.

A subsequent isolated KVM/R2 check exercised `fence_on_failure`: cancelling a
controller with an escaped `setsid sleep` child produced an `interrupted` receipt
only after the VM was stopped. A nonzero controller failure also cold-stopped
the VM; a clean exit left it available for ordinary idle handling. The finite
controller survived a daemon restart in the same guest boot and session. These
checks qualify runtime recovery, not the success of a particular model provider.
