# Phase 5 networking spike

Status: spike complete, including repaired Rust cold-restore/netd-restart
recovery. The first managed smoltcp integration and remaining Phase 5 scope are
in [STATUS-networking.md](../../docs/STATUS-networking.md).
This is a single-disposable-guest experiment, not production netd or a completed
networking/security acceptance gate.

The experiment is a standalone Cargo workspace, pinned to smoltcp 0.14.0.
It does not change production networking dependencies. The accompanying VMM
fix corrects HOST_TSO4/HOST_UFO feature bits from 4/5 to 11/14; libkrucible
rejects the previous mask with EINVAL when adding a network device.

## Product scope

Start with reliable outbound TCP and DNS for cloning repositories, installing
packages, and calling APIs. Default to isolation from the host, private networks,
metadata services, and unrelated sandboxes. Provide explicit exceptions, project
connections, and deliberately exposed preview ports. Decide wider UDP support
from real workloads. Existing TCP connections need not survive a snapshot;
new connections must work after resume and service restart.

Implementation can differ from Go. TCP termination, owner topology, HTTP
inspection, and secret substitution are not automatically compatibility
requirements. The access boundaries we select must actually be enforced.

## Reproduce the first probes

```sh
cargo test --manifest-path experiments/net-spike/Cargo.toml --locked
cargo clippy --manifest-path experiments/net-spike/Cargo.toml --locked --all-targets -- -D warnings
```

These tests use two smoltcp peers connected by bounded in-memory Ethernet queues.
No packets leave the process; 203.0.113.10 is a documentation address. Interfaces
have different configured addresses, so the destination probe does not merely
connect to a service on the gateway's own IP.

| Probe | What it establishes |
|---|---|
| AnyIP disabled | A foreign-address listener alone does not establish a connection. |
| AnyIP enabled, default route through gateway's own address | Foreign-address TCP establishes and transfers distinct request/response payloads. |
| Two overlapping connections to the same IP/port | Separate source ports retain independent payloads with separate socket instances. |
| Deliberately invalid ingress TCP checksum, default capabilities | The stack rejects it. |
| Same invalid ingress checksum, RX verification disabled | Handshake succeeds; outbound checksums remain valid to the verifying peer. |

Listeners are provisioned by the harness before the SYN arrives. Dynamic
admission, destination policy, host dialing, and connection reclamation are
exercised separately by the experimental binary and live harness below. The checksum probe models incomplete checksums by corrupting a
valid checksum; it does not reproduce the complete virtio offload/GSO path.
Do not interpret the two-peer test runtime as a throughput or latency benchmark.

## Evidence

Baseline source: agent-house main `779d5b1`.

- Local macOS: all five smoltcp probes pass; Clippy passes with warnings denied.
- `agent_house` Linux x86_64: Go baseline passes with
  `go test -count=1 -timeout=120s ./cmd/ahvm-netd ./pkg/gateway`.
  Reported package test times: 0.048s and 0.003s. These are test-suite runtimes,
  not networking performance measurements.
- `agent_house` Linux x86_64: all five smoltcp probes pass with the same lockfile.

The Go baseline was run from an isolated archive of main. The existing host
checkout was older and modified; neither it nor its VMM/rootfs was changed.
The five in-memory probes do not boot a guest. The live harness below does.

## Findings that affect the next experiment

1. smoltcp AnyIP plus routing can provide the foreign-destination receiving
   primitive. This is enough to continue evaluating it, not to select it yet.
2. The VMM currently advertises checksum and segmentation offload features in
   `rust/ahvm-vmm/src/ffi.rs`. Go netd disables receive checksum checking because
   its raw Ethernet stream lacks the corresponding virtio metadata. Evaluate
   disabling unsupported guest offloads versus handling them correctly before
   trusting a real-guest result. Do not simply enlarge a receive buffer and
   assume that handles segmentation.
3. Guest policy must bind to a host-authorized connection/identity, not just an
   address claimed inside an untrusted Ethernet frame. Explicitly test source
   IP/MAC spoofing in the integration gate.

## Original spike sequence and current progress

1. **Initial implementation complete:** real length-prefixed Unix-stream link and bounded dynamic flow
   admission. Forward admitted TCP flows to host sockets; bound connect/idle
   deadlines and buffers. Validate fragmented stream reads and backpressure.
2. **Initial UDP DNS forwarding complete:** explicit resolver configuration works on this host. Decide
   IPv6 posture explicitly; unsupported traffic must not create an access bypass.
3. **Outbound workload slice complete:** disposable guest clones a repo, installs dependencies,
   and calls HTTPS. Preview support and the Go comparison remain pending.
4. Test isolation (including identity spoofing), resource exhaustion, reconnect
   after restore/restart, and cleanup. Use controlled local upstream servers for
   reproducible load; public internet workloads are functionality checks.
