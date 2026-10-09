# Resident idle pause

Ordinary VMs and new or cold-started Omarchy GPU desktops on the qualified
Linux x86_64 runtime pause after **30 seconds** without guest activity.
Pause suspends vCPUs and retains the worker, RAM and attached disk. It does not
snapshot, export a backup, detach storage or free memory. Background disk
replication continues independently. CPU/RAM reservations remain charged.

Exec, files, opening/attaching a shell or desktop viewer, and preview requests resume a paused VM
before accessing the guest. This works for both local and replicated storage.
Connected shells, desktop viewers and previews, even quiet ones, and in-flight guest operations
prevent automatic pause and idle stop. After the last operation/connection ends,
the idle timer starts again. Ordinary detached background sessions alone do not
keep the VM awake.
[Managed runs](MANAGED-RUNS.md) hold server-owned activity protection through
completion, then use a configurable idle stop (five minutes by default) for their VM. Operators running
unmanaged detached work can disable pause.

Status/list/storage polling does not resume a VM. Passive session inspection and
cleanup do not wake it either; these guest RPCs can refuse while it is paused.
Explicit `ahvm start NAME` also resumes a resident paused VM. A dead worker still
requires the existing recovery/start path, not resident resume.

## Configuration

- `AHVM_PAUSE_SECS`: initial default **30**; **0** disables automatic pause;
  otherwise **5–86400** seconds.
- `AHVM_AGENT_IDLE_STOP_SECS`: initial managed-agent stop default **300**;
  **60–86400** seconds. Applies only to VMs used by managed runs, after work ends.
- `AHVM_IDLE_SECS`: initial ordinary VM stop default **3600** (one hour).
  Administrators can change it with `idle_stop_secs`, **60–86400** seconds.
  Existing environment overrides, including shorter qualification values, remain supported.
- Authenticated host administrator: `GET`/`PUT /v1/admin/idle-policy` with
  `{"pause_after_secs":30,"agent_idle_stop_secs":300,"idle_stop_secs":3600}`. PUT accepts any field
  independently; omitted fields are preserved. Changes persist in the daemon database, override the
  environment default after restart, and affect subsequent pause/stop decisions.
- AHVM Cloud: the admin dashboard's separate **Idle pause**, **Agent idle stop** and **VM idle stop** forms update this same host
  policy. Ordinary users cannot change it. An older/unreachable host disables
  the form rather than claiming a setting was saved.
- Sweep interval is `AHVM_SWEEP_SECS`, capped at five seconds (minimum one).
  An idle VM pauses on the next available sweep after crossing the threshold.
- Ordinary idle stop terminates the
  worker and releases RAM. Replicated storage can then evict synced local data
  according to its separate eviction policy. Local stop retains its checkpoint
  behavior for non-desktop VMs. Desktop stop discards RAM and applications;
  subsequent start boots the retained disk. Pause, ordinary stop and managed-agent
  stop are independent timers measured from the latest guest activity.

Setting pause to zero prevents future pauses; already-paused VMs resume on guest
work or explicit start. An already-committed transition finishes before new work
is admitted. Admin settings are not read from a remote database on guest calls.
Shortening a stop timeout can stop a VM that is already idle long enough. The
runtime rechecks the current policy and active holds when committing a stop;
completed managed VMs retain the agent timeout even if the ordinary timeout is shorter.

Omarchy resident pause preserves the worker, graphical applications and unsaved
RAM state during short idle periods. The later **cold stop (one hour by default)** still loses
that RAM state. A desktop VM used by a managed run receives the managed stop
policy after completion. Qualification covers local and replicated disks on the
Intel UHD 770 host; see [measurements and limits](OMARCHY-PAUSE-QUALIFICATION.md).
Software Ubuntu/XFCE desktop and macOS GPU pause remain disabled.

A desktop worker started by an older runtime has no resident control socket.
Upgrading the daemon preserves that running session and keeps it ineligible for
pause until its next cold start with the updated GPU worker. `start` on an already
running VM does not add this capability. Control-channel failure refuses a pause
before committing it; an unconfirmed resume never triggers a silent replacement.

Before downgrading to a daemon without pause support, resume or stop paused VMs:
older daemons cannot read the new persisted `paused` state.
