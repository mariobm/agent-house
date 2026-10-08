# Linux worker isolation: measured overhead

Date: 2026-10-08. These measurements compare released v0.3.16
(`6e06cebcd437e6476301d5bf402d7967a358c047`) with the broker-based isolation
implementation in this PR. libkrucible is unchanged at
`79cdb626fff10c16731c6f4bec690dacd7feec59`.

Two complete runs of each configuration on the same Linux x86_64 KVM host,
kernel 7.0.0-22, systemd 259, ext4, with the same cached Ubuntu image. Each
guest has one vCPU and 512 MiB RAM. Separate disposable systemd services use
an unprivileged test account, per-VM cgroups and managed networking. The new
configuration additionally uses the packaged broker's capability restrictions,
private mounts and distinct worker identities. No production service is changed.

| Operation | v0.3.16, two runs | Isolated, two runs |
|---|---:|---:|
| Create and boot | 336–349 ms | 421–438 ms |
| First guest exec | 14–15 ms | 14–15 ms |
| Stop, local RAM/disk snapshot | 659–721 ms | 732–739 ms |
| Start from local snapshot | 424–441 ms | 524–556 ms |
| Delete | 98–111 ms | 113–122 ms |

Guest work below is the median of five samples **within each run**. CPU work
hashes the same 1 MiB memory buffer 512 times. Disk work writes 64 MiB and
fsyncs it, then reads and hashes the file. Networking downloads 8 MiB from a
host HTTP server through an exact private-address grant.

| Guest workload | Baseline run medians | Isolated run medians |
|---|---:|---:|
| CPU SHA-256 | 242–245 ms | 244–245 ms |
| 64 MiB write + fsync | 61–64 ms | 65–69 ms |
| 64 MiB read + SHA-256 | 42–47 ms | 44–49 ms |
| Managed TCP, 8 MiB | 22.9–23.3 ms | 24.2–25.3 ms |

The small sample supports approximately 0.1 second additional launch/resume
cost and similar CPU throughput. Filesystem/network differences are small in
absolute terms but visible; this is not evidence of zero overhead. There was
other host activity, and this is not a statistical capacity or tail-latency
qualification. The broker serializes launches and fsyncs its identity registry;
large retained registries and simultaneous creation need separate measurement.

These are **direct host API, local-storage** tests. They do not measure Cloud
API latency, R2 hydration, replicated cold boot or desktop startup. Each run
also verifies adoption of the same live PID after daemon restart, snapshot
resume with a matching file hash, and successful deletion. Raw timings are in
the adjacent JSON files. The benchmark driver used for this run is retained
outside the repository at `/tmp/ahvm-worker-isolation-review/perf.py` on the
review machine; it creates fresh test services and never invokes the Cloud API.

The independent installed CLI gate (`scripts/test-rust-install.py`) passed in
18.16 seconds with at most two 256 MiB guests: file transfers and interrupted
replacement, PTY input/resize/detach/reattach, private-access grants, preview
revocation, snapshots, live worker adoption and SIGKILL recovery. An attached
shell remains alive past the idle deadline; detaching permits automatic stop.

`adversarial.json` records the actual broker-launched probe results from
`qualify-broker.py`: separate identities, peer filesystem/socket denial,
sealed configuration, retained-directory-FD isolation, forged-record rejection
exact role descendant cleanup, identity exhaustion before spawning, and offline
compaction that preserves the allocation counter and existing VM records. It is a focused regression gate, not a
claim that every hostile-tenant escape route has been eliminated.

`gpu.json` is a separate functional check using the existing Omarchy image,
2 vCPUs and 8 GiB RAM, with a freshly built GPU worker and the isolated broker.
Hyprland and WayVNC started, an actual 1280×720 framebuffer was captured and
inspected, and a command was typed and executed through VNC. Daemon restart
adopted the same live worker; local disk stop/cold-start preserved a marker.
Create took 7.63 seconds, stop 0.20 seconds and cold-start 6.26 seconds. These
are single local-storage timings without a matched desktop baseline; they do
not measure isolation overhead or replicated desktop recovery.

`replicated.json` records a native NBD/R2 recovery gate with a separate 2 GiB
filesystem and 128 MiB minimal guest image. It verified actual VMM jail device
inodes and root-owned PID/role/device records, local journal eviction, removal
of the cached base, daemon and volume-service restart, and matching guest data
after remote hydration. Removing one backed-up remote object returned `EIO`
for the known block; restoring it recovered the original data. Initial create
was 6.08 seconds and recovery start 5.14 seconds in this small fixture. These
are functional timings, not representative Ubuntu Cloud cold-wake results.
All 268 objects under the fresh qualification prefix were removed, NBD3 was
detached, and the temporary bucket-scoped credential was revoked. The JSON's
`source_head` is the base commit; it was run with this PR's working-tree changes
and records hashes of the binaries used.
