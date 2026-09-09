# Networking qualification — 2026-09-09

Decision: continue evaluating smoltcp, but keep PR #16 draft. The initial
performance problem was the prototype's fixed sleep, not an established stack
limitation. Two shared VMM recovery failures block this phase's exit.

## Setup and scope

All runs used agent_house, Linux x86_64, an isolated network namespace with no
internet route, a local controlled HTTP upstream, and identical disposable
Alpine guests (1 vCPU, 512 MiB). The Go candidate was built from agent-house main
779d5b1. The Rust VMM had only the network-feature-bit correction and used pinned
libkrucible 2e4313f. Candidate netds run separately; the second comparison reverses
the order. Neither candidate changes production networking.

11.0.0.1 represents the allowed upstream **inside this private namespace only**;
11.0.0.2 represents a forbidden host address. Both are assigned to its loopback
interface, not the host's interface. No packet can route to these addresses on
the internet. The configuration deliberately distinguishes the simulated public
upstream from simulated host services. This is a controlled gateway benchmark,
not a deployment policy for a real host.

Commands (paths must refer to isolated artifacts):

```sh
unshare --net python3 experiments/net-spike/isolation.py "$RUST_NETD" "$ISOLATION_WORK"
unshare --net python3 experiments/net-spike/qualify.py "$WORK" \
  --image "$BASE_IMAGE" --vmm "$VMM" --rust "$RUST_NETD" --go "$GO_NETD" \
  --order go,rust
```

Use a different WORK and `--order rust,go` for the reversed run. The guest needs
forge, Python 3, Curl, and a shell. The scripts assert they are outside PID 1's
network namespace before configuring addresses. Processes are owned and reaped
by each harness. Neither script targets unrelated workers.

`qualify.py` returns failure if any recovery check fails, even when performance
succeeds. Its structured report preserves all completed checks; it does not turn
a recovery failure into a skipped/passing test. The supplied reports fail this
criterion. The exit-status aggregation was added after collecting them and was
checked against both saved reports.

## Isolation and resource bounds

[Before fixes](results/isolation-before.json) / [final results](results/isolation-final.json).

- Positive control reaches the upstream. Direct host-side controls prove the
  supposedly forbidden services are reachable before sending guest frames.
- Host, loopback, private, metadata, and other-sandbox addresses are denied.
- Forged source IP and MAC are denied, as are fragmented IPv4 packets.
- Found and fixed: destination TCP port zero panicked; source port zero and
  SYN+FIN could initiate host connections. All now reject without terminating
  the prototype or opening an upstream connection.
- A deterministic corpus of 2,000 malformed frames survives without upstream
  connections. This is not coverage-guided fuzzing or a security review.
- 1,000 distinct half-open attempts produce exactly 64 upstream connections,
  71 open netd descriptors, and 9,356 KiB RSS. The guest stream is drained during
  this test so receive backpressure does not masquerade as the admission limit.

Scope is one guest with a fixed host-assigned identity. This does not establish
multi-tenant isolation, every malformed-packet case, sustained flood resistance,
DNS policy completeness, or safe handling of all checksum/GSO combinations.

## Performance

[Fixed-sleep baseline](results/qualification-fixed-sleep.json),
[Go-first readiness-poll run](results/qualification-poll-go-first.json),
[Rust-first readiness-poll run](results/qualification-poll-rust-first.json).

Each run measures 20 new-connection small HTTP requests, three 8 MiB downloads,
three 8 MiB uploads, and 100 requests at concurrency four. Every payload is
verified by SHA-256. Times include client/server and payload verification costs,
not just packet processing. DNS and TLS are not part of this controlled load.

| Metric | Rust after readiness polling | Go gVisor baseline |
|---|---:|---:|
| Median small-request latency | 0.318–0.342 ms | 0.383–0.397 ms |
| p95 small-request latency, Go-first run | 0.604 ms | 0.680 ms |
| Median 8 MiB download throughput | 332–337 MiB/s | 255–261 MiB/s |
| Median 8 MiB upload throughput | 705–723 MiB/s | 1,380–1,422 MiB/s |
| Netd peak RSS sampled via VmHWM | 3.38–3.52 MiB | 16.41–19.76 MiB |

The original fixed 1 ms sleep achieved only 12.6 MiB/s downloads and 25.5 MiB/s
uploads. It now waits on useful socket readiness, connect-completion wakeups,
and smoltcp timers. Idle sockets are not continuously polled for writability.
The 100-request churn test passes and descriptor counts return to their initial
values. Idle CPU was below the 0.01-second accounting resolution in a two-second
sample for both candidates; this is not evidence of literally zero CPU usage.

The upload gap is still material. These are two small local runs, not a broad
performance guarantee or proof of parity under contention, multiple guests,
loss, long-lived sessions, or a production security policy.

## Recovery: failed for both candidates

| Check | Rust | Go |
|---|---|---|
| Fresh boot and new TCP connection | Pass | Pass |
| New connection after benchmark load | Pass | Pass |
| Pause → resume → new TCP connection | Pass | Pass |
| Snapshot command (about 573 MB) | Pass | Pass |
| Fresh worker cold restore → new TCP connection | **Fail** | **Fail** |
| Independently fresh working VM → kill netd → new netd → new connection | **Fail** | **Fail** |

The final restart experiment first boots a fresh working VM and verifies TCP,
so the netd-restart result cannot be attributed to the preceding failed restore.
The earlier fixed-sleep run did not isolate those scenarios; use the two final
reports for the independent restart finding.

Static inspection corroborates the failures:

- `libkrucible/src/devices/src/virtio/persist.rs`: DeviceSnapshot and
  snapshot_device cover console, vsock, RNG, and block, but omit virtio-net.
  A successful snapshot therefore does not establish that network queues and
  negotiated state can be restored.
- `libkrucible/src/devices/src/virtio/net/worker.rs`: backend hangup explicitly
  logs that networking is disabled. It does not reconnect a UnixstreamPath.

These are shared VMM integration gaps, not reasons to switch smoltcp to gVisor.
A Go sidecar still needs the same recovery fixes on this VMM path.

Next work: add virtio-net quiescing/persistence/restore with its queue state,
and a defined backend-disconnect/reconnect lifecycle (or an explicitly stable
transport broker). Rerun fresh TCP and DNS after cold restore and netd restart.
Existing TCP sessions may break, per the agreed product scope; recovery of new
connections must not be waived. No fork changes are included in this spike.
