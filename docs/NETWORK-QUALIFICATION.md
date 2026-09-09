# Phase 5 bounded networking qualification

This qualifies the current Rust TCP/DNS gateway, authenticated previews and exact
private TCP grants on `agent_house` with **two 1-vCPU, 4 GiB guests**.
At most two sandboxes are alive: restore-as-new first destroys the peer, and
KVM tests run serially.
It changes no host firewall, resolver, production daemon or production state.
Upload optimization remains deferred. General UDP and IPv6 remain unsupported.

## Findings fixed

- **VMM transport stall:** two preview clients that stopped reading a large
  response caused unrelated preview handshakes to return 502/EAGAIN. The VMM's
  Unix vsock proxy performed blocking sends on the shared transport thread.
  Fork commit `39626f9` uses nonblocking writes, a 256 KiB pending buffer per
  Unix connection and delivery-based credits. Partial writes retain their suffix;
  guest half-close drains pending bytes before host EOF. Writable interest survives
  guest credit waits and event-thread polling changes are serialized with proxy
  state. The same slow-reader gate now passes, including an intact 32 MiB response.
- **TCP churn exhaustion:** short completed connections filled all 64 gateway
  slots with TIME_WAIT state, refusing new connections at only 8 concurrent
  clients. Under pressure, the oldest TIME_WAIT entry can now be reclaimed;
  established and half-closed flows are never evicted. Final ACKs are queued
  before reclamation on the ordered Unix guest link. This deliberately shortens
  TIME_WAIT retention under pressure; it does not increase the 64-flow bound.
- **Offload mismatch:** the Rust VMM advertised checksum/GSO support although its
  Unix wire protocol carries only Ethernet and strips the required metadata.
  It now advertises no checksum/segmentation offloads. Netd verifies IPv4 and
  TCP/UDP checksums, source identity and supported sizes/protocols **before** host
  socket allocation. IPv4 UDP's standard zero-checksum form remains accepted.
  Live `ethtool` confirms guest TX checksumming, scatter/gather and TSO are off.
  Guest software GRO/RX reporting is not an advertised host TX-offload contract.
- **DNS malformed compression:** coverage-guided fuzzing found header references
  and labels overlapping their own compression pointer. Both are rejected with
  deterministic regressions. Name expansion and traversal remain bounded.
- **Oversized DNS replies:** responses over 1232 bytes now become small TC replies
  preserving the question bytes/case and clearing answer/authority/additional
  counts. Clients retry over TCP. A controlled KVM resolver sends an oversized
  UDP answer without TC and verifies that the guest obtains the answer over TCP.
- **Preview browser boundaries:** bootstrap paths containing backslashes cannot
  become browser-normalized external redirects. HTTPS credentials now use a
  `__Host-ahvm_preview` cookie, preventing sibling Domain-cookie injection for
  that credential. Both old and new credential names are stripped from guest
  requests and blocked in guest Set-Cookie responses. Localhost retains the
  development `ahvm_preview` cookie.

## Evidence and limits

The gate passed in **80.86 seconds**, testing production code `d88b182` with
fork `39626f9`:

- Each guest touched and verified **3 GiB RAM**, held simultaneously for about
  five seconds. Sampled VMM peak RSS was about 3.2 GiB each.
- **2,269 preview requests** at 2/8/16 concurrent clients, 208 outbound requests,
  two stalled readers, and a byte-verified **32 MiB response** passed.
- Three gateway kills recovered in **1.27 / 2.21 / 4.30 seconds**, with the peer
  gateway stable. Isolation, preview auth/revocation, stop/start, snapshot/restore
  as new, and daemon adoption passed.
- Forge threads stayed at **7**; busy-VMM descriptors settled from **72 to 73**.
  The transient descriptor peak was **1,023** due to five-second deferred proxy
  cleanup; packaging must allow for that peak.
