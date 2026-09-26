# Resident idle pause

Ordinary (non-desktop) VMs pause after **30 seconds** without guest activity.
Pause suspends vCPUs and retains the worker, RAM and attached disk. It does not
snapshot, export a backup, detach storage or free memory. Background disk
replication continues independently. CPU/RAM reservations remain charged.

Exec, files, opening/attaching a shell, and preview requests resume a paused VM
before accessing the guest. This works for both local and replicated storage.
Connected shells and previews, even quiet ones, and in-flight guest operations
prevent automatic pause and idle stop. After the last operation/connection ends,
the idle timer starts again. Ordinary detached background sessions alone do not
keep the VM awake.
[Managed runs](MANAGED-RUNS.md) hold server-owned activity protection through
completion, then use a five-minute idle stop for their VM. Operators running
unmanaged detached work can disable pause.

Status/list/storage polling does not resume a VM. Passive session inspection and
cleanup do not wake it either; these guest RPCs can refuse while it is paused.
Explicit `ahvm start NAME` also resumes a resident paused VM. A dead worker still
requires the existing recovery/start path, not resident resume.

## Configuration

- `AHVM_PAUSE_SECS`: initial default **30**; **0** disables automatic pause;
  otherwise **5–86400** seconds.
- Authenticated host administrator: `GET`/`PUT /v1/admin/idle-policy` with
  `{"pause_after_secs":30}`. Changes persist in the daemon database, override the
  environment default after restart, and affect subsequent pause decisions.
- AHVM Cloud: the admin dashboard's **Idle pause** form updates this same host
  policy. Ordinary users cannot change it. An older/unreachable host disables
  the form rather than claiming a setting was saved.
- Sweep interval is `AHVM_SWEEP_SECS`, capped at five seconds (minimum one).
  An idle VM pauses on the next available sweep after crossing the threshold.
- `AHVM_IDLE_SECS` remains **3600** by default. This later stop terminates the
  worker and releases RAM. Replicated storage can then evict synced local data
  according to its separate eviction policy. Local stop retains its checkpoint
  behavior. Changing the pause timeout does not change either policy.

Setting pause to zero prevents future pauses; already-paused VMs resume on guest
work or explicit start. An already-committed transition finishes before new work
is admitted. Admin settings are not read from a remote database on guest calls.

Desktop idle pause is not enabled until separately qualified. Desktop stop/start
and the existing one-hour idle-stop policy remain unchanged.

Before downgrading to a daemon without pause support, resume or stop paused VMs:
older daemons cannot read the new persisted `paused` state.
