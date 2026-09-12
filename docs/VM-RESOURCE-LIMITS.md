# Per-VM resource limits

Linux hosts can opt in with `AHVM_CGROUP_ROOT`, pointing at a systemd-delegated
cgroup v2 subtree. Default self-hosted behavior is unchanged. Missing controllers,
unwritable limits or live workers outside the expected group fail closed; stop
existing VMs before enabling this setting for the first time.

The cloud deployment uses two services under one aggregate slice:

* `ahvm-cloud-node.service`: independently restartable daemon.
* `ahvm-cloud-workers.service`: persistent delegated subtree. A small `sleep`
  process in its `keeper` leaf keeps systemd from removing the empty subtree.
* `ahvm-cloud.slice`: aggregate CPU, memory and task ceilings for both services.

The worker service uses `Delegate=cpu memory pids` and
`DelegateSubgroup=keeper` (systemd 254+). Configure `AHVM_CGROUP_ROOT` as its actual
cgroup path, `/sys/fs/cgroup/ahvm.slice/ahvm-cloud.slice/ahvm-cloud-workers.service`.
A privileged service setup step grants the daemon user ownership of the common
slice's `cgroup.procs` file, which cgroup v2 requires for migration between sibling
services. It does not delegate the slice's resource-control files.

Do not put the delegated VM groups inside the daemon service: on the tested
systemd 259, restarting that service with surviving workers fails to spawn the
executor (`EBUSY`). The separate worker service stays running across daemon upgrades.
Stopping it intentionally terminates all cloud workers; drain first.

Each VM and its network gateway share:

| Controller | Limit |
| --- | --- |
| CPU | Allocated vCPU count, enforced over a 100-ms period |
| Host memory | Twice guest RAM plus 256 MiB |
| Swap | Disabled |
| Host tasks | 256 threads/processes combined |
| OOM | Kill the whole VM group, leaving its neighbours and daemon alive |

Snapshot writes charge file cache as well as guest RAM, hence the second RAM copy.
These are host limits; the guest still sees its requested RAM. Capacity planning
must reserve this overhead in the aggregate ceiling, plus daemon headroom. Guest
processes run inside the VM and are bounded by its guest RAM/CPU; `pids.max` bounds
host-side VMM and gateway threads, not the guest's process table.

A constant `/bin/sh` launcher writes its own PID to the group before `exec` of the
worker. Paths and arguments are passed separately, never interpolated as shell
code. Admission failure exits before the worker runs. Only the small trusted
launcher starts in the daemon group. No unsafe Rust or root daemon is required.
This adds one shell launch per worker start, with no work on the exec/file hot path.

Gateway replacement retains the group. Adoption verifies membership without moving
or signalling an unverified PID. Delete removes the empty group; startup reclaims
empty groups left by failed/interrupted creation. No group with live tasks is removed.

An OOM becomes the existing failed-worker state; a subsequent start can recover it
using the normal retained snapshot policy. Resource limits do not add a newer
recovery point or guarantee preservation of unsnapshotted RAM.

Disk accounting, I/O, bandwidth, request/stream limits and host-loss backups remain
separate requirements before opening the shared-host pilot to untrusted workloads.