- Preview p95 latency was **4.87 / 92.69 / 224.19 ms** at 2/8/16 clients.
- Snapshots used disk-backed `/var/tmp`. Swap use was zero after the run, and all
  test processes and disposable files were cleaned up.

The memory allocations were released before networking load. This is a bounded
smoke test, not sustained combined memory/network stress. The Python fixture
uses one vCPU and 25 ms pauses between requests; these latency observations are
not a production throughput benchmark or evidence that upload optimization is
resolved. Resource sampling ran every 200 ms, so peaks are observations rather
than hard limits.

### Other validation

The regular KVM network gate passes in 17.37 seconds and HTTP acceptance in
31.04 seconds. The new oversized-DNS gate passes separately. Tests verify the
actual pinned fork, rootfs and Linux binaries; ordinary Cargo runs skip KVM.
The fork's 66 device tests and targeted Clippy pass. Its wider TEE feature checks
still expose existing RNG imports in `virtio/persist.rs`; GPU/input Clippy also
finds an existing redundant formatting borrow in `virtio/input/worker.rs`. These
files are unchanged by the backpressure fix.

Fuzz targets exercise the actual DNS parser and pre-admission Ethernet parser
with libFuzzer/AddressSanitizer, one worker per target, a 512 MiB RSS limit and
60-second runs (13,498,186 DNS and 64,535,300 ingress inputs in the final runs).
They are bounded fuzz runs, not exhaustive proof. Smoltcp bypasses
checksum verification under `cfg(fuzzing)`; the ordinary unit and live protocol
tests explicitly cover corrupted checksums before host dials. Stateful transport
coverage comes from the live gates and socket backpressure regression.

The focused review covers packet admission, resolver reply matching, private
endpoint ownership, preview token scope/stripping, browser bootstrap, resource
bounds, revocation, and recovery. It is not an independent security audit of KVM,
the guest kernel, TLS termination, or every VMM device. Preview app cookies still
follow browser Domain-cookie rules: guest apps should use host-only `__Host-`
authentication cookies, and production preview hosting needs a dedicated domain
and HTTPS. Bearer/API credentials must never be issued to guest apps.

## Upgrade boundary

Use matched daemon/netd/VMM artifacts and the pinned fork. Device-layout version
is now **3**; version-2 snapshot bundles are explicitly incompatible, including
old non-networked bundles. Old live networked workers are refused on adoption
using the persisted Ethernet contract, even if their old gateway has died.
Export required files with the old deployment and recreate sandboxes with the
new artifacts. This qualification used fresh disposable state and did not
perform a production upgrade. Existing HTTPS browser grants need their access
link reopened to establish the new cookie name.

Merge the fork backpressure PR first, then the agent-house qualification PR.
Phase 6 can then package the tested artifacts, TLS/listener configuration,
service limits, CLI flows and a fresh-install acceptance run. Upload performance
work can proceed separately after this Phase 5 slice.

## Reproduce within the resource budget

Use the same environment as `scripts/test-network-access.py`, a fresh directory,
and a guest with forge, Python, curl, ip and `/usr/sbin/ethtool`:

```sh
AHVM_NETWORK_QUALIFY=1 python3 scripts/test-network-access.py
python3 scripts/test-netd-protocol.py /absolute/path/to/ahvm-netd
```

The checked-in access gate defaults to 256 MiB per guest. The documented run
used a disposable copy with `memory_mb` set to 4096 for both guests and a brief
parallel Python allocation check (3 GiB per guest, touching and verifying every
4 KiB page, held for five seconds). To repeat it, make those test-only changes
and use a fresh disk-backed `AHVM_ACCESS_TEST_DIR` with sufficient host RAM for
two 4 GiB guests plus overhead.

Run `kvm_network` and daemon acceptance **one after another** with
`--test-threads=1`; running both binaries concurrently would exceed two guests.
The protocol script uses no VMs and only loopback sockets.

Fuzz setup and bounded commands are in
[the fuzz harness](../rust/ahvm-netd/fuzz/README.md).
