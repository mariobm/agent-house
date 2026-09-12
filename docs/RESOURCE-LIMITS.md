# Optional hosted resource limits

Self-hosted behavior is unchanged unless these variables are configured.

## API payload bandwidth

`AHVM_API_BYTES_PER_SEC` sets a per-VM, per-direction payload budget from 65536 to
1000000000 bytes/s. HTTP file uploads/downloads, legacy JSON files, exec output,
session REST calls and session WebSocket payloads share it. JSON/base64 overhead
counts. A 64-KiB burst keeps small requests responsive; parallel calls and
reconnections do not reset credit. Different VMs and directions are independent.

Enabled pacing also admits at most four retained HTTP transfers per VM and 32
across the daemon. Existing WebSocket/backend stream limits remain separate.
Unauthenticated or foreign-VM requests cannot allocate limiter entries. Control
and lifecycle routes are not paced. Desktop and preview transports are not yet
covered; a hosted service must keep them unavailable until separately bounded.

This is separate from `AHVM_NETWORK_BYTES_PER_SEC` and the trusted per-VM Ethernet
override. Unlimited Ethernet does not disable an API cap. Neither is a monthly
traffic allowance. The API setting is host configuration, not a public client
request field.

## Storage quota broker

`AHVM_STORAGE_SOCKET` opts the krucible backend into a host-owned Unix-socket
quota broker. The daemon remains unprivileged. Each newline-delimited request is
at most a few hundred bytes:

```json
{"action":"prepare","id":"example"}
```

Actions are `prepare` (create a bounded tree before any worker writes), `verify`
(adoption/start), and `release` (remove only an empty project root and release its
reservation after open/unlinked allocations disappear). A successful reply is:

```json
{"ok":true,"path":"/canonical/backend/sandboxes/example"}
```

The daemon checks the canonical parent and exact id. A missing broker, malformed
reply or mismatched path refuses the operation. A broker must authenticate the
local daemon uid, own the parent directory, enforce both byte/inode quotas and
aggregate reservations, serialize its state, and never accept arbitrary paths or
commands. Recursive cleanup runs with the ordinary daemon uid. Failed deletion
remains retryable, including after directory removal but before slot release.

All stop/start recovery generations live within the same VM tree. Named
snapshots/forks are explicitly unavailable in this opt-in mode until a separate
storage-reservation design covers the shared snapshot registry. Existing named
registries refuse quota-mode startup. Do not use a virtual image size alone as
an aggregate host storage bound.
