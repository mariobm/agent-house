# AWS Architecture and Cost Plan for a Firecracker Sandbox Platform

**Status:** Review draft

**Pricing snapshot:** 2026-09-03

**Primary regions modeled:** US East (N. Virginia), `us-east-1`; Europe (Frankfurt), `eu-central-1`

**Companion review:** [MicroVM Sandbox Platforms: Research, Performance, and Open-Source Orchestration Plan](./MICROVM_PLATFORM_RESEARCH.md)

## Executive recommendation

AWS can host the proposed open-source Firecracker platform without requiring bare-metal instances. As of this review, Amazon EC2 exposes nested virtualization on supported virtual instance families, allowing KVM and therefore Firecracker to run inside an EC2 instance.

The recommended AWS design is:

- **EC2 Auto Scaling groups of `m8i.4xlarge` nodes** for the first general-purpose sandbox pool.
- Enable nested virtualization explicitly in the launch template.
- **Firecracker runs directly under a custom node agent**; do not put Kubernetes, EKS, ECS, or Nomad in the per-sandbox creation path.
- Use **EBS gp3 as the initial node cache** and **S3 as the durable store** for templates, disk chunks, manifests, and cold checkpoints.
- Add **`m8id` or `i7i` local-NVMe pools** for I/O-heavy workloads after benchmarking. Local NVMe is a cache only, never the sole durable copy.
- Run the stateless API, controller, and HTTP gateway on **ECS Fargate** or a small conventional EC2 pool; use **RDS PostgreSQL** for clustered metadata.
- Use **an Application Load Balancer** for the API and HTTP/WebSocket previews. Add a Network Load Balancer only when raw TCP/SSH exposure is required.
- Keep at least one On-Demand sandbox node warm. Use Spot only for interruptible or durably checkpointed workloads.

Indicative monthly totals, before taxes and support, are:

| Deployment | `us-east-1` | `eu-central-1` | Intended use |
|---|---:|---:|---|
| Scheduled development node | **$180–$240** | **$215–$275** | About 160 running hours/month; no HA |
| One-node 24/7 pilot | **~$780** | **~$920** | Colocated control plane; 500 GB node cache |
| Two-node small production | **~$1,865** | **~$2,190** | Two AZs, managed control plane, 2 TB total cache |
| Six-node medium production | **~$5,960** | **~$6,920** | Three AZs, 5 TB S3 state, 10 TB/month internet traffic |

These are planning estimates, not AWS quotes. The dominant variables are node count, average utilization, EBS/NVMe choice, NAT processing, internet egress, and log volume.

## 1. Feasibility on AWS

### 1.1 Nested virtualization support

Amazon EC2 now supports KVM and Hyper-V as L1 hypervisors inside selected virtual EC2 instances. The Nitro hypervisor remains L0; the EC2 host instance is L1; Firecracker guests are L2.

AWS currently lists these supported families:

- C8i, M8i, R8i;
- C8id, M8id, R8id;
- C8i-flex, M8i-flex, R8i-flex;
- X8i;
- C7i, M7i, R7i;
- C7i-flex and M7i-flex;
- I7i.

