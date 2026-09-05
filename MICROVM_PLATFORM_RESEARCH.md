# MicroVM Sandbox Platforms: Research, Performance, and Open-Source Orchestration Plan

**Status:** Review draft

**Research date:** 2026-09-03

**Audience:** Platform engineering, infrastructure, security, and product reviewers

## Executive summary

The recommended foundation for a self-hostable platform similar to Fly.io Sprites or Amp Orbs is:

- **Firecracker as the default Linux microVM runtime.** It offers the best combination of hardware isolation, density, small device surface, startup speed, and operational maturity for general-purpose Linux sandboxes.
- **Snapshot-restored templates instead of cold boots.** The latency users perceive includes scheduling, networking, disk attachment, guest startup, and application readiness—not just VM creation.
- **Local NVMe for the hot path and S3-compatible object storage for durability.** Use immutable base images plus copy-on-write state; upload deduplicated chunks and checkpoint metadata asynchronously.
- **A purpose-built node agent and scheduler, not Kubernetes or Nomad in the request path.** Kubernetes can deploy the control plane, but it should not be responsible for creating each sandbox.
- **A guest supervisor plus an inner container.** The supervisor owns lifecycle and management APIs; the user receives root-like control inside an OCI container within the microVM.
- **Cloud Hypervisor as a later, optional backend** when GPU/VFIO, Windows, live migration, richer device support, or dynamic resource resizing justify a larger VMM.

The concise product proposition is:

> Install one package on a KVM-capable Linux server and get persistent, checkpointable, auto-sleeping, hardware-isolated development machines with an SDK, terminal, and preview URLs—without Kubernetes, Nomad, or cloud lock-in.

There is no single universally fastest technology:

| Workload | Fastest practical category | Recommended choice |
|---|---|---|
| Full mutable Linux with strong tenant isolation | Snapshot-restored microVM | **Firecracker** |
| Full Linux where kernel isolation is unnecessary | Container | runc/LXC |
| Tiny functions with a constrained ABI | Wasm or lightweight sandbox | Wasmtime/WASI or Hyperlight |
| Specialized immutable application | Unikernel | Unikraft |
| GPU, Windows, hot-plug, or live migration | General-purpose cloud VMM | Cloud Hypervisor |

## 1. Scope and terminology

This review covers runtimes and platforms relevant to short-lived and persistent development environments, coding-agent sandboxes, CI workers, remote shells, and isolated application previews.

A **microVM** is a virtual machine optimized for small footprint and fast startup. It still runs a guest kernel and normally uses hardware virtualization through KVM on Linux. A **sandbox platform** adds images, scheduling, networking, persistence, identity, lifecycle APIs, observability, quotas, and a user-facing SDK around the runtime.

“Runs on any existing server” must be stated precisely. The Firecracker path requires:

- Linux on x86_64 or aarch64;
- hardware virtualization enabled in firmware;
- KVM exposed as `/dev/kvm`;
- sufficient kernel, cgroup, networking, and filesystem features;
- root or narrowly scoped privileged setup for KVM, TAP devices, namespaces, cgroups, and jail creation.

Firecracker documents its host prerequisites in its [getting-started guide](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md). It is not a native Windows or general macOS server backend. A macOS developer mode can use libkrun/HVF, but production parity should remain a Linux/KVM goal.

## 2. Evaluation criteria

The technologies were evaluated against:

1. isolation boundary and attack surface;
2. time from API request to a usable command or listening application;
3. steady-state CPU, memory, filesystem, and network overhead;
4. density and behavior under concurrent creation;
5. snapshot, fork, suspend, and resume support;
6. compatibility with normal Linux tooling and OCI images;
7. operational complexity and self-hostability;
8. suitability for persistent developer environments;
9. licensing and extensibility;
10. evidence quality behind performance claims.

## 3. Technology landscape