5. Initial controlled measurements are recorded in [QUALIFICATION.md](QUALIFICATION.md).
   Broader throughput, latency, CPU, RSS, connection-count, and churn coverage remains. Agree thresholds and record a decision: continue Rust, change
   approach, or use the Go sidecar. Fuzzing/security review precede untrusted use.

No custom TCP implementation is planned. No final networking architecture has
been selected. The production `ahvm-netd` remains unchanged during this probe; only the VMM
feature-mask bug is fixed outside the experiment.

## References

- [smoltcp 0.14.0 source](https://github.com/smoltcp-rs/smoltcp/tree/v0.14.0)
- [Interface AnyIP API](https://docs.rs/smoltcp/0.14.0/smoltcp/iface/struct.Interface.html#method.set_any_ip)
- [Device/checksum capabilities](https://docs.rs/smoltcp/0.14.0/smoltcp/phy/struct.DeviceCapabilities.html)
- Existing baseline: `cmd/ahvm-netd/netstack.go`, `forward.go`, `delivery_test.go`,
  and `pkg/gateway/link.go`.


## Live TCP/DNS milestone

`src/main.rs` serves one fixed-identity guest over a Unix socket. It admits up to
64 TCP flows, uses 64 KiB per-direction TCP buffers, bounds the Ethernet queues,
and forwards up to 64 concurrent UDP DNS requests to an explicitly configured
resolver. Host connects have a five-second deadline and TCP inactivity expires
after 120 seconds. IPv6 and fragmented IPv4 are unsupported and dropped.
The initial one-millisecond sleep was replaced with socket-readiness polling.
Controlled isolation, resource, performance, and recovery results are in
[QUALIFICATION.md](QUALIFICATION.md), including the passing Rust recovery update.

Basic ingress MAC/IP pinning and public-destination filtering are present; pass
ALL host IPv4 addresses in the last argument. There is no multi-guest control
plane, preview support, private-network exception API, DNS-over-TCP fallback, or
security qualification. Receive TCP/UDP checksum validation is bypassed for the
current virtio stream. Real workloads passed with guest offloads enabled, but
that does not establish complete GSO/offload correctness.

The live harness booted an Alpine 3.22.1 minimal rootfs with the existing forge
copied from the host's Rust guest image. No host service, firewall, network
interface, existing guest image, or existing checkout was changed. The new VMM
was built in scratch space from main and the pinned submodule plus the two-bit
constant correction.

Successful runs: [initial results](results/linux-live-2026-09-09.json) and
[final fresh-image harness results](results/linux-live-fresh-2026-09-09.json).

| Guest workload | Result | Wall time for this run |
|---|---|---|
| Resolve Alpine repository via DNS gateway | Passed | 0.011 s |
| `apk update` and installation of Git, Curl, certificates, ethtool (16 packages) | Passed | 1.496 s |
| HTTPS GitHub API request with certificate verification | Passed | 0.255 s |
| Shallow clone of `octocat/Hello-World`, verify README | Passed | 0.838 s |
| Metadata and host HTTP attempts | Timed out as intended | about 2 s each |

These are single-run functionality timings, not comparative benchmarks. The
negative curls alone are not proof of isolation: controlled reachable endpoints,
spoofing, adversarial traffic, and exhaustion tests remain necessary.

The initial run using 1.1.1.1 timed out. The same DNS path succeeded with the
server's configured resolver, 185.12.64.2. Resolver selection is explicit rather
than hard-coded to an arbitrary public service.

### Reproduce on a Linux KVM host

Prepare an ext4 base image containing Alpine,
`/init.krun` (forge), `/workspace`, and trusted CA certificates. The harness copies it to WORK/guest.ext4 and writes only to that disposable copy. Build the corrected ahvm-vmm and the experiment:

```sh
cargo build --manifest-path experiments/net-spike/Cargo.toml --release --locked
python3 experiments/net-spike/live.py "$WORK" --image "$BASE_IMAGE" \
  --vmm "$VMM" \
  --prototype "$PWD/experiments/net-spike/target/release/ahvm-net-spike" \
  --resolver "$RESOLVER_IPV4" --host-ip "$HOST_IPV4_ADDRESSES"
```

`HOST_IPV4_ADDRESSES` is a comma-separated list. The VMM runtime needs libkrunfw
in `/usr/local/lib64` on this test host. The harness owns, terminates, and reaps
its VMM and networking child on success and failure. Logs and results are kept
in WORK. It will not replace an existing run marker. `AHVM_SPIKE_TRACE=1` adds
packet-header tracing for diagnosis; leave it off for measurements.

The final harness starts each run from a fresh base-image copy and requests a
guest sync before terminating its children. A repeat against the previously
abruptly stopped mutable image hit APK signature/cache errors; that run is not
counted as a networking pass. Persistence across restarts is a separate pending
gate, not established by these fresh-image runs.
