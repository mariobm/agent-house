# Linux VMM worker isolation

The daemon writes a filesystem allowlist into each worker spec. On Linux,
`ahvm-vmm` closes inherited descriptors above stderr after exec, then applies
Landlock before creating any libkrun context or thread. Failure stops the
worker. The daemon checks ABI support without restricting itself, so an
unsupported host cannot report a healthy upgraded runtime.

Landlock ABI 6 (Linux 6.12+ with Landlock enabled) is required. The policy allows
the exact disk/device, socket, temporary and runtime directories for writes, selected base images,
libraries and a small set of system files for reads, and the KVM device. GPU
workers additionally receive the trusted GPU library tree and render nodes.
The VM directory and its supervisor metadata remain read-only to the worker.
Snapshots are staged under its runtime directory and moved out of the writable
tree before the daemon adds trusted backing-image metadata. A restore receives read access
to only the selected bundle. The daemon publishes named snapshots itself.
Guest-mounted host paths require explicit host-authored policy rules.

Workers cannot read or write sibling VM file contents or daemon state, follow a
symlink to a forbidden file, trace the daemon, send signals outside their
Landlock domain, or connect to an abstract Unix socket outside it. The worker
starts with an empty inherited environment and explicit runtime variables;
its temporary files and home are within its own directory. Stdio remains the
configured console/log descriptors. This is additional protection after a
VMM defect, not proof that guest-controlled code cannot escape the VM.
Landlock constrains new opens; descriptors legitimately opened after startup
remain usable if the VMM is later compromised. Native snapshot artifacts are
still untrusted VMM output. Trusted backing-image metadata is created only
after moving the staging directory out of the worker's writable tree.

Workers still share the service's Unix identity. On Landlock ABI 6–8, connecting
to a pathname Unix socket is not restricted: a compromised worker could reach
a sibling control socket despite the file allowlist. ABI 9 adds a second
policy layer for pathname sockets within the worker's writable directories.
Separate per-VM identities or namespaces, restrictions on other syscalls and
isolation of the network gateway remain follow-up work. Ordinary VMM workers
also deny host TCP bind/connect through Landlock; managed networking runs in
netd. Host UDP sockets are still available after a VMM compromise on ABI 6–9.
KVM and trusted GPU device ioctls remain part of the host attack surface.
Landlock also does not mediate chmod/chown, timestamps and extended attributes:
a compromised same-UID worker can still alter sibling file permissions/metadata.
macOS development VMMs do not get this Linux policy. Do not treat this change
as complete multi-tenant host-process containment.

Missing `net_uds` now leaves guest networking offline. The daemon always sets
`trusted_host_socket_access` to false. A trusted standalone spec can explicitly
set it to true to enable host INET sockets through TSI; this cannot be combined
with managed networking or GPU mode. There is no guest/API opt-in. The libkrun
vsock muxer checks the TSI feature flags before processing guest TSI requests,
so API authorization is not the boundary for those packets.

## Fresh installs and migration

Both manual bundles and `ahvm host add --install` use the same installer. It
checks the Landlock/cgroup/systemd requirements and configures
`AHVM_CGROUP_ROOT` under a private `ahvm_<unit>.slice`. The daemon is a sibling
of the delegated `<unit>-workers.service`, whose keeper process lives in a
leaf. Only that private common slice's `cgroup.procs` is writable by the service
account; the global system slice remains root-owned. Per-VM limits cover VMM
and gateway CPU, memory/page cache, swap and task count. They do not provide
filesystem isolation or separate Unix identities.

Existing upgrades preserve units and resource policy. Stop sandboxes explicitly
before changing cgroup settings: adoption refuses workers outside the required
group and never moves existing VMs. Install/render the new private slice and
worker/daemon unit templates with your existing unit, user and paths, set
`AHVM_CGROUP_ROOT` to that worker unit's cgroup, reload systemd, then start the
daemon and the stopped sandboxes. Keep custom Cloud units and their existing
delegation configuration. Restarting the daemon alone leaves live workers
running with their original policy.

New named snapshots retain the exact original backing image. Legacy named
snapshots without `backing-image.json` use the configured default for their
allowlist; a custom image or changed default may therefore fail restore.
Restore with the original image configured or recreate the checkpoint before
switching images. Existing VM stop/start continues using its saved backing.

## Packet bounds and qualification