| Technology | Isolation model | Full Linux guest | Main advantage | Main limitation | Best use |
|---|---|---:|---|---|---|
| runc/LXC | Shared host kernel | No | Lowest general Linux overhead | Weakest tenant boundary in this comparison | Trusted workloads and single-tenant environments |
| gVisor | Userspace kernel intercepts syscalls | Semantically partial | OCI workflow with a stronger boundary than runc | Syscall, network, and filesystem overhead; compatibility gaps | Medium-trust container workloads |
| Firecracker | KVM microVM | Yes | Small device model, density, startup, snapshots | Intentionally limited devices and lifecycle features | Multi-tenant Linux sandboxes |
| Cloud Hypervisor | KVM/MSHV VM | Yes | Broader devices, hot-plug, VFIO, migration | Larger surface and more features to operate | GPU/Windows/specialized VM workloads |
| libkrun | Embedded KVM/HVF VM | Yes | Simple local embedding; Linux and macOS paths | Host confinement still needs careful design | Desktop/local sandbox engine |
| Kata Containers | OCI containers inside lightweight VMs | Yes | Kubernetes/CRI integration | Additional layers and Kubernetes-centric operation | Existing Kubernetes estates |
| Unikraft | Unikernel | Application-specific | Extremely fast specialized boot | Not a mutable general Linux environment | Functions and appliances |
| Wasmtime/WASI | Wasm capability sandbox | No | Very low startup and strong capability model | POSIX and Linux compatibility constraints | Portable functions/plugins |
| Hyperlight | In-process micro-VM-like sandbox | No guest OS | Very fast function isolation | Not a general Linux machine | Small untrusted function calls |

## 4. Firecracker

Firecracker is the strongest default for this product class. It uses KVM but deliberately exposes a minimal virtual device model. Each microVM is represented by a separate Firecracker process, which makes host accounting, cgroup placement, lifecycle ownership, and failure isolation straightforward.

The published Firecracker specification targets:

- no more than 125 ms from the `InstanceStart` API action to guest `/sbin/init` on supported configurations;
- no more than 5 MiB of VMM memory overhead for a small 1-vCPU, 128-MiB guest;
- at least 95% of bare-metal performance for compute-only workloads.

These are design targets, not end-to-end application-readiness guarantees. They exclude scheduler latency, image transfer, network setup, disk preparation, guest services, and application startup. See the [Firecracker specification](https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md).

### Strengths

- Narrow device surface and a production-oriented jailer/seccomp model.
- Good density because the VMM is small and each VM can be tightly cgroup-limited.
- Snapshot creation and restoration suitable for pre-initialized templates.
- Straightforward control through an HTTP API over a Unix socket.
- Apache-2.0 project with significant production deployment evidence.

### Limitations

- Linux guests only; no general BIOS/UEFI machine model.
- No GPU/VFIO path comparable to a richer VMM.
- Networking, IP allocation, routing, storage composition, and orchestration are intentionally left to the operator.
- Snapshots are coupled to host CPU compatibility, Firecracker version, guest kernel, and device configuration. A snapshot catalog needs strict compatibility keys.
- Firecracker starting quickly does not make an OCI image or large workspace appear instantly. The platform must solve image preparation and storage locality.

### Relevant implementation material

