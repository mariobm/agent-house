# Rust networking integration

The Phase 5 spike is merged. This slice integrates its smoltcp TCP/UDP-DNS
forwarder as `rust/ahvm-netd`, supervised by the Rust engine and enabled by the
Rust daemon. It is opt-in; it is not the Phase 5 security/production exit.

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
current host-interface IPv4 addresses. Host addresses are enumerated at each new
connection admission, not on every data packet. UDP is limited to DNS forwarding
through the configured resolver; other UDP and IPv6 are dropped. There is no
private-access exception or inter-project routing API in this slice.

The engine starts netd before the VMM, persists its PID/starttime separately in
`net-state.json`, and checks it every 250 ms. Linux /proc identity checks avoid
shell-process polling in the recurring monitor. Dead gateways restart with a
one-second retry backoff and reconnect through the repaired VMM socket backend.
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

## Remaining Phase 5 work

- Deliberately exposed preview ports and explicit project/private access policy.
- DNS-over-TCP fallback and a decision on broader UDP/IPv6 support.
- Complete checksum/segmentation-offload qualification. This integration retains
  the spike's receive-checksum handling for the stripped virtio stream; the live
  TCP test is not proof of every offload combination.
- Sustained multi-guest load, coverage-guided fuzzing and focused security review
  before enabling networking for untrusted guests. The bounded-connect fix is
  useful hardening, not a completed adversarial-traffic qualification.
- Upload optimization remains deferred. This slice does not claim new throughput
  parity or ship CLI/systemd packaging and cutover.