Current main and the released submodule pin `79cdb626` already validate socket
address lengths before the unsafe host address conversion and bound copied
guest TX data to the declared payload, at most 64 KiB. The older
`4f247dda` GPU branch predates those fixes. Bundle creation from Git now rejects
a fork checkout that differs from the parent's pin or has tracked changes;
experimental direct builds remain available. No host escape was demonstrated
by the reported malformed-packet findings.

`AHVM_WORKER_SANDBOX_TEST=1 cargo test --manifest-path rust/Cargo.toml --locked
-p ahvm-engine worker_sandbox::tests::kernel_isolation -- --nocapture` exercises
real Linux allow/deny behavior in a disposable child. It includes parent proc,
process-memory, signal, abstract-socket, symlink/hardlink and file checks and
prints the pathname-socket gap on older ABIs. VM, snapshot, networking and
installation qualification must be run separately on Linux/KVM.

The opt-in native NBD/R2 gate is
`experiments/durable-storage/qualify-worker-isolation.py`. It requires a
dedicated private `ahvm-volume-qualification` bucket credential, an existing
nonroot test identity, Linux/KVM, systemd, a free NBD device outside the
production pool, static BusyBox, e2fsprogs, nbd-client and boto3. Build the
current daemon, volume service, forge, release VMM and exact-policy probe first:

```bash
cargo build --manifest-path rust/Cargo.toml --locked \
  -p ahvm-daemon -p ahvm-volume -p ahvm-forge
cargo build --manifest-path rust/Cargo.toml --locked --release -p ahvm-vmm
cargo build --manifest-path rust/Cargo.toml --locked \
  -p ahvm-engine --example worker_policy_probe
sudo env AHVM_WORKER_REPLICATED_TEST=1 python3 \
  experiments/durable-storage/qualify-worker-isolation.py --execute \
  --root /var/tmp/ahvm-nbd-r2-FRESH_ID --credentials /private/r2.json \
  --daemon "$PWD/rust/target/debug/ahvm-daemon" \
  --volumed "$PWD/rust/target/debug/ahvm-volumed" \
  --forge "$PWD/rust/target/debug/ahvm-forge" \
  --vmm "$PWD/rust/target/release/ahvm-vmm" \
  --policy-probe "$PWD/rust/target/debug/examples/worker_policy_probe" \
  --lib /usr/local/lib64 --device /dev/nbdUNUSED --user TEST_USER \
  --source-head "$(git rev-parse HEAD)"
```

The gate uses native volume eviction and recovery in bounded, disposable
systemd units and a separate 2-GiB filesystem. It writes and hashes guest data,
syncs and stops, requires the local journal to be gone, removes the copied
pinned base, then restarts both supervisors and recovers through a fresh NBD
worker. `local_base_reads` remains enabled. A separate zero-filled default
image satisfies the daemon's general default-image preflight; it cannot
supply the pinned VM's filesystem. The exact emitted worker policy is probed
as the nonroot identity for own-device/runtime access and peer/TCP denial.

The negative phase backs up and deletes one referenced private object
containing a known marker block. It requires an explicit direct NBD `EIO` at
that block, restores and verifies the exact bytes, then checks successful guest
recovery again. Logs distinguish each phase. Cleanup removes only the owned
VM, units/cgroups, NBD attachment, loop mount, copied credentials and fresh R2
prefix. Sanitized evidence stays under the fresh root's `evidence/` directory.
This gate checks explicitly synchronized disk recovery. Guest fsync retains its
local-journal contract; host-disk loss can still lose writes pending replication.

[Recorded Linux qualification](results/worker-isolation-linux.json) covers a
fresh nonroot install, effective cgroups, daemon adoption, managed DNS, custom
backing-image snapshots, raw disk writes/sync and inherited descriptor closure.
[Native NBD/R2 qualification](results/worker-replicated-isolation-linux.json)
also passed synchronized writes, native journal eviction, both supervisor
restarts, absent pinned local base, fresh NBD recovery, exact-policy TCP denial,
missing-object `EIO`, verified restoration and cleanup. It used a minimal
128-MiB image with a 1-vCPU/256-MiB guest and a 1-MiB marker. Its 7.009-second
remote recovery is an observation for that fixture, not an Ubuntu boot target.
Release worker policy setup measured 61–83 microseconds in these runs. This is
startup-only evidence, not a throughput benchmark or a comparison of complete
cold-boot latency. The NBD/R2 workers measured 51–52 microseconds.
GPU and native macOS isolation were not qualified in this change.