KVM is an explicitly supported L1 hypervisor, and AWS states that enabling nested virtualization has no separate fee. AWS also recommends evaluating bare metal for strict latency or performance-sensitive workloads. See the current [EC2 nested virtualization guide](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/amazon-ec2-nested-virtualization.html) and the [2026 launch announcement](https://aws.amazon.com/about-aws/whats-new/2026/02/amazon-ec2-nested-virtualization-on-virtual/).

This is a material change from older AWS designs, where running Firecracker generally implied an EC2 bare-metal instance. Old deployment guides and cost comparisons may therefore overstate the minimum AWS cost.

### 1.2 Launch configuration

Nested virtualization must be enabled in the CPU options. A launch test can use:

```bash
aws ec2 run-instances \
  --image-id ami-REPLACE_ME \
  --instance-type m8i.4xlarge \
  --cpu-options "NestedVirtualization=enabled" \
  --subnet-id subnet-REPLACE_ME \
  --security-group-ids sg-REPLACE_ME \
  --iam-instance-profile Name=microvm-node
```

The node bootstrap must fail closed unless all of these succeed:

```bash
test -c /dev/kvm
test -r /dev/kvm
test -w /dev/kvm
```

It should then run a known Firecracker smoke VM, verify vsock and TAP networking, publish a node-ready signal, and only then enter the scheduler pool.

### 1.3 Important constraints

- The supported virtual families are currently Intel/x86_64. Firecracker supports aarch64, but an AWS Arm pool cannot assume nested KVM on ordinary Graviton instances unless AWS adds those families to the supported list. Use x86_64 first.
- Family and Availability Zone support changes over time. The deployment must discover offerings before selecting instance types.
- Nested virtualization adds another CPU, storage, and network translation layer. Do not assume bare-metal Firecracker performance; benchmark L2 guest workloads on every chosen family.
- Fargate and Lambda do not expose `/dev/kvm`, so they can run controllers and gateways but not Firecracker sandbox nodes.
- EKS can schedule privileged KVM workloads on compatible EC2 nodes, but it adds orchestration layers without solving snapshot locality or microVM lifecycle. It is optional for the control plane, not recommended for sandbox creation.

## 2. Recommended AWS architecture

```text
Users / CLI / SDK
        |
Route 53 + ACM
        |
Application Load Balancer
        |
        +--------------------+
        |                    |
Control API / Gateway     Preview traffic
(ECS Fargate, 2+ tasks)      |
        |                    |
        +------ Route catalog+
        |
RDS PostgreSQL <----> Scheduler / Reconciler
        |                    |
        |                    v
        |          EC2 Auto Scaling Group
        |          nested virtualization enabled
        |                    |
        |          +---------+---------+
        |          | Node agent        |
        |          | Firecracker VMs   |
        |          | EBS/NVMe cache    |
        |          +---------+---------+
        |                    |
        +--------------------+
                             |
                 S3 durable object store
                 templates, chunks, checkpoints
```

### 2.1 Service mapping

| Platform responsibility | AWS service | Recommendation |
|---|---|---|
| Public DNS | Route 53 | Wildcard record for preview domains |
| TLS certificates | ACM | Wildcard certificate where policy allows |
| HTTP/API/WebSocket entry | Application Load Balancer | Target custom gateway replicas |
| Raw TCP entry | Network Load Balancer | Add only when needed |
| Control API and gateway | ECS Fargate or small EC2 ASG | Two tasks/instances across AZs for production |
| Desired state and scheduling | Application service | Custom scheduler/reconciler; no per-VM ECS/EKS task |
| Metadata | RDS PostgreSQL | Single-AZ for pilot; Multi-AZ for production |
| Optional ephemeral coordination | ElastiCache/Valkey | Add only after profiling; PostgreSQL can handle MVP leases |
| Sandbox compute | EC2 ASG | Supported nested-virtualization families |
| Durable VM state | S3 | Versioned, encrypted, lifecycle-managed buckets |
| Node hot cache | EBS gp3 or local NVMe | Reconstructable from S3 |
| Image input | ECR | Store OCI inputs; convert to prepared templates asynchronously |
| Encryption | KMS | Separate keys or encryption contexts by environment/tenant class |
| Secrets | Secrets Manager/SSM Parameter Store | Control-plane bootstrap only; not copied into guest snapshots |
| Metrics and logs | CloudWatch + OpenTelemetry | Cap cardinality and retention; optionally export elsewhere |
| Events | EventBridge/SQS | Spot notices, asynchronous builds, checkpoint jobs |
| Host administration | Systems Manager | Avoid public SSH to nodes |

## 3. Sandbox node selection

### 3.1 Default: M8i

`m8i.4xlarge` provides 16 vCPUs and 64 GiB of memory. AWS describes up to 15 Gbps network and up to 10 Gbps EBS bandwidth for this size. It is EBS-only. See the [M8i instance specification](https://aws.amazon.com/ec2/instance-types/m8i/).

Why it is the baseline:

- balanced CPU-to-memory ratio for development environments;
- supported nested virtualization;
- broadly available compared with newer local-disk variants;
- enough capacity to test meaningful multi-sandbox density;
- smaller cost and failure domain than a large bare-metal node.

An initial capacity model should reserve about 8 GiB for the host, node agent, page cache, gateway helpers, and safety margin. That leaves approximately 56 GiB for guests.

For a standard 1-vCPU/2-GiB sandbox:

- memory limit: 28 theoretical guests;
- CPU limit without overcommit after host reserve: approximately 12–14 active guests;
- practical target with 2:1 CPU overcommit and mixed activity: **20–24 allocated guests per node**;
- warm-suspended guests still consume memory and must count against the memory limit.

These are scheduling assumptions, not guaranteed capacity. Measure page cache, Firecracker overhead, guest kernel memory, disk queues, and workload burstiness.

### 3.2 Local-NVMe option: M8id

`m8id.4xlarge` has the same 16-vCPU/64-GiB shape plus one 950-GB local NVMe SSD. It is an excellent hot-cache and copy-on-write disk host where available.

At the modeled US prices, one M8id node costs roughly $762/month, versus about $694/month for one M8i node plus 950 GB of baseline gp3. The approximately $68/month premium buys substantially lower-latency local storage and much higher local IOPS. It can become cheaper than heavily provisioned gp3 once additional IOPS and throughput charges are included.

Instance store is ephemeral. AWS documents that instance-store data is erased when the instance terminates. Every template, checkpoint, and acknowledged durable filesystem change therefore needs an S3 or EBS durability policy. See [EC2 termination behavior](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/how-ec2-instance-termination-works.html).

Regional availability is narrower than plain M8i, so the infrastructure must offer an EBS-backed fallback.

### 3.3 Storage/density option: I7i

`i7i.4xlarge` provides 16 vCPUs, 128 GiB memory, and about 3.75 TB local NVMe. It is attractive for:

- many warm memory-resident sandboxes;
- large repository and build caches;
- disk-intensive CI or agent workloads;
- reducing the number of cache misses from S3.

Its modeled On-Demand price is about $1,102/month in `us-east-1` and $1,314/month in Frankfurt. It is only cost-effective when the extra memory and NVMe are used. A half-empty I7i node is worse economics than M8i.

### 3.4 Family strategy

Benchmark at least:

- **C8i** for CPU-heavy, low-memory build workers;
- **M8i** for the general pool;
- **R8i** for warm, memory-heavy development sessions;
- **M8id/I7i** for local-I/O-heavy pools;
- **M7i** as a potentially lower-cost older-generation control comparison.

Do not place one snapshot indiscriminately across different CPU generations. The snapshot catalog should key artifacts by CPU feature baseline, VMM version, kernel, image, VM shape, and device layout.

## 4. Storage architecture on AWS

### 4.1 Durable layer: S3

Use S3 for:

- compressed base filesystem chunks;
- prepared kernels and root images;
- memory snapshot chunks for durable hibernation;
- disk checkpoint manifests;
- user workspace block/extent chunks;
- audit exports and recovery metadata.

Enable:

- bucket versioning where recovery value exceeds version-storage cost;
- server-side encryption with KMS;
- explicit tenant/environment prefixes and IAM conditions;
- lifecycle expiration for abandoned checkpoints;
- incomplete multipart-upload cleanup;
- object lock only for audit/compliance data that actually requires it;
- access logging or CloudTrail data events only where their additional cost is justified.

Use an **S3 gateway VPC endpoint** so node-to-S3 traffic avoids NAT Gateway processing. AWS's VPC pricing guidance notes that gateway endpoints for S3 do not carry the NAT hourly or processing charge. See [Amazon VPC pricing](https://aws.amazon.com/vpc/pricing/).

S3 Standard is modeled at $0.023/GB-month in `us-east-1` and approximately $0.0245/GB-month in Frankfurt. Request count and retrieval patterns also matter. Large immutable chunks, batched manifests, and deduplication prevent small-object request amplification.

S3 Express One Zone can deliver consistent single-digit-millisecond access for latency-sensitive data, but it is single-AZ and differently priced. Treat it as a measured optimization, not the durable multi-AZ source of truth. See [S3 storage classes](https://aws.amazon.com/s3/storage-classes/).

### 4.2 Node cache: EBS gp3

The first portable implementation should use one or more encrypted gp3 data volumes per node. gp3 includes 3,000 IOPS and 125 MB/s baseline performance; additional IOPS and throughput are billed separately. See [EBS pricing](https://aws.amazon.com/ebs/pricing/).

Recommended layout:

- small EBS root volume for the immutable host OS;
- separate data volume for templates, disk overlays, snapshot memory, and staging;
- XFS or ext4 according to the copy-on-write strategy;
- cache inventory rebuilt by the node agent after reboot;
- no controller metadata stored only on the cache volume.

For high-concurrency snapshot restore, baseline gp3 can become the bottleneck before CPU. Test queue depth, random read latency, throughput, and the cost of provisioned performance. At that point compare:

1. larger or faster gp3;
2. multiple gp3 volumes striped at the host;
3. io2 for strict latency;
4. an M8id/I7i local-NVMe node.

### 4.3 Persistence levels

Expose clear durability levels:

| Level | Stored where | Survives node loss | Typical latency | Use |
|---|---|---:|---:|---|
| Ephemeral | Node cache only | No | Lowest | Scratch CI |
| Local warm | Node RAM + cache | No | Sub-second target | Interactive idle sessions |
| Durable disk | S3 checkpoint/chunks | Yes | Seconds or background | Default developer workspace |
| Durable hibernate | S3 memory + disk | Yes | Highest | Explicit process-preserving checkpoint |

An API must not acknowledge “durable” until the required manifest and objects are committed and readable from S3 according to the stated recovery-point objective.

## 5. Networking design

### 5.1 VPC topology

Production baseline:

- two or three Availability Zones;
- public subnets only for load balancers and NAT gateways;
- private application subnets for controllers and gateways;
- isolated/private sandbox-node subnets;
- an S3 gateway endpoint;
- optional interface endpoints for ECR, SSM, CloudWatch, KMS, and Secrets Manager after comparing endpoint hourly fees against NAT processing.

Each Firecracker guest receives a TAP interface inside a node-owned network namespace. The node routes or NATs guest traffic; guests do not each receive an EC2 ENI. Assigning an ENI per microVM would hit instance and subnet limits, slow creation, and expose AWS networking operations in the hot path.

### 5.2 Ingress and previews

Use an ALB to terminate TLS and send API and wildcard preview traffic to two or more custom gateway tasks. The gateway uses the route catalog to forward to the owning node and microVM.

The gateway should:

- authenticate private previews;
- validate the sandbox/port mapping;
- ask the controller to wake a stopped sandbox;
- buffer or retry the first request within a bounded deadline;
- support WebSocket upgrades;
- remove routes before node drain completes.

The ALB low-traffic planning allowance is about $22/month in `us-east-1`, based on the fixed hourly price plus roughly one LCU. Actual cost follows the maximum of connection, bandwidth, and rule-evaluation dimensions. See [Elastic Load Balancing pricing](https://aws.amazon.com/elasticloadbalancing/pricing/).

Add an NLB only for raw TCP services. Do not pay for both load balancers before that feature is needed.

### 5.3 Egress isolation

Block guest access to:

- `169.254.169.254` and all host metadata/identity endpoints;
- the node agent and privileged helper sockets;
- control-plane and database subnets;
- other guest networks unless explicitly shared;
- AWS service endpoints not allowed by policy.

Offer policy tiers:

- no egress;
- allowlisted domains/services through a proxy;
- general internet egress with abuse controls;
- tenant-supplied egress policy.

NAT Gateway is simple but expensive for high-bandwidth sandboxes. In `us-east-1`, the modeled cost is $0.045 per gateway-hour, $0.045 per processed GB, plus public IPv4 and normal internet egress. Frankfurt is modeled at approximately $0.052/hour and $0.052/GB. Avoid sending S3 traffic through NAT; keep the gateway in the same AZ as its nodes to avoid cross-AZ charges.

## 6. Control plane and data plane

### 6.1 Controller and gateway

For production, run at least two ECS Fargate tasks across Availability Zones. A reasonable starting task size is 0.5 vCPU and 1 GiB for each API/controller replica, then separate gateway tasks if traffic or failure isolation requires it.

At current `us-east-1` Fargate Linux rates, 0.5 vCPU and 1 GiB is approximately:

```text
(0.5 × $0.04048 + 1 × $0.004445) × 730
= about $18.02 per task-month
```

Two always-on tasks are therefore about $36/month before logs and ephemeral storage. See [AWS Fargate pricing](https://aws.amazon.com/fargate/pricing/).

The control service maintains desired state, while each node reconciles local actual state. Commands and preview traffic go directly through the gateway to the node; they must not stream through the scheduler or database.

### 6.2 PostgreSQL

Use:

- SQLite colocated with the node for a developer/lab edition;
- RDS PostgreSQL Single-AZ for a pilot where temporary control-plane unavailability is acceptable;
- RDS PostgreSQL Multi-AZ for production.

The cost tables use allowances rather than a false-precision RDS quote because database class, storage, backup retention, I/O, and Multi-AZ topology change the total. The small-production allowance is $60/month in `us-east-1` and $75/month in Frankfurt. Validate it in the [RDS PostgreSQL pricing calculator](https://aws.amazon.com/rds/postgresql/pricing/) using the chosen class and deployment mode.

Redis/Valkey is not required on day one. PostgreSQL advisory locks, leases, and `LISTEN/NOTIFY` are sufficient for a small control plane. Add ElastiCache only when measured route lookup, queue, or lease pressure warrants another stateful service.

## 7. Scaling and availability

### 7.1 Node Auto Scaling

Publish custom metrics:

- allocatable guest memory;
- active and warm sandbox slots;
- runnable vCPU pressure;
- snapshot-cache hit rate;
- disk queue depth and free cache space;
- p95/p99 create and wake latency;
- number of pending placements.

Scale on projected allocatable slots and queueing delay, not average EC2 CPU alone. A node should join only after AMI bootstrap, KVM/Firecracker smoke tests, template preload, and gateway reachability complete.

Keep a minimum of one On-Demand node for development/pilot and at least one per required failure domain for production. EC2 Auto Scaling itself has no additional service fee; launched instances and monitoring are billed. See [EC2 On-Demand pricing](https://aws.amazon.com/ec2/pricing/on-demand/).

### 7.2 EC2 warm pools

EC2 Auto Scaling warm pools can retain pre-initialized instances in `Stopped`, `Hibernated`, or `Running` states. Stopped instances incur storage rather than compute charges. A stopped warm pool can shorten node-fleet scaling, but it is still far slower than restoring a Firecracker guest on an already-running node. Use it to prepare the next host, not to satisfy a sub-second sandbox wake SLO. See [EC2 Auto Scaling warm pools](https://docs.aws.amazon.com/autoscaling/ec2/userguide/ec2-auto-scaling-warm-pools.html).

### 7.3 Spot capacity

Use Spot for:

- ephemeral CI jobs;
- rebuildable template builds;
- cold sandboxes with recent durable disk checkpoints;
- overflow capacity where interruption is visible in the product contract.

Do not put the only current copy of an interactive workspace or warm process state on Spot.

Enable Capacity Rebalancing, diversify across supported families and Availability Zones, stop placement immediately on a rebalance signal, and checkpoint/drain. AWS notes that the rebalance recommendation can arrive with the two-minute interruption notice, so evacuation must finish in under two minutes and still tolerate no graceful window. See [EC2 rebalance recommendations](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/rebalance-recommendations.html) and [Auto Scaling Capacity Rebalancing](https://docs.aws.amazon.com/autoscaling/ec2/userguide/ec2-auto-scaling-capacity-rebalancing.html).

### 7.4 Failure behavior

The system must handle:

- node disappears without a drain;
- S3 checkpoint upload is partial;
- controller is unavailable while existing sessions continue;
- one AZ is unavailable;
- a route points at a stale node epoch;
- a new AMI cannot restore an older snapshot;
- Spot capacity cannot be replaced in the same family.

Use node epochs and fenced leases so two nodes cannot own the same writable sandbox state. Checkpoints should be immutable, content-addressed, and committed by a final atomic manifest write.

## 8. Security design on AWS

### 8.1 Shared responsibility

AWS protects the L0 Nitro layer and boundary between EC2 instances. The project remains responsible for the L1 host OS, KVM/Firecracker installation, L2 guest kernels, node agent, networks, images, snapshots, applications, and all tenant policy. AWS calls this out explicitly in its nested virtualization guidance.

### 8.2 IAM and identity

- Give node roles read-only access to template prefixes and narrowly scoped write access to their checkpoint staging paths.
- Keep database, KMS administration, and user-management privileges out of node roles.
- Use S3/KMS encryption contexts and session tags to narrow tenant access.
- Never expose the EC2 instance profile to a microVM.
- Issue a platform identity document to the guest over vsock, then exchange it for short-lived, audience-scoped OIDC tokens.
- Rotate identity after snapshot restore, fork, tenant reassignment, or node movement.

### 8.3 Network and host hardening

- Use private subnets and SSM Session Manager; do not expose SSH on sandbox nodes.
- Require IMDSv2 on the EC2 host and block all microVM routes to IMDS.
- Apply Firecracker jailer, seccomp, per-VM UID/GID, cgroup v2, and a unique jail/network namespace.
- Use signed AMIs, kernels, root images, and snapshot manifests.
- Enable EBS and S3 encryption; decide whether tenant-managed keys are a product requirement.
- Keep audit logs outside the node cache.
- Patch host kernels and Firecracker with rolling node drains and snapshot-compatibility tests.
- Use GuardDuty, Security Hub, and VPC Flow Logs selectively; their data volume can materially increase cost.

## 9. Performance strategy specific to AWS

### 9.1 Measure the nested layers

The complete I/O path is longer than on bare metal:

```text
L2 guest virtio -> Firecracker -> L1 Linux block/network -> Nitro -> EBS/ENA
```

Benchmark:

- L1 EC2 baseline versus L2 Firecracker guest;
- cold boot versus snapshot restore;
- gp3 baseline versus provisioned gp3 versus local NVMe;
- S3 cold-cache restore with and without gateway endpoint;
- one VM versus 10/50/100 concurrent restores;
- p50/p95/p99 command readiness, not only VMM start.

### 9.2 AWS-specific optimizations

1. Bake Firecracker, kernels, the node agent, and common templates into or alongside a versioned AMI.
2. Preload the highest-demand snapshot and hot memory pages during node admission.
3. Use M8id/I7i instance store as a disposable cache for high-I/O pools.
4. Put S3 endpoints and nodes in-region; avoid NAT and cross-AZ paths for checkpoint traffic.
5. Prefer one larger sequential range GET over many small object reads where restore behavior permits.
6. Separate template pools by CPU compatibility so snapshots do not fall back to cold boot unexpectedly.
7. Maintain spare guest slots on running nodes; EC2 fleet scaling is measured in tens of seconds or minutes, not milliseconds.
8. Drain before AMI replacement and pre-warm the replacement node's cache.
9. Use io_uring profiles only after measuring nested EBS and NVMe behavior.
10. Track cost per ready sandbox-hour alongside latency; low node CPU can still mean healthy capacity if warm sessions are memory-bound.

## 10. Cost-model assumptions

All amounts are USD and rounded. Base estimates use:

- 730 hours per month;
- Linux On-Demand compute;
- no Savings Plan, Reserved Instance, Spot discount, credits, free tier, tax, VAT, or AWS Support plan;
- `m8i.4xlarge` at approximately $0.8467/hour in `us-east-1` and $1.0143/hour in Frankfurt;
- gp3 at $0.0800/GB-month in `us-east-1` and $0.0952/GB-month in Frankfurt, without extra provisioned IOPS/throughput;
- S3 Standard at $0.0230/GB-month in `us-east-1` and approximately $0.0245/GB-month in Frankfurt;
- ALB allowance of about $22/month in `us-east-1` and $27/month in Frankfurt at low usage;
- NAT at $0.045/hour and $0.045/processed GB in `us-east-1`; approximately $0.052/hour and $0.052/GB in Frankfurt;
- public IPv4 at $0.005/address-hour;
- internet egress modeled at $0.09/GB after the first 100 GB/month aggregated free allowance;
- CloudWatch logs approximated at $0.50/ingested GB in `us-east-1`, with regional variation;
- request charges, backups, cross-AZ traffic, detailed metrics, and KMS requests represented only by a small miscellaneous allowance unless explicitly shown.

Current prices should be re-entered in the [AWS Pricing Calculator](https://calculator.aws/) before approval. The official source pages are linked in the final section.

## 11. Cost scenarios

### 11.1 Scheduled development environment

Assumptions:

- one `m8i.4xlarge` running 160 hours/month;
- 200 GB gp3 retained for the month;
- 100 GB S3 state;
- no ALB, NAT Gateway, or RDS;
- access through SSM, VPN, or a temporary development endpoint;
- $20 egress and $8–$15 observability/miscellaneous allowance.

| Component | `us-east-1` | Frankfurt |
|---|---:|---:|
| EC2 compute | $135 | $162 |
| 200 GB gp3 | $16 | $19 |
| 100 GB S3 | $2 | $2.50 |
| Egress, logs, IPv4/misc. | $27–$87 | $31–$91 |
| **Estimated total** | **$180–$240** | **$215–$275** |

This is the lowest-cost useful engineering environment. Automate start/stop schedules and budget alarms; EC2 stopped time still incurs EBS storage.

### 11.2 One-node 24/7 pilot

Assumptions:

- one `m8i.4xlarge` always running;
- controller and SQLite colocated;
- 500 GB gp3 node cache;
- 250 GB S3 durable state;
- one ALB and one NAT Gateway;
- 100 GB NAT-processed traffic;
- 500 GB total internet egress, of which 400 GB is billed after the free allowance;
- 20 GB logs and a small DNS/KMS/secrets allowance.

| Component | `us-east-1` | Frankfurt |
|---|---:|---:|
| Sandbox EC2 | $618 | $740 |
| 500 GB gp3 | $40 | $48 |
| 250 GB S3 | $6 | $6 |
| ALB | $22 | $27 |
| NAT + IPv4 + 100 GB processing | $41 | $47 |
| 400 GB billable internet egress | $36 | $36 |
| Logs and miscellaneous | $15 | $18 |
| **Estimated total** | **$778** | **$922** |

This configuration is not highly available. A host failure interrupts all running sandboxes; only committed S3 state is portable.

### 11.3 Two-node small production

Assumptions:

- two `m8i.4xlarge` nodes across two AZs;
- 1 TB gp3 cache per node;
- two 0.5-vCPU/1-GiB Fargate controller/gateway tasks;
- managed PostgreSQL allowance;
- 1 TB S3 state;
- one ALB;
- NAT Gateway in each AZ and 500 GB processed traffic;
- 2 TB total internet egress, of which 1.9 TB is billed;
- 100 GB log ingestion in the US model, represented by a $50 allowance;
- DNS, KMS, secrets, and request allowance.

| Component | `us-east-1` | Frankfurt |
|---|---:|---:|
| 2 × sandbox EC2 | $1,236 | $1,481 |
| 2 TB total gp3 | $160 | $190 |
| Fargate control/gateway | $36 | $42 |
| RDS PostgreSQL allowance | $60 | $75 |
| 1 TB S3 | $23 | $25 |
| ALB | $22 | $27 |
| 2 × NAT + IPv4 + processing | $96 | $109 |
| 1.9 TB billable internet egress | $171 | $171 |
| Logs | $50 | $60 |
| DNS/KMS/secrets/requests | $10 | $10 |
| **Estimated total** | **$1,864** | **$2,190** |

This gives host redundancy but not enough spare capacity to guarantee full load after one node fails. For N+1 capacity, either cap normal use below 50%, add a third node, or allow degraded performance while Auto Scaling replaces the host.

### 11.4 Six-node medium production

Assumptions:

- six `m8i.4xlarge` nodes across three AZs;
- 1 TB gp3 per node;
- four Fargate tasks across controllers and gateways;
- Multi-AZ database allowance;
- 5 TB S3 state;
- higher ALB/LCU allowance;
- three NAT Gateways and 2 TB processed traffic;
- 10 TB total internet egress, of which 9.9 TB is billed in the simplified model;
- 500 GB log ingestion in the US model;
- broader KMS, DNS, secrets, and request allowance.

| Component | `us-east-1` | Frankfurt |
|---|---:|---:|
| 6 × sandbox EC2 | $3,709 | $4,443 |
| 6 TB total gp3 | $480 | $571 |
| Fargate control/gateway | $72 | $84 |
| RDS PostgreSQL allowance | $160 | $190 |
| 5 TB S3 | $115 | $123 |
| ALB/LCUs | $50 | $60 |
| 3 × NAT + IPv4 + processing | $200 | $229 |
| 9.9 TB billable internet egress | $891 | $891 |
| Logs | $250 | $300 |
| DNS/KMS/secrets/requests | $30 | $30 |
| **Estimated total** | **$5,957** | **$6,921** |

At this size, bandwidth and logs together approach the price of multiple compute nodes. Meter them per tenant and make retention, preview bandwidth, and build-output transfer visible in quotas.

## 12. Unit economics

### 12.1 Compute per occupied sandbox-hour

On one `m8i.4xlarge` in `us-east-1`:

| Average simultaneously occupied sandboxes | Raw node compute per sandbox-hour |
|---:|---:|
| 8 | $0.106 |
| 12 | $0.071 |
| 16 | $0.053 |
| 20 | $0.042 |
| 24 | $0.035 |

In Frankfurt, the same values are about 20% higher because the modeled node rate is $1.0143/hour.

This is only EC2 node compute. A sellable internal cost must also allocate:

- host reserve and unused failover capacity;
- control plane and database;
- EBS/NVMe cache;
- S3 retained state and requests;
- checkpoint upload/download traffic;
- internet and cross-AZ transfer;
- logs, metrics, support, and engineering operations.

### 12.2 Persistent state per sandbox

At `us-east-1` S3 Standard rates:

| Actual durable bytes per sandbox | Storage cost/month |
|---:|---:|
| 5 GB | $0.12 |
| 10 GB | $0.23 |
| 20 GB | $0.46 |
| 50 GB | $1.15 |
| 100 GB | $2.30 |

A “100 GB disk” should be a sparse logical limit, not a fully provisioned 100-GB object. Track actual unique compressed chunks. Memory hibernation can be more expensive than workspace storage: 100 stopped 2-GiB VMs with non-deduplicated full memory images would add roughly 200 GB before compression and version retention.

### 12.3 Capacity planning formula

For each pool:

```text
required_nodes = max(
  ceil(total_allocated_guest_memory / allocatable_memory_per_node),
  ceil(peak_active_vcpu / allowed_cpu_overcommit_per_node),
  ceil(peak_disk_iops / safe_iops_per_node),
  minimum_failure_domain_nodes
) + failure_headroom
```

Use measured p99 saturation thresholds rather than manufacturer maximums for `safe_iops_per_node` and network throughput.

## 13. Cost-reduction plan

Ranked by likely impact:

1. **Increase useful node occupancy.** Cache-aware placement and controlled CPU overcommit usually save more than small service-price tuning.
2. **Cold-stop idle sandboxes.** Warm memory has a real opportunity cost even when CPU is idle.
3. **Schedule non-production nodes.** Reducing one M8i node from 730 to 160 hours saves about $483/month in `us-east-1`.
4. **Use S3 gateway endpoints.** Do not pay NAT processing for image and checkpoint traffic.
5. **Use local NVMe where it replaces provisioned EBS performance.** Compare total gp3 capacity + extra IOPS/throughput against the M8id/I7i premium.
6. **Use a blended On-Demand/Spot fleet.** Keep interactive/durable capacity On-Demand; put interruptible jobs on diversified Spot pools.
7. **Purchase Savings Plans only after utilization stabilizes.** Commit to the measured always-on base, not peak capacity.
8. **Bound logs and metrics.** Per-process or per-sandbox debug streams can quietly become a major bill.
9. **Avoid unnecessary cross-AZ traffic.** Keep node, NAT, and gateway paths AZ-local where possible; S3 remains regional durability.
10. **Meter internet egress.** Repository clones, package downloads, model artifacts, and preview traffic can dominate the bill.

### Spot sensitivity example

If four of six medium-production nodes average a 60% discount while two remain On-Demand, modeled US sandbox compute falls from about $3,709 to about $2,225, saving approximately $1,484/month. Spot rates and interruptions vary; this is a sensitivity case, not a quote or guaranteed saving.

### M8id sensitivity example

Replacing one M8i + 950 GB gp3 cache with one M8id changes modeled US monthly infrastructure from about $694 to $762, a premium near $68. The decision is justified only if local NVMe improves throughput/latency enough to increase occupancy, reduce cold starts, or avoid extra provisioned EBS performance.

## 14. Implementation plan on AWS

### Phase A — AWS feasibility and benchmark environment (2 weeks)

- Confirm nested virtualization in target regions and AZs.
- Launch M7i/M8i/C8i/R8i candidates with the CPU option enabled.
- Build a minimal host AMI with Firecracker, jailer, node agent, nftables, and telemetry.
- Compare Firecracker L2 performance with the L1 EC2 host.
- Benchmark gp3 baseline, provisioned gp3, and one local-NVMe family.
- Validate S3 upload/restore and snapshot compatibility across replacement nodes.

**Gate:** select one default node type using end-to-end latency and cost per occupied sandbox-hour.

### Phase B — single-node AWS pilot (3–5 weeks)

- VPC, S3 gateway endpoint, KMS key, encrypted buckets and volumes.
- One sandbox node, colocated controller/SQLite, SSM administration.
- ALB and wildcard Route 53/ACM preview routing.
- CloudWatch dashboard, budget, anomaly alert, and shutdown automation.
- S3 durability and restore test after terminating the node.

**Gate:** recreate every durable sandbox from S3 on a new EC2 instance.

### Phase C — production control plane (4–6 weeks)

- Two or more Fargate controller/gateway tasks.
- RDS PostgreSQL with backups and tested restore.
- Node registration, fenced leases, cache-aware placement, drain, and health epochs.
- Multi-AZ load balancing and private subnets.
- Tenant quotas, OIDC workload identity, and audit events.

**Gate:** controller/task/AZ failure does not corrupt sandbox ownership or durable state.

### Phase D — elastic fleet and cost controls (4–6 weeks)

- EC2 Auto Scaling custom metrics and lifecycle hooks.
- Stopped warm pool for prepared hosts if measurements justify it.
- Mixed-instance pools and snapshot CPU compatibility classes.
- Spot pool for interruptible workloads with Capacity Rebalancing.
- Per-tenant usage, bandwidth, storage, and log metering.
- Savings Plan recommendation based on at least 30–60 days of utilization.

**Gate:** load test, Spot interruption test, node-loss chaos test, and cost dashboard pass.

## 15. Infrastructure-as-code layout

Implement with Terraform or OpenTofu modules that can also serve as the public self-host AWS installer:

```text
infra/
  modules/
    network/
    endpoints/
    storage/
    kms/
    database/
    control_plane/
    edge/
    node_iam/
    sandbox_node_pool/
    observability/
    budgets/
  environments/
    dev/
    staging/
    production/
```

The `sandbox_node_pool` module should expose:

- region/AZ discovery and compatible instance overrides;
- `NestedVirtualization=enabled` in CPU options;
- On-Demand/Spot mix and Capacity Rebalancing;
- AMI and snapshot compatibility generation;
- root and cache-volume sizing/performance;
- target spare slots, warm-pool size, and scale limits;
- S3 prewarm manifest;
- node drain and lifecycle hooks;
- security groups, IAM role, and SSM access.

Do not bind core application logic to AWS APIs. Define object-store, node-discovery, identity, and routing interfaces so the same open-source controller can run on an ordinary Linux server, another cloud, or an AWS deployment.

## 16. Acceptance criteria for the AWS release

### Functional

- A supported virtual EC2 node launches Firecracker guests through `/dev/kvm`.
- A sandbox can be created, exec'd into, exposed over HTTPS, checkpointed, stopped, restored, and deleted.
- Durable state restores on a newly launched node in another AZ.
- Guest access to IMDS and control-plane networks is denied.
- Node replacement and controller restart reconcile without duplicate ownership.

### Performance

- Cached snapshot create: p50 <100 ms and p99 <250 ms, measured API-to-command-ready.
- Warm wake: p99 <200 ms on the same node.
- S3-backed cold wake: p99 <2 seconds for the agreed reference workspace.
- Steady compute: target >90% of the L1 EC2 baseline initially, then optimize; compare separately with physical bare metal.
- Node admission and cache prewarm time are measured and included in fleet-scale planning.

### Reliability and security

- Forced node termination loses no state acknowledged as durable.
- Spot interruption exercise drains or recovers all eligible sandboxes.
- RDS restore, S3 object recovery, and KMS rotation procedures are tested.
- Snapshot identity/secrets are regenerated after restore and fork.
- External threat review covers L1 host, root helper, Firecracker API socket, vsock protocol, and guest egress.

### FinOps

- Every node, sandbox, checkpoint, and preview route has tenant/project cost tags.
- Dashboards show occupied sandbox-hours, warm-memory-hours, actual stored bytes, egress, logs, cache hit rate, and cost per ready sandbox-hour.
- AWS Budgets and Cost Anomaly Detection alerts are configured before production traffic.
- The team reviews actual versus modeled cost after 7, 30, and 60 days.

## 17. Risks and decisions required

| Decision or risk | Why it matters | Recommended first answer |
|---|---|---|
| Nested versus bare metal | Performance and cost differ materially | Start nested M8i; benchmark bare metal only if SLOs fail |
| EBS versus local NVMe | Portability versus latency/IOPS | gp3 MVP; M8id/I7i measured pool later |
| One versus multiple AZs | Cost, availability, cross-AZ transfer | One AZ dev; two AZ small production; three AZ mature production |
| Warm memory retention | Fast UX consumes expensive RAM | Tenant warm quotas and adaptive cold-stop |
| RDS Multi-AZ timing | HA versus fixed cost | Single-AZ pilot; Multi-AZ before production SLA |
| NAT Gateway design | Simple but can dominate transfer cost | S3 gateway endpoint; one NAT/AZ only where required |
| Spot eligibility | Savings versus interruption | Opt-in workload class; never sole durable copy |
| Log retention | Debug value versus cost | Short hot retention, sampled metrics, S3 archive if needed |
| Arm support | Potential price/performance, but no listed virtual nested families | x86 first; revisit when officially supported |
| AWS coupling | Easier operations can undermine self-hostability | Keep AWS adapters outside the core scheduler/runtime APIs |

## 18. Final recommendation

Approve an AWS proof of concept using one `m8i.4xlarge`, 500 GB gp3, S3, and a colocated control plane. Budget approximately **$780/month in `us-east-1`** or **$920/month in Frankfurt** for a continuously running, internet-accessible pilot; or approximately **$180–$275/month** if the engineering environment runs only during working hours and omits production networking services.

The proof of concept must answer three questions before a production commitment:

1. What is the nested Firecracker penalty relative to the L1 EC2 host for representative builds, filesystem workloads, and networking?
2. Does gp3 meet concurrent restore and build p99 targets, or does local NVMe improve total cost per occupied sandbox-hour?
3. How many active and warm standard sandboxes fit on one node before p99 latency becomes unacceptable?

If those results are satisfactory, move to two nodes, Fargate controllers/gateways, RDS PostgreSQL, and multi-AZ routing at approximately **$1,865–$2,190/month**. Preserve the open-source product boundary: AWS should be a deployment adapter, not a required runtime dependency.

## Primary AWS sources

- [EC2 nested virtualization guide](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/amazon-ec2-nested-virtualization.html)
- [EC2 nested virtualization launch announcement](https://aws.amazon.com/about-aws/whats-new/2026/02/amazon-ec2-nested-virtualization-on-virtual/)
- [M8i and M8id instance specifications](https://aws.amazon.com/ec2/instance-types/m8i/)
- [EC2 On-Demand pricing](https://aws.amazon.com/ec2/pricing/on-demand/)
- [EBS pricing](https://aws.amazon.com/ebs/pricing/)
- [S3 pricing](https://aws.amazon.com/s3/pricing/)
- [S3 storage classes](https://aws.amazon.com/s3/storage-classes/)
- [Amazon VPC and NAT Gateway pricing](https://aws.amazon.com/vpc/pricing/)
- [Elastic Load Balancing pricing](https://aws.amazon.com/elasticloadbalancing/pricing/)
- [AWS Fargate pricing](https://aws.amazon.com/fargate/pricing/)
- [RDS for PostgreSQL pricing](https://aws.amazon.com/rds/postgresql/pricing/)
- [CloudWatch pricing](https://aws.amazon.com/cloudwatch/pricing/)
- [EC2 termination and instance-store behavior](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/how-ec2-instance-termination-works.html)
- [EC2 Auto Scaling warm pools](https://docs.aws.amazon.com/autoscaling/ec2/userguide/ec2-auto-scaling-warm-pools.html)
- [EC2 rebalance recommendations](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/rebalance-recommendations.html)
- [EC2 Auto Scaling Capacity Rebalancing](https://docs.aws.amazon.com/autoscaling/ec2/userguide/ec2-auto-scaling-capacity-rebalancing.html)
- [AWS Pricing Calculator](https://calculator.aws/)
