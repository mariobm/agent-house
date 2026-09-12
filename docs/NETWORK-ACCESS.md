# Preview ports and private TCP access

This adds opt-in access controls to the Rust daemon. Upload optimization remains
after Phase 5. Load/fuzz/offload qualification and the production security review
are still pending; this is not an untrusted-workload security sign-off.

## Previews

Build the daemon and guest image with the current Rust forge. Older forge images
do not implement the new tunnel request and cannot serve previews. Managed
networking is required: guest loopback must be the guest's own network stack.

Enable a separate listener, for example for local development:

```sh
AHVM_PREVIEW_LISTEN=127.0.0.1:8081
AHVM_PREVIEW_DOMAIN=preview.localhost
```

The control API continues on AHVM_LISTEN. Never reverse-proxy preview content onto
the control API's origin. Each sandbox/port has a distinct hostname. For a remote
installation, configure wildcard DNS and TLS for a dedicated preview domain and
forward it to the preview listener, preserving Host. Non-localhost domains use
Secure cookies and require browser-facing HTTPS. Preview domains are limited to
100 characters; localhost examples are for local/SSH-forwarded development.
There is no anonymous public-preview mode.

Using the owner's normal API Bearer token:

- `PUT /v1/sandboxes/{id}/previews/{port}` registers a TCP port (204).
- `GET /v1/sandboxes/{id}/previews` returns `port` and `host_label` entries.
- `POST /v1/sandboxes/{id}/previews/{port}/access` returns a one-hour preview-only
  `token`, `expires_at` and `host_label` for browser access. Issuing another token
  invalidates the old token for new requests.
- `DELETE /v1/sandboxes/{id}/previews/{port}` revokes the port (204).

Open `https://{host_label}.{preview_domain}/?ahvm_token={token}` (use http and the
listener port for local development). The listener validates the scoped token,
sets an HttpOnly cookie and redirects to the path without the token. Only token
hashes are stored. The cookie grants access to that one sandbox/port, never the
control API. New requests are rejected after expiry, rotation or revocation.
Bearer authentication also works directly on the preview listener for API tools.
Cross-origin cookie-authenticated scripted requests are rejected, including
requests from sibling preview origins. Bootstrap links are credentials: use them
only with the intended owner. TLS ingress should not log their query strings.

`host_label` is the hex-encoded sandbox id split into DNS-sized labels, followed
by `--{port}`. Use the returned value, rather than assembling it in clients.
This preserves arbitrary valid sandbox ids and gives each port its own origin.
Paths and queries are forwarded unchanged, so root-relative assets work.

The guest service must listen on IPv4 loopback or all guest interfaces. Ordinary
HTTP request/response bodies stream with bounded buffering; WebSocket upgrades
are relayed. API Authorization, the preview cookie, proxy credentials and
hop-by-hop headers are stripped before guest delivery. This is an HTTP/1 preview
proxy, not an arbitrary CONNECT tunnel. Response trailers are not forwarded.

Limits are 16 registered ports per sandbox, 64 concurrent forwarded connections
per daemon, and 64 tunnels per forge. Upstream response headers have a 30-second
budget. A connection lasts at most five minutes and the guest tunnel closes
after 30 seconds without traffic (clients should reconnect/heartbeat). Streams
hold thermal activity while live; the budget also fires when a slow client stops
reading. Port revocation cancels established forwarding within the next one-second
check; already delivered/buffered bytes cannot be recalled.

Registrations survive stop/start and daemon restart. Stopped services return an
error until explicitly started; preview requests do not implicitly boot VMs.
Destroy removes registrations via the sandbox foreign key. Restore-as-new starts
without registrations or browser tokens. In-flight streams disconnect on worker
loss/stop and clients reconnect afterwards.

## Explicit private access

Private destinations are still denied by default. Set AHVM_PRIVATE_ACCESS_FILE
to a host-owned JSON policy file with exact sandbox ids and TCP endpoints:

```json
{
  "project-dev": {
    "owner_user_id": "alice",
    "destinations": ["10.20.0.15:5432", "192.168.50.8:443"]
  }
}
```