- [Root filesystem and kernel setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/rootfs-and-kernel-setup.md)
- [Snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)
- [Block I/O engines](https://github.com/firecracker-microvm/firecracker/blob/main/docs/api_requests/block-io-engine.md)
- [Production host setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md)

## 5. Cloud Hypervisor

Cloud Hypervisor is also Rust-based and KVM-focused but intentionally supports a broader cloud VM feature set. Recent releases include userfaultfd-based restore work, background memory prefaulting, live migration improvements, encrypted migration, and VFIO migration support. See the [Cloud Hypervisor releases](https://github.com/cloud-hypervisor/cloud-hypervisor/releases).

It should be an optional backend, not the MVP default.

Choose it when the platform needs:

- VFIO or GPU devices;
- Windows guests;
- live migration between hosts;
- memory/CPU/device hot-plug;
- a broader VM device model.

The original Firecracker NSDI paper measured both Firecracker and Cloud Hypervisor on the same hardware. Cloud Hypervisor was marginally faster in one serial-start test, while Firecracker produced a better high-concurrency p99 in that particular 2020 experiment (approximately 146 ms versus 158 ms). The result is historically useful, but it is not a current universal ranking. Hardware, kernels, VMM versions, storage, snapshot strategy, and workload dominate the observed result. See the [NSDI 2020 paper](https://www.usenix.org/system/files/nsdi20-paper-agache.pdf).

## 6. Fly.io Sprites

Sprites are the closest product reference for a persistent machine that feels disposable. The public design describes Fly Machines backed by Firecracker, a prepared base environment, an inner container for user work, real ext4 filesystems, sparse logical disks, durable object storage, and local NVMe caching. See Fly.io's [design and implementation article](https://fly.io/blog/design-and-implementation/) and the [Sprites lifecycle documentation](https://docs.sprites.dev/concepts/lifecycle/).

Important design lessons:

- Do not import and unpack a user OCI image on every creation request. Prepare templates before they enter the hot path.
- Make logical disks large and sparse; charge and transfer actual occupied blocks.
- Separate durable state from the host cache so a machine can be reconstructed elsewhere.
- Put a management service inside the guest for exec, files, ports, service restart, and checkpoints.
- Distinguish a warm wake, where processes remain, from a cold wake, where disk persists but processes restart.
- Treat connections as ephemeral even when process or filesystem state survives.

Published behavior describes roughly 100–500 ms warm wakes and 1–2 second cold wakes. Those numbers are product-level ranges, not guaranteed results on arbitrary self-hosted hardware.

The public material is not completely consistent about checkpoint semantics and timing: one description emphasizes live or millisecond-scale behavior, while another lifecycle description warns that a checkpoint can stop processes and take 10–30 seconds. The implementation should therefore define its own explicit operations instead of exposing one ambiguous “checkpoint” verb.

## 7. Amp Orbs

Amp Orbs provide a strong reference for product semantics rather than for a VMM choice. The backend hypervisor/VMM is not publicly specified, so Orbs should not be described as Firecracker-based or used in a low-level performance comparison without additional evidence.

Publicly visible product ideas include:

- a machine per agent thread;
- project snapshots used to create consistent environments;
- setup and resume hooks;
- short-lived identity using OIDC rather than copied long-lived secrets;
- integrated terminal, files, diffs, preview portals, collaboration, and automation;
- automatic pause when idle;
- explicit machine sizes and cost accounting.

Amp documents project behavior in [Customizing Orbs](https://ampcode.com/docs/orbs/customizing) and resource tiers in [Orb sizes and costs](https://ampcode.com/docs/orbs/sizes-and-costs). The most reusable lesson is that the user experience is the orchestration API and lifecycle model, not the hypervisor brand.

## 8. E2B

E2B is the closest open-source architectural starting point for a Firecracker sandbox cloud. Its documented architecture includes:

- a Firecracker microVM per sandbox;
- prebuilt and prebooted snapshots;
- lazy memory restoration with userfaultfd;
- hot-page prefetching;
- an in-process NBD-based copy-on-write root disk path;
- separate control and data planes;
- a guest environment daemon;
- node-level network-slot pools and caching;
- a Go node orchestrator.

See the [E2B architecture document](https://github.com/e2b-dev/infra/blob/main/docs/ARCHITECTURE.md) and [E2B infrastructure repository](https://github.com/e2b-dev/infra/).

### Why not simply fork E2B and stop there?

E2B is an excellent technical reference, but its self-hosted stack is a cloud deployment rather than a one-package server product. It brings Terraform, Nomad, PostgreSQL, Redis, object storage, and provider assumptions. Its repository also documents support gaps for a generic Linux deployment. Reusing selected components or patterns is attractive; adopting the entire operational topology would conflict with the “any KVM server” goal.

## 9. Daytona

Daytona is the better reference for a complete developer-facing self-host experience. It exposes APIs, dashboard/SDK workflows, preview/proxy behavior, SSH, storage, snapshots, and a deployable service composition. Its current repository licensing must be reviewed carefully for the intended distribution model; it is presently published under AGPL-3.0.

The relevant documents are the [Daytona architecture](https://www.daytona.io/docs/en/architecture/) and its [open-source deployment guide](https://github.com/daytonaio/daytona/blob/main/apps/docs/src/content/docs/en/oss-deployment.mdx).

Its public architecture describes Linux namespace-based isolation in important paths. Docker-in-Docker or Compose deployments may also weaken or disable resource controls. Therefore, latency claims such as “under 90 ms” are not directly comparable with the cold or snapshot-restored startup of a hardware-isolated Firecracker microVM.

## 10. Supporting projects

### Flintlock

[Flintlock](https://github.com/liquidmetal-dev/flintlock) manages Firecracker and Cloud Hypervisor microVM lifecycles. It is useful as a reference or node-runtime component, but it does not supply the complete project/snapshot/identity/preview experience.

### firecracker-containerd

[firecracker-containerd](https://github.com/firecracker-microvm/firecracker-containerd) integrates containerd with Firecracker. It remains useful research material but is not a plug-and-play orchestration product and adds a containerd-centered abstraction that may not match persistent development machines.

### Kata Containers

[Kata Containers](https://katacontainers.io/software/) is the right answer when an organization already wants Kubernetes/CRI semantics and VM-backed pods. It is not the shortest path to a lightweight standalone server appliance.

### libkrun

[libkrun](https://github.com/containers/libkrun) is attractive for embedding a VM runtime in desktop software and for macOS through HVF. A production service still needs host namespaces, resource limits, identity separation, networking, and confinement around it.

### Hyperlight, Unikraft, and Wasm

[Hyperlight](https://github.com/hyperlight-dev/hyperlight), [Unikraft](https://unikraft.org/docs/concepts/performance), and WASI runtimes can beat a full Linux microVM for narrow functions. They do so by changing the compatibility contract. They are complementary execution classes, not drop-in replacements for a shell where arbitrary package managers, daemons, databases, and build tools must work.

## 11. Which is fastest?

### The defensible answer

For **strongly isolated, mutable, general-purpose Linux environments**, a cached Firecracker snapshot is the best default. For **trusted full-Linux workloads**, containers are faster and simpler. For **small functions**, Wasm/Hyperlight/unikernels can start faster than a Linux VM. For **large, sustained I/O or specialized devices**, Cloud Hypervisor can be the better system even if its creation latency is not the lowest.

### Why published numbers do not form one leaderboard

Vendors measure different boundaries:

- VMM process start;
- API `InstanceStart` to guest init;
- snapshot restore to first guest instruction;
- shell command readiness;
- HTTP service readiness;
- cold image pull versus cached image;
- one sandbox versus hundreds created concurrently.

A valid comparison must start at the client request and end at a successful command or application response. It must publish cache state, concurrency, hardware, guest size, kernel, image, network setup, and percentile distribution.

## 12. Performance improvements that matter most

### 12.1 Remove general schedulers from the creation hot path

Use one controller-to-node RPC. The node agent should own preallocated cgroups, UID/GID ranges, network namespaces, TAP devices, IP addresses, jail directories, and cached snapshot handles. Kubernetes may run controllers and gateways, but creating a pod must not be a prerequisite for creating a microVM.

### 12.2 Snapshot after the guest is genuinely ready

Boot a minimal guest, start the supervisor and common services, establish entropy and device state safely, then snapshot. Maintain snapshot variants keyed by:

- CPU architecture and compatible CPU feature mask;
- Firecracker and snapshot format version;
- guest kernel and root image digest;
- vCPU and memory size;
- device layout and feature flags.

Snapshots must regenerate machine identity, SSH host material, random seeds, lease state, and credentials after restore.

### 12.3 Use lazy memory restore with measured hot-page prefetch

Userfaultfd allows the VM to resume without reading every memory page first. Record pages touched during the first seconds of representative resumes and prefetch only that working set. Re-profile whenever the guest image, kernel, or supervisor changes.

### 12.4 Optimize the guest boot path

- Use an uncompressed `vmlinux` when the memory/storage tradeoff is acceptable.
- Compile required drivers into the kernel.
- Avoid an initramfs unless required.
- Disable serial console logging on the production fast path.
- Use a minimal init and a long-running supervisor.
- Defer nonessential services until after readiness.

### 12.5 Match block I/O mode to workload

Keep a conservative synchronous profile for latency-sensitive small I/O and an asynchronous io_uring profile for high-throughput builds and scans. Firecracker's published block I/O material shows that the asynchronous engine can substantially increase read IOPS in its test configuration, with an initialization penalty around 110 ms. That is a profile choice, not a universal win; benchmark the actual kernel, filesystem, and device.

### 12.6 Make storage local-first and content-addressed

Use:

1. an immutable content-addressed base image;
2. a sparse copy-on-write overlay per sandbox;
3. block or extent manifests;
4. compressed immutable chunks in object storage;
5. a local NVMe read/write cache;
6. background upload and garbage collection.

For the MVP, reflinks on XFS/Btrfs or LVM/dm-thin are simpler. A custom NBD or virtio-block backend similar to E2B's design is a later optimization after profiling proves the need.

### 12.7 Schedule for locality and tail latency

Use power-of-two-choices scheduling among feasible nodes, then score:

- cached template/snapshot presence;
- free memory and CPU pressure;
- disk queue depth and page-fault load;
- tenant anti-affinity;
- failure domain;
- projected wake latency.

Avoid placing solely by free vCPU count. Snapshot cache misses and I/O contention dominate p99 latency.

### 12.8 Prevent suspend/resume thrashing

Maintain distinct idle thresholds:

- active to warm-suspended;
- warm-suspended to cold-stopped;
- cold state to archival or deletion.

Adapt thresholds to recent interaction and host memory pressure. A fixed short timer produces repeated wake/suspend cycles and worse user experience.

## 13. Recommended open-source architecture

```text
CLI / SDK / Web UI
        |
        v
API + Auth + Project Service
        |
        +---- PostgreSQL / SQLite metadata
        |
        v
Scheduler + Reconciler ---------- Object storage
        |                         templates, chunks,
        |                         checkpoints, manifests
        v
Node Agent(s) <-----------------> Route / Gateway catalog
        |
        v
Minimal privileged helper
        |
        v
Firecracker jailer -> Firecracker microVM
                         |
                         v
                  Guest supervisor
                         |
                         v
                  Inner user container
```

### 13.1 Control plane

Responsibilities:

- users, organizations, projects, policies, and quotas;
- desired state and reconciliation;
- cache-aware placement;
- template builds and snapshot compatibility metadata;
- checkpoint and storage manifests;
- preview route catalog;
- short-lived workload identity;
- audit events and billing/usage records.

Use SQLite in the single-node edition and PostgreSQL in the clustered edition. Do not introduce a custom Raft layer initially; the database is already the durable source of truth.

### 13.2 Node agent

The node agent should:

- report capacity, pressure, health, and cache inventory;
- own every Firecracker process and its state machine;
- allocate prepared network/cgroup/jail slots;
- download and verify signed templates;
- manage overlays, snapshots, and checkpoint uploads;
- expose metrics and structured lifecycle events;
- recover or garbage-collect orphaned processes after restart.

Run the main agent unprivileged. Put only required operations in a small, audited root helper with a narrow Unix-socket protocol.

### 13.3 Guest supervisor

Expose a versioned protocol over vsock for:

- exec and PTY sessions;
- streaming stdin/stdout/stderr;
- file upload/download and watches;
- process and task management;
- port/service discovery;
- filesystem freeze/thaw for consistent checkpoints;
- readiness, health, and resource metrics;
- resume/setup hooks;
- short-lived OIDC identity retrieval.

Users should receive root inside an inner OCI container or namespace, not in the supervisor's root namespace. This preserves a stable management channel even when the user breaks their own environment.

### 13.4 Gateway

Support wildcard routes such as `{port}-{sandbox}.{domain}`. The gateway should:

- authenticate private previews and apply share policies;
- buffer or retry the first request while a sandbox wakes;
- support HTTP, WebSockets, and eventually raw TCP;
- forward directly to the owning node or sandbox rather than through the control API;
- update routes from a strongly consistent or versioned route catalog.

The control plane should never proxy command or application data.

### 13.5 Lifecycle model

```text
absent -> preparing -> running -> warm-suspended -> cold-stopped
              ^          |             |                |
              |          +-------------+----------------+
              |                    resume
              +------------- recreate from durable state
```

Expose separate operations:

- **Filesystem checkpoint:** durable disk consistency point; processes may continue.
- **Warm suspend:** memory/process state retained on the same host.
- **Cold stop:** filesystem retained; processes restart on wake.
- **Durable hibernate:** memory and disk state uploaded; slower and more expensive.
- **Fork:** create a new sandbox from an immutable checkpoint manifest.

### 13.6 Self-hosted packaging and installation

Offer two deployment profiles from the same codebase:

| Profile | Components | State | Intended user |
|---|---|---|---|
| Standalone | API, scheduler, gateway, node agent, and Firecracker on one host | SQLite + local disk; optional S3-compatible backup | Individual server, lab, small team |
| Clustered | HA API/scheduler/gateway plus one or more independent nodes | PostgreSQL + required S3-compatible object store | Production and multi-host installations |

The node runtime must install as a native system service because it needs KVM, cgroups, network namespaces, TAP devices, and a privileged helper. The controller, dashboard, gateway, PostgreSQL, and optional object store can be distributed as OCI containers for convenience. Do not run the Firecracker node agent inside a privileged Docker-in-Docker container as the primary production installation path.

Ship:

- signed Debian and RPM packages for the node runtime;
- a versioned container image set for control-plane services;
- a single binary CLI for preflight, initialization, upgrades, backup, and diagnostics;
- systemd units with sandboxing directives;
- a Docker Compose file for the standalone control plane;
- Helm manifests only as an optional clustered control-plane deployment;
- Terraform/OpenTofu examples for common clouds, without making them dependencies;
- an offline artifact bundle for air-gapped installations.

A host preflight command should report, rather than silently modify:

- CPU architecture and virtualization flags;
- readable/writable `/dev/kvm` access;
- kernel, cgroup v2, user namespace, seccomp, vsock, TAP, nftables, and filesystem support;
- free CPU, memory, disk, inode, and network capacity;
- incompatible security modules or nested-virtualization restrictions;
- whether reflinks or thin provisioning are available;
- expected maximum guest sizes and a conservative density estimate.

The intended administrator flow is:

```text
platformctl preflight
platformctl init --mode standalone --data-dir /srv/platform
platformctl template build ./example-template.yaml
platformctl sandbox create --template example
platformctl doctor
```

For a cluster:

```text
platformctl control init --database <postgres-dsn> --object-store <s3-url>
platformctl node join --token <short-lived-join-token>
platformctl node drain <node-id>
```

These are proposed UX contracts, not commands from an existing implementation. Installation should be idempotent; upgrades should support node-by-node drain, snapshot compatibility checks, rollback of controller schema changes, and explicit backup validation.

## 14. Security baseline

For hostile multi-tenancy, require:

- jailer, seccomp, cgroup v2, per-VM UID/GID, and chroot/jail directories;
- unique network namespace, TAP device, IP, and firewall policy per VM;
- CPU, memory, PID, disk, IOPS, network, and log quotas;
- nftables-based egress policies and metadata-service blocking;
- signed image/snapshot manifests and encrypted object storage;
- no long-lived credential baked into templates or snapshots;
- short-lived OIDC tokens issued after every wake or restore;
- regenerated machine identity, random state, SSH material, and leases;
- mTLS between controller, nodes, and gateways;
- regular guest kernel, VMM, host kernel, and firmware patching;
- no cross-tenant KSM/deduplication of live memory;
- evaluation of SMT disablement for the highest-risk tenant class.

Snapshots are sensitive: they can contain plaintext secrets and process memory. Encrypt them, scope access per tenant, audit reads, and apply retention/deletion policies.

## 15. Proposed product and API surface

The first stable API should include:

- `projects`: template, source, setup/resume commands, environment policy;
- `templates`: build, publish, version, deprecate;
- `sandboxes`: create, inspect, exec, stop, suspend, resume, delete;
- `checkpoints`: create, list, restore, fork, expire;
- `ports`: expose, protect, share, revoke;
- `identity`: exchange sandbox identity for scoped external tokens;
- `events`: lifecycle, audit, usage, and failure streams;
- `nodes`: join, drain, inspect, remove.

Keep the public API runtime-neutral, but do not hide backend capabilities. Feature discovery should indicate whether a node pool supports Firecracker, Cloud Hypervisor, GPU, durable hibernation, or local-only execution.

## 16. Performance targets and benchmark plan

### Initial SLO targets

| Operation | p50 | p99 | Conditions |
|---|---:|---:|---|
| Create from cached ready snapshot | <100 ms | <250 ms | Template and hot pages cached on node |
| Warm wake on same node | <100 ms | <200 ms | Memory retained |
| Cold wake with local cached state | <500 ms | <1 s | Supervisor and services restart |
| Cold wake from object storage | <1 s | <2 s | Typical small changed working set |
| Filesystem checkpoint | <100 ms | <500 ms | Background upload excluded from acknowledgment only when durability policy permits |
| Fork from local checkpoint | <150 ms | <500 ms | Copy-on-write metadata operation |
| Steady compute | >95% bare-metal | — | Compute-bound benchmark |

These are product goals, not inherited Firecracker guarantees.

### Benchmark matrix

Measure from external API request to a verified command or HTTP response for:

- cache: hot, warm, cold, and object-store-only;
- concurrency: 1, 10, 50, 100, and 500 creations;
- workloads: Node.js, Python, Rust, C/C++, Git, SQLite, PostgreSQL, `fio`, and `iperf`;
- guest sizes: 1/2/4/8 vCPU and 512 MiB through 16 GiB;
- lifecycle: cold boot, snapshot restore, warm wake, cold wake, checkpoint, and fork;
- failure: killed node, partial upload, corrupt snapshot, object-store latency, and network partition;
- isolation: noisy CPU, memory pressure, I/O queue contention, fork bombs, log floods, and egress abuse;
- security: secret/identity regeneration after restore and cross-tenant access attempts.

Compare at least:

- runc;
- gVisor;
- Firecracker cold boot;
- Firecracker snapshot restore;
- Cloud Hypervisor;
- managed reference observations from E2B, Sprites, and Daytona where equivalent measurements are possible.

Publish raw data, host configuration, software versions, confidence intervals, p50/p95/p99, and failure counts.

## 17. Delivery plan

### Phase 0 — benchmark and feasibility spike (2 weeks)

- Validate KVM, Firecracker, vsock, TAP networking, cgroup v2, and snapshot restore on target hardware.
- Build one minimal kernel/rootfs and one realistic development image.
- Measure cold boot, cached snapshot restore, guest readiness, disk I/O, and concurrent starts.
- Decide whether the first storage backend uses reflinks, dm-thin, or a small NBD service.

**Exit:** repeatable end-to-end measurements and a written backend decision.

### Phase 1 — single-server MVP (4–6 weeks)

- Node agent, minimal root helper, Firecracker lifecycle, and crash reconciliation.
- Guest supervisor with exec/PTY/files/ports.
- SQLite metadata and local filesystem state.
- CLI and minimal API.
- Static preview gateway and basic authentication.

**Exit:** one installable package creates and manages isolated development machines on a supported KVM host.

### Phase 2 — fast creation (4–6 weeks)

- Template builder and signed artifact catalog.
- Ready snapshots, compatibility keys, lazy restore, and hot-page profiling.
- Preallocated network/cgroup/jail slot pools.
- End-to-end latency dashboards and regression gates.

**Exit:** cached snapshot creation meets the initial p99 goal under agreed concurrency.

### Phase 3 — persistent Sprite-like state (4–6 weeks)

- Sparse copy-on-write disks and immutable checkpoints.
- S3-compatible object-store backend and local cache.
- Warm suspend, cold stop, resume hooks, and fork.
- Upload durability, garbage collection, and repair tooling.

**Exit:** a sandbox survives host loss after its durability point and can resume on another node.

### Phase 4 — multi-node control plane (4–6 weeks)

- PostgreSQL metadata, node registration, scheduler, and reconciliation.
- Cache-aware placement, drain/evacuation, quotas, and multi-node gateway routing.
- HA controller and rolling node upgrades.

**Exit:** a node can fail or drain without losing durable sandbox state.

### Phase 5 — hostile multi-tenant hardening (6–8 weeks)

- Threat model and external security review.
- Egress policy, OIDC workload identity, audit logs, encryption, and rotation.
- Resource-abuse tests, chaos testing, snapshot compatibility migrations, and supply-chain signing.

**Exit:** documented isolation guarantees, incident procedures, and upgrade/rollback playbooks.

A useful single-node beta is plausible in **3–4 months** for a focused team. A credible hostile multi-tenant service is more realistically **6–9 months**, followed by ongoing security and kernel/VMM maintenance.

## 18. Build versus reuse recommendation

Reuse aggressively below the product boundary:

- Firecracker, jailer, KVM, Linux cgroups/namespaces, nftables;
- OCI image tooling and containerd libraries for image preparation, if useful;
- PostgreSQL, SQLite, S3-compatible APIs, OpenTelemetry;
- Envoy, HAProxy, or a similarly mature data-plane proxy;
- E2B's architecture as a performance reference;
- Daytona's self-hosting and developer UX as a product reference;
- Flintlock, Kata, and firecracker-containerd as implementation studies.

Build the differentiated layer:

- single-package node installation;
- runtime-neutral lifecycle API;
- persistent sparse disks and portable checkpoints;
- fast local caches and compatibility-aware snapshots;
- project setup/resume semantics;
- safe workload identity;
- preview routing and wake-on-request;
- multi-node reconciliation without a mandatory cluster orchestrator.

## 19. Main risks and review questions

| Risk | Impact | Mitigation / question |
|---|---|---|
| Snapshot/CPU incompatibility | Failed restores after upgrades or rescheduling | Strict compatibility keys; rebuild snapshots; test mixed node generations |
| Object-store latency | Slow cold wakes and forks | Chunking, locality-aware scheduling, hot-page and disk-block prefetch |
| Host privilege boundary | Node compromise | Tiny audited helper, unprivileged agent, jailer/seccomp, no broad root daemon API |
| Secrets captured in snapshots | Credential theft | Issue identity after restore; scrub templates; encrypt and scope snapshots |
| Warm state consumes memory | Poor density and high cost | Adaptive cold-stop policy and per-tenant warm quotas |
| Per-VM networking overhead | Slow p99 at burst | Prepared netns/TAP/IP pools; batch route updates |
| Product scope expands into a cloud | Slow delivery | Single-node edition first; explicit phase gates |
| Marketing benchmarks are incomparable | Wrong runtime decision | Run a reproducible end-to-end benchmark suite |

Reviewers should explicitly decide:

1. Is hostile multi-tenancy required for the first release, or is the first release trusted/single-tenant?
2. Is process-preserving durable hibernation a launch requirement, or are durable files plus service restart sufficient?
3. Must OCI images be accepted directly, or can projects build prepared templates asynchronously?
4. What is the first supported host matrix: Ubuntu/Debian only, or broader distributions?
5. Is a macOS local backend required, and may it have reduced feature parity?
6. What are the target guest sizes, active/warm ratios, and durability recovery-point objectives?

## 20. Final recommendation

Build a small, opinionated Firecracker platform before building a generalized cloud:

1. Ship a single-server Linux/KVM appliance.
2. Make command readiness and wake latency the primary measurements.
3. Prepare images and snapshots outside the request path.
4. Keep the hot path local: node cache, sparse overlay, prepared networking.
5. Make filesystem durability portable through S3-compatible storage.
6. Expose warm suspend, cold stop, checkpoint, and fork as distinct semantics.
7. Add multi-node scheduling only after single-node recovery and storage are reliable.
8. Add Cloud Hypervisor only for a validated capability gap.

That path captures the useful qualities of Sprites and Orbs while remaining installable on ordinary KVM-capable servers and avoiding an early dependency on Kubernetes or a specific cloud.

## Primary sources

- [Firecracker repository and host prerequisites](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md)
- [Firecracker specification](https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md)
- [Firecracker NSDI 2020 paper](https://www.usenix.org/system/files/nsdi20-paper-agache.pdf)
- [Firecracker snapshot support](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md)
- [Firecracker production host setup](https://github.com/firecracker-microvm/firecracker/blob/main/docs/prod-host-setup.md)
- [Cloud Hypervisor releases](https://github.com/cloud-hypervisor/cloud-hypervisor/releases)
- [Fly.io Sprites design and implementation](https://fly.io/blog/design-and-implementation/)
- [Sprites lifecycle](https://docs.sprites.dev/concepts/lifecycle/)
- [Fly.io suspend and resume](https://fly.io/docs/reference/suspend-resume/)
- [Amp Orbs customization](https://ampcode.com/docs/orbs/customizing)
- [Amp Orb sizes and costs](https://ampcode.com/docs/orbs/sizes-and-costs)
- [E2B architecture](https://github.com/e2b-dev/infra/blob/main/docs/ARCHITECTURE.md)
- [E2B infrastructure repository](https://github.com/e2b-dev/infra/)
- [Daytona architecture](https://www.daytona.io/docs/en/architecture/)
- [Daytona open-source deployment](https://github.com/daytonaio/daytona/blob/main/apps/docs/src/content/docs/en/oss-deployment.mdx)
- [Flintlock](https://github.com/liquidmetal-dev/flintlock)
- [firecracker-containerd](https://github.com/firecracker-microvm/firecracker-containerd)
- [Kata Containers](https://katacontainers.io/software/)
- [libkrun](https://github.com/containers/libkrun)
- [Hyperlight](https://github.com/hyperlight-dev/hyperlight)
- [Unikraft performance documentation](https://unikraft.org/docs/concepts/performance)
