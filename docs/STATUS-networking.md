# Rust networking integration

The Phase 5 spike is merged. This slice integrates its smoltcp TCP/UDP-DNS
forwarder as `rust/ahvm-netd`, supervised by the Rust engine and enabled by the
Rust daemon. It is opt-in. The bounded Phase 5 qualification and its upgrade boundary are
recorded in [network qualification](NETWORK-QUALIFICATION.md).

## Enable on a Linux host

Build the Rust daemon, netd and VMM from the same checkout and pinned fork.
In addition to the existing daemon variables, set:

```sh
AHVM_NETD_BIN=/absolute/path/to/ahvm-netd
AHVM_DNS_RESOLVER=127.0.0.53
```

Use a resolver actually reachable on your host. `127.0.0.53` is the existing
systemd-resolved stub on agent_house, not a portable default. An external
resolver is also configurable. Both values are host configuration, never taken
from a sandbox API request. The guest image needs forge, `/bin/sh`, and `ip`.
The engine configures eth0, the default route and resolv.conf through forge
before reporting a successful boot. That setup also runs after cold restore.

Use fresh networked sandboxes. Enabling/disabling networking changes the virtual
device set, so existing snapshots from the other mode cannot be restored. A
live networked VM requires network configuration when the daemon is reopened;
changing the configured resolver requires stopping its VMs first.

## Isolation and lifecycle

Each sandbox has its own netd process, socket and connection table. The private
link uses guest 100.64.0.2 / MAC 02:00:00:00:00:02 and gateway 100.64.0.1. These
addresses deliberately repeat on separate links: they are not host-routable
sandbox endpoints. `SandboxInfo.ip` reports this internal guest address.
The host-owned Unix socket determines the sandbox identity; packet source MAC
and IP must match the configured link identity. Socket directories are mode
0700 and net sockets mode 0600.

TCP admission rejects private, special-use, metadata, shared-address and all
current host-interface IPv4 addresses unless an exact host-authorized TCP grant
matches the sandbox and endpoint. Host addresses are enumerated at each new
connection admission, not on every data packet. UDP is limited to DNS forwarding
through the configured resolver; replies must match the transaction ID, opcode,
and single question (case-insensitive name, type and class). Malformed or
mismatched replies are ignored within the original five-second query deadline.
The connected UDP socket also pins the resolver's source address and port.

TCP to the guest gateway on port 53 is forwarded only to that same configured
resolver, so guest resolvers can retry truncated UDP responses over TCP. This is
client-driven fallback: the gateway preserves the DNS truncation bit and proxies
TCP framing unchanged. It also permits direct TCP DNS queries. DNS TCP connections
share the existing 64-flow/dial limits and TCP timeouts. The exception allows no
other gateway port or arbitrary private destination. Other UDP and IPv6 are dropped. Private-access exceptions and authenticated previews are described in
[network access](NETWORK-ACCESS.md). There is no implicit inter-project routing.

The engine starts netd before the VMM, persists its PID/starttime separately in
`net-state.json`, and checks it every 250 ms. Linux /proc identity checks avoid
shell-process polling in the recurring monitor. Dead gateways reconnect through the repaired VMM socket backend. Repeated
failures use retry delays of 1, 2, 4, 8, 16, 32, then at most 60 seconds. The
backoff resets only after 30 seconds of observed worker uptime; socket creation
alone does not reset a crash loop. Recovery keeps retrying at the capped rate.
Supervisor logs identify the sandbox, failure, retry delay, and successful
restart; worker details remain in that sandbox's netd.log.
Another sandbox's running gateway is unaffected. Daemon restart adopts matching
identities; adopted signalling uses the existing pidfd path. Stop/destroy and
failed boots clean up associated gateways. Gateways with no connected VMM exit
after 30 seconds, including a supervisor crash before the first VMM starts.

A daemon object dropped inside a still-running process hands its owned network
children to background reapers, so subsequent adoption/termination cannot leave
those children as zombies. Startup also cleans verified VMM/netd orphans from
networked directories that never acquired a committed sandbox record.

