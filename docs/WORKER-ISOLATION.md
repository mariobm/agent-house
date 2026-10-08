# Linux worker identities and filesystem isolation

The Linux daemon requires `ahvm-worker-broker`. Each VMM and network gateway
runs with a distinct host UID/GID, an empty supplementary-group list, no
capabilities, `no_new_privs`, and a private filesystem view. These controls
address the shared-identity metadata and pathname-socket gaps in the previous
Landlock-only deployment. They are additional containment, not proof of a
complete hostile-tenant security boundary.

## Launch and supervision

The unprivileged daemon authenticates to a root-owned broker over a Unix
socket using `SO_PEERCRED`. Requests select a VM and fixed role, not arbitrary
commands, executable paths, identities or environment variables. The root-only
configuration selects binaries, libraries, image roots, devices and the
existing delegated cgroup subtree. Linux daemon startup fails if the broker or
its configuration does not match. Direct library/standalone development callers
can still explicitly use the older Landlock-only path; that is not the Linux
daemon's production launch path.

A fresh single-threaded launcher creates a private mount namespace and an
empty tmpfs chroot. It copies the validated spec into that root and mounts only
the role's approved files. Specs and selected base images/libraries are
read-only. Writable disk, temporary, runtime and socket paths use idmapped
mounts: they remain daemon-owned on the host and appear worker-owned inside
the jail. Changing identity does not recursively chown a VM's disk tree.
Device nodes are private inodes referring to only the configured KVM, NBD or
render devices. The host nodes do not become writable to a shared worker group.

The VMM does not see supervisor state or sibling VM paths. Its own gateway's
socket directory is read-only; the gateway has write access to that directory
and cannot access the VMM's disk. Setup descriptors close before worker exec;
only the intended stdio/log descriptors remain. All real, effective, saved and
filesystem IDs are checked after dropping credentials. The trusted broker unit
retains the capabilities needed for setup across its own execs; the launcher
clears ambient, bounding, permitted, effective and inheritable sets before
executing worker code.

Both worker roles then apply Landlock. The ordinary VMM denies host TCP
bind/connect; managed networking belongs to the gateway. Missing managed
networking leaves the VM offline. Broker specs cannot enable transparent host
socket proxying or arbitrary host filesystem mounts. The standalone trusted
TSI opt-in remains outside this deployment mode.

Root-owned records bind VM, role, PID, process start time, identity generation
and approved root disk. Allocation is persisted before launch, and the launcher
waits for its PID record to become durable before processing the launch plan.
The daemon can adopt only matching broker records. Stop verifies process
identity and drains the exact role cgroup, including descendants, using pidfds.
Unknown identities, unexpected nested role groups and observation errors fail
closed. An empty `cgroup.procs` alone is insufficient: cleanup waits for
`cgroup.events` to report `populated 0`.

Roles live in `vm-<id>/vmm` and `vm-<id>/netd`; aggregate CPU, RAM, swap and
process limits stay on the VM parent. Gateway replacement does not kill the
VMM. Daemon and broker restarts preserve live workers. Stop VMs explicitly
before host maintenance; stopping a supervisor is not a VM drain.

## Snapshots and replicated disks

Snapshot output starts under the VMM's writable runtime directory. The daemon
validates the native artifacts, creates a **fresh directory inode**, moves only
those artifacts into it, and creates trusted backing-image metadata there.
A retained staging-directory descriptor cannot name the new trusted metadata.
Native RAM/checkpoint artifacts remain untrusted VMM output, including when a
worker retains descriptors to them. Named snapshots preserve their original
backing image; old snapshots without that metadata need the original default
image configured when restored.

For replicated disks, add the exact root-owned `worker_broker_socket` to the
volume-service configuration. Keep `client_uid` set to the daemon account.
The volume service authenticates that caller as before, then uses a root-only
broker inspection to bind the VMM's exact PID/start time/role to its approved
NBD device. It does not trust a broad UID range or a daemon-written PID alone.
See [volume service configuration](VOLUME-SERVICE.md).

## Requirements, installation and migration

Linux x86_64/KVM, enabled Landlock ABI 6 (Linux 6.12+), systemd 254+, unified
cgroup v2 with CPU/memory/pids controllers, and idmapped-mount support on the
actual VM-data filesystem are required. ext4 is qualified. The broker probes
user/mount namespaces and the data filesystem before notifying systemd that
it is ready. Unsupported hosts fail rather than weakening isolation.

Fresh installations use the templates and defaults in
[RUST-INSTALL.md](RUST-INSTALL.md). The broker's registry and jail backing
root are separate root-owned mode-0700 directories. Its JSON configuration is
root-owned mode 0600. Executables and library ancestors must be root-owned and
not writable by other users; do not point production configuration at a
user-writable Cargo build tree.