The daemon reserves each named id for the specified owner: another user cannot
claim it by creating or restoring a sandbox, including after deletion. On daemon
startup, any existing row must have the configured owner. The engine receives
only the endpoints for that id. Applications/guest requests cannot edit policy.
When embedding the daemon library, configure AppState.private_owners consistently
with the engine policy; the binary does this from the same file.

Rules match the actual destination IPv4 address AND port; there are no hostnames,
CIDRs, wildcard ports or implicit project-wide grants. Up to 64 endpoints per id
and 4096 configured ids are accepted. Public host-interface addresses may be
explicitly granted too. Link-local/metadata, shared guest addresses, unspecified,
multicast and reserved high-address destinations remain forbidden even in rules.
Loopback grants require guest routing that reaches the gateway; normally guest
127.0.0.1 stays inside the guest, so use a reachable host/interface address.
No new host firewall rules, routes or public listeners are created by grants.

The policy is read at daemon startup. To change a live sandbox's policy: stop the
sandbox with the existing daemon, edit the host policy, restart the daemon, then
start the sandbox. Adoption refuses a different saved policy instead of silently
retaining old access. A grant remains reserved to its owner until removed from the
host policy; deleting a sandbox does not grant its name to another owner.
Restore-as-new/fork does not copy grants: the new id needs its own explicit rule.

## Verification

`scripts/test-network-access.py` runs the complete gate through a freshly built
Rust daemon on Linux/KVM. It creates disposable guests, a positive host listener,
a guest HTTP/WebSocket service and a temporary policy. Required environment:
AHVM_DAEMON_BIN, AHVM_VMM_BIN, AHVM_NETD_BIN, AHVM_BASE_IMAGE, AHVM_LIB,
AHVM_DNS_RESOLVER, and a fresh AHVM_ACCESS_TEST_DIR. The guest image needs current
Rust forge, Python 3, curl and ip. It tests exact-port/private isolation, previews,
browser credentials, streaming, WebSocket revocation, stop/start, restore-as-new,
adoption, policy mismatch refusal and cleanup. It changes no host resolver or
firewall configuration. Treat plain cargo tests as unit/API checks, not KVM proof.

[Recorded Linux results](results/network-access-linux.json): the access gate passes
in 7.00 seconds, the existing KVM network gate in 17.20 seconds and network-enabled
HTTP acceptance in 31.39 seconds. Local and server crate tests, Clippy and formatting
pass. These are functional checks, not throughput benchmarks.

The follow-up [qualification](NETWORK-QUALIFICATION.md) caps the gate at two guests,
adds load/slow-reader checks, and hardens HTTPS credentials to use the
`__Host-ahvm_preview` cookie. Old HTTPS grants need their access link reopened.
The `ahvm_preview` development cookie is used only for localhost domains.

## Optional bandwidth cap

Set `AHVM_NETWORK_BYTES_PER_SEC` on the daemon to cap each managed VM's virtual
Ethernet link in each direction. For example, `10485760` permits 10 MiB/s per
direction. Valid values are 65536 through 1000000000 bytes/second; leaving it
unset preserves unlimited self-hosted networking. Stop VMs before changing this
policy: adopting a live gateway with a different saved limit is refused.

The gateway uses independent ingress/egress token buckets with 100 ms of burst
credit. Ethernet traffic, DNS and the link's four-byte frame headers all count.
The limit aggregates all connections on that VM, while other VMs have their own
buckets. It applies backpressure rather than dropping throttled TCP data. A
gateway restart starts a fresh burst allowance. Existing bounded host/socket
buffers can temporarily burst independently of the virtual link. This is a rate
limit, not monthly transfer accounting or a host-wide egress billing guarantee.

REST file transfers, terminal/desktop streams and preview proxies do not traverse
this link and need separate API transport controls. Do not describe this setting
as covering those paths.

Linux qualification: `AHVM_KVM_BANDWIDTH_TEST=1` selects the `per_vm_bandwidth`
test in `ahvm-engine/tests/kvm_network.rs`. Supply the same VMM, image, netd,
resolver, library and fresh test-directory variables as the network gate. It
creates two disposable 1-vCPU/1-GiB guests and grants only a temporary exact host
listener endpoint. Both transfer 2 MiB in each direction at 256 KiB/s concurrently,
verify payload integrity and check peer exec responsiveness. No external bulk
traffic is generated.