Limits are 64 tracked TCP flows, 64 outstanding host dial threads (including
threads whose guest flow disappeared or whose Unix connection closed), and 64
concurrent DNS exchanges per gateway. Buffers, frame assembly and connect/idle
budgets remain bounded. Established TCP streams are not preserved through netd
loss or cold restore; applications must reconnect.

## Validation on agent_house

[Hardening results](results/networking-hardening-linux.json): UDP reply matching,
TCP DNS fallback and capped restart backoff pass the expanded two-test KVM gate
in 16.83 seconds. The final DNS isolation test with a positive host-listener
control passes in 0.83 seconds; network-enabled HTTP acceptance passes in 31.26
seconds. These are functional checks, not throughput benchmarks.

[Recorded results](results/networking-integration-linux.json): the dedicated
KVM gate passes in 10.48 seconds, and the network-enabled HTTP acceptance gate
in 31.16 seconds. A separate rebuilt-daemon binary smoke test also passes
create → guest HTTPS → destroy over localhost HTTP.

`rust/ahvm-engine/tests/kvm_network.rs` is the dedicated opt-in KVM gate. It
creates two disposable Alpine guests and verifies HTTPS/DNS, three gateway
SIGKILL/restarts without replacing the peer's gateway, a positive guest-local
listener inaccessible from the other guest, rejection of an actually listening
host service, snapshot restore-as-new, stop/start, VM SIGKILL recovery, daemon
object re-adoption, a further gateway restart after adoption, and destroy/cleanup.

```sh
AHVM_KVM_NETWORK_TEST=1 \
AHVM_NETWORK_TEST_DIR=/absolute/fresh/disposable/path \
AHVM_VMM_BIN=/absolute/path/to/ahvm-vmm \
AHVM_NETD_BIN=/absolute/path/to/ahvm-netd \
AHVM_GUEST_IMAGE=/absolute/path/to/alpine-forge.ext4 \
AHVM_DNS_RESOLVER=127.0.0.53 LD_LIBRARY_PATH=/usr/local/lib64 \
cargo test --manifest-path rust/Cargo.toml --locked -p ahvm-engine \
  --test kvm_network -- --nocapture
```

The same KVM test binary includes a controlled DNS fixture on a disposable Linux
loopback address, port 53 (requires permission to bind that port). It sends wrong
transaction-ID and wrong-question replies before a valid truncated reply. The
guest must ignore the wrong replies, retry via TCP, and receive the complete
answer. The test also checks that another gateway port remains unavailable.
No host resolver configuration is changed.

The HTTP acceptance gate also accepts AHVM_NETD_BIN and AHVM_DNS_RESOLVER, and
adds a guest HTTPS request through the real API before its existing lifecycle,
file rollback, session continuity, and adoption checks. It still requires
AHVM_KVM_TEST=1, AHVM_VMM_BIN, AHVM_GUEST_IMAGE and LD_LIBRARY_PATH.

Targeted local/server crate tests and Clippy use:

```sh
cargo test --manifest-path rust/Cargo.toml --locked \
  -p ahvm-netd -p ahvm-engine -p ahvm-daemon
cargo clippy --manifest-path rust/Cargo.toml --locked \
  -p ahvm-netd -p ahvm-engine -p ahvm-daemon --all-targets -- -D warnings
```

Plain cargo tests explicitly skip KVM gates; only the separately enabled live
runs count as KVM validation. GitHub-hosted CI is not relied on for these results.

Preview ports and exact owner-bound private TCP grants are implemented in the
[network access slice](NETWORK-ACCESS.md); broader project routing is not implicit.

## Qualification and next steps

The resource-bounded Phase 5 qualification, fixes, evidence and limits are in
[NETWORK-QUALIFICATION.md](NETWORK-QUALIFICATION.md). Run its KVM gates serially
to stay within two small guests. General UDP and IPv6 remain unsupported.
Upload optimization stays deferred. Phase 6 CLI and packaging implementation,
fresh-install verification and operational limits are documented in
[RUST-INSTALL.md](RUST-INSTALL.md).