Existing hosts require an explicit migration before deploying this daemon:

1. Preserve the current runtime, configuration and daemon database for rollback.
   Stop every legacy VM through its existing API/CLI and verify the old worker
   cgroups are empty. A supervisor restart alone does not apply isolation.
2. Reserve an unused host UID/GID range, checking users/groups, subordinate ID
   allocations and `/etc/ahvm-worker-ranges/`. Install the new broker binary and
   create its separate private state/jail directories. Preserve the existing
daemon account and disk ownership.
   The sandbox parent may remain root-owned for a storage-quota broker;
   it must not be group/other-writable, and every VM directory and spec must
   still belong to the daemon. Do not chown the quota-controlled parent.
3. Render `packaging/rust/ahvm-worker-broker.service.in` for the actual unit,
   prefix, data and broker-state paths. Retain the existing Cloud resource slice
   and delegated worker service. Configure the broker with their actual paths;
   set `AHVM_WORKER_BROKER_SOCKET` in the daemon environment and add the broker
   dependency to the daemon unit.
4. Set root-owned `worker-broker.json` fields as shown below. Allow only the NBD
   pool allocated to this installation and its required render nodes. For
   replicated storage, set the same broker socket in the volume service.
5. Reload systemd, start the broker and volume service, then the daemon. Start
   a disposable VM and verify identities, file access, networking and recovery
   before starting retained VMs. Existing disks need not be deleted.

For the default self-hosted paths, the configuration shape is:

```json
{
  "socket": "/run/ahvm-rust-worker-broker/worker.sock",
  "state_dir": "/var/lib/ahvm-rust-worker-broker/state",
  "jail_dir": "/var/lib/ahvm-rust-worker-broker/jails",
  "data_dir": "/var/lib/ahvm-rust/sandboxes",
  "cgroup_root": "/sys/fs/cgroup/ahvm_ahvm_rust.slice/ahvm-rust-workers.service",
  "daemon_uid": 999,
  "daemon_gid": 999,
  "uid_base": 1073741824,
  "gid_base": 1073741824,
  "identity_count": 1048576,
  "vmm_bin": "/opt/ahvm-rust/bin/ahvm-vmm",
  "netd_bin": "/opt/ahvm-rust/bin/ahvm-netd",
  "gpu_bin": "/opt/ahvm-rust/bin/ahvm-vmm-gpu",
  "lib_path": "/opt/ahvm-rust/lib",
  "image_roots": ["/opt/ahvm-rust/share", "/var/lib/ahvm-images"],
  "devices": ["/dev/dri/renderD128"]
}
```

Replace the example daemon IDs with the existing account's actual IDs and use
only existing approved paths. `gpu_bin` may be null; `devices` may be empty for
local non-desktop VMs. Replication requires explicit `/dev/nbdN` entries. The
installer fills these values for a fresh local-storage installation.

The updated host-upgrade helper refuses an unmigrated installation before
stopping services. Historical clients embed their older helper; use the new
client or perform this migration manually. A normal upgrade after migration
restarts broker and daemon together and rolls both back if health checks fail.
Changes to broker mount/device/library configuration require all workers stopped.
A live legacy worker cannot be adopted as an isolated worker.

## Identity lifetime and registry maintenance

The default reserved range contains 1,048,576 identities. This is **lifetime
launch capacity**, not a simultaneous-VM limit. Each VMM or gateway launch
consumes a fresh identity, including failed launch intents. Normal managed
networking uses two identities when both roles launch. A VM wake uses fresh
identities while reusing its daemon-owned disk through idmapped mounts.

Never reset the allocation counter or delete the registry to reclaim IDs.
Exhaustion fails closed; extend the reserved range after checking and recording
its additional allocation. The broker permits an increased count, but not a
changed range base or a smaller count. Retain installer reservation records
after uninstall until an administrator has proved no old installation uses them.

The registry retains the latest role record for each VM name and fsyncs the
whole registry during launch. This cost grows with retained records. A limit
of 16,384 role records bounds growth. Offline maintenance can remove records
for deleted VM directories only, after proving their role groups are drained:

```sh
sudo systemctl stop ahvm-rust ahvm-rust-worker-broker
sudo /opt/ahvm-rust/bin/ahvm-worker-broker --compact /etc/ahvm-rust/worker-broker.json
sudo systemctl start ahvm-rust-worker-broker ahvm-rust
```

Compaction takes the exclusive registry lock, retains existing stopped VM
records and all live/ambiguous records, and preserves the allocation counter.
It does not reclaim identities. An interrupted launch with a surviving process
requires administrator inspection and an explicit drain; the broker will not
invent a replacement identity or signal an unrelated process.

## Qualification and performance

[Recorded comparisons and raw results](../experiments/worker-isolation/evidence-20261008/README.md)
cover two baseline and two isolated Ubuntu runs, the installed CLI lifecycle
suite, actual broker-launched adversarial probes, native NBD/R2 recovery and an
Omarchy GPU desktop.
Create gained about 0.1 second and local snapshot resume about 0.1 second in the
small comparison; CPU work was similar. Disk and network measurements showed
small absolute differences. This is not a Cloud cold-wake, concurrency or
large-registry capacity benchmark. No broker call is added to guest disk or
network I/O.

The fixed-launcher adversarial gate is opt-in, root-only and uses a disposable
nonroot test account and fresh test directory:

```sh
sudo env AHVM_WORKER_BROKER_TEST=1 python3 experiments/worker-isolation/qualify-broker.py \
  --root /var/tmp/ahvm-worker-isolation-FRESH_ID \
  --broker /root-owned/bin/ahvm-worker-broker \
  --probe /root-owned/bin/worker_isolation_probe \
  --user TEST_USER --uid-base UNUSED_RESERVED_TEST_BASE
```

Build the probe with `cargo build --manifest-path rust/Cargo.toml --locked
-p ahvm-engine --example worker_isolation_probe`. The gate checks actual role
credentials and capabilities, private files and pathname sockets, metadata
operations, sealed specs, retained snapshot directory descriptors, forged
worker records and descendant cleanup. A separate Landlock-only gate remains
available through `AHVM_WORKER_SANDBOX_TEST=1`; its reported ABI 6–8 pathname
socket gap does not describe the additional private mounts and IDs above.

The native NBD/R2 gate is
`experiments/durable-storage/qualify-worker-isolation.py`. It requires a
credential scoped to the private `ahvm-volume-qualification` bucket, an unused
NBD device outside production, KVM, systemd, static BusyBox, e2fsprogs,
nbd-client and boto3. Supply current daemon, broker, volume-service, forge,
VMM and `worker_policy_probe` binaries at root-owned paths:

```sh
sudo env AHVM_WORKER_REPLICATED_TEST=1 python3 \
  experiments/durable-storage/qualify-worker-isolation.py --execute \
  --root /var/tmp/ahvm-nbd-r2-FRESH_ID --credentials /private/r2.json \
  --daemon /root-owned/bin/ahvm-daemon --volumed /root-owned/bin/ahvm-volumed \
  --worker-broker /root-owned/bin/ahvm-worker-broker \
  --forge /root-owned/bin/ahvm-forge --vmm /root-owned/bin/ahvm-vmm \
  --policy-probe /root-owned/bin/worker_policy_probe \
  --lib /root-owned/lib --device /dev/nbdUNUSED --user TEST_USER \
  --uid-base UNUSED_RESERVED_TEST_BASE --source-head COMMIT_SHA
```

It uses a separate 2-GiB filesystem and a minimal 128-MiB image, hashes guest
writes, syncs and evicts the local journal, removes the copied base image,
restarts supervisors and recovers through a new storage worker. It inspects the
actual jail's private NBD inode and exact root-owned identity/device binding.
It then removes one backed-up remote object containing a known block and
requires direct NBD `EIO`, restores the object and verifies recovery again.
Cleanup is restricted to its own VM, units, device, loop mount, credentials and
fresh remote prefix. This checks explicitly synchronized recovery; pending
replication can still be lost if the host disk is lost.

## Remaining boundaries

The daemon, broker, volume service and kernel remain trusted. The broker has
powerful mount/identity capabilities, so its control API is intentionally
small. KVM and configured GPU ioctls remain host attack surfaces. Host UDP
sockets remain available after a VMM compromise; the gateway needs host
network access by design. This change does not add a complete syscall filter
or private network namespace. Descriptors legitimately opened by a worker
remain usable after its compromise. macOS development VMMs do not receive
this Linux isolation.

The libkrucible pin remains `79cdb626`: bounded guest vsock addresses and a
64-KiB declared TX payload limit are already in that pin. This change does not
modify the fork. Build from the pinned submodule; the older experimental GPU
checkout must not be packaged as a release. No host escape was demonstrated
by the earlier malformed-packet findings.

Historical Landlock-only evidence remains in
[worker-isolation-linux.json](results/worker-isolation-linux.json) and
[worker-replicated-isolation-linux.json](results/worker-replicated-isolation-linux.json).
Those older measurements do not quantify the new broker/mount setup.
