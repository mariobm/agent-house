# Managed agent event delivery

The node can capture an agent controller's durable projection journal and push
ordered events to Cloud. Cloud stores and streams these events to clients. A
client connection never owns guest work, and the guest receives no Cloud callback
credential. This optional protocol supports both Pi Durable and OpenCode.

## Enable the private host channel

Configure `AHVM_AGENT_EVENT_ORIGIN` in the **host daemon's** service environment,
for example `https://cloud.example`. It must be an HTTPS origin with no path,
query, fragment, or embedded credentials. The daemon advertises
`managed-agent-events-v1` in `/v1/healthz` only when that setting is valid. There
is no default destination. Existing managed jobs keep their original immutable
requests and continue using their existing observation protocol.

The Cloud dispatcher checks that capability before adding `event_delivery` to a
new `POST /v1/admin/runs/{id}` request:

```json
{
  "sandbox_id": "agent-vm",
  "argv": [
    "/usr/local/bin/ahvm-dev", "/usr/bin/env", "node",
    "/home/ahvm/.local/state/ahvm-agent/runs/RUN_ID/controller.mjs",
    "/home/ahvm/.local/state/ahvm-agent/runs/RUN_ID/input.json"
  ],
  "max_runtime_secs": 1800,
  "fence_on_failure": false,
  "session_isolated": true,
  "event_delivery": {
    "url": "https://cloud.example/internal/agent-events/RUN_ID",
    "token": "PER_RUN_BASE64URL_GRANT_AT_LEAST_32_CHARS",
    "request_hash": "64_LOWERCASE_HEX_CHARACTERS"
  }
}
```

Replace `RUN_ID` consistently with the immutable run ID. The callback's origin
must exactly match the configured host origin, and its path must be exactly
`/internal/agent-events/{id}`. The node rejects alternate ports, URLs with auth,
queries, or fragments, noncanonical paths, and other controller commands. The
token is 32–256 base64url characters. Event-enabled jobs must use the isolated
session contract and cannot fence a shared VM.

Delivery configuration belongs only to the private admission request and host
database. It is excluded from every API/wire receipt's `request_json`, from
debug logs, and from the command and guest input. The sender uses it only in the
HTTPS Authorization header. It disables redirects and proxies and bounds each
request to ten seconds, including a JSON response body of at most 4096 bytes.

## Guest journal and compact patches

The controller appends and fsyncs records to the fixed private path
`/home/ahvm/.local/state/ahvm-agent/runs/{id}/events.ndjson` before publishing its
atomic `result.json` projection. Each UTF-8 record includes a newline and is at
most 262144 bytes. The entire guest journal is bounded to 16 MiB. The first
record is:

```json
{"schema":1,"seq":1,"checkpoint":{"schema":1,"runId":"RUN_ID","requestHash":"HASH","sessionId":"ses_ID","messageId":"msg_ID","bootId":"BOOT_ID","phase":"running","text":"","tools":[]}}
```

Later records carry a full checkpoint or compact patches:

```json
{"schema":1,"seq":2,"patch":[{"op":"text","path":["text"],"prefix":0,"append":"Hello 🌍"},{"op":"set","path":["tools","0","status"],"value":"completed"},{"op":"remove","path":["question"]}]}
```

This example assumes the referenced tool and question existed in the previous
checkpoint. `seq` starts at one and increases without gaps. Each patch has at
most 256 operations and paths of at most eight segments. Supported operations
are `set` (replace a field/array element), `remove` (remove an existing object
field), and `text` (retain a UTF-8 byte prefix and append a string). The prefix
must end at a character boundary; it is not a JavaScript UTF-16 index. Array
length/order changes replace the array, and array elements cannot be removed by
index. Null and an absent field remain distinct.

Only these projection roots can change: `text`, `tools`, `question`, `phase`,
`detail`, `githubPublish`, and `admissionError`. Prototype-related keys, excessive
depth, and changed run/request/session/message/boot/harness identities are
rejected. Reconstructed projections keep the existing bounds: 32768 text bytes,
32 visible tools, 16384 question bytes, and at most 262144 total JSON bytes.
Callback and provider authentication fields, reasoning, and raw provider
responses/errors are excluded from this protocol. Tool output follows the
existing bounded projection policy and can include user-provided content.
Tool/question/phase transitions bypass text coalescing; text updates
may coalesce at 250–500 milliseconds.

## Durable host outbox and acknowledgements

The host reads the journal every 250 milliseconds while work is active. Each
captured record commits its byte cursor, guest sequence, full reconstructed
checkpoint and ordered event in **one SQLite transaction**. A database failure
or a full outbox leaves the cursor unchanged. Host event sequences are separate
from guest sequences and survive daemon restart.

The sender serially posts frozen event bytes:

```json
{
  "schema": 1,
  "run_id": "RUN_ID",
  "sandbox_id": "agent-vm",
  "request_hash": "HASH",
  "event_seq": 1,
  "kind": "checkpoint",
  "receipt": {"id":"RUN_ID","sandbox_id":"agent-vm","phase":"running","boot_id":"BOOT_ID","finished_at":null,"exit_code":null,"request_json":"CANONICAL_EXECUTION_REQUEST_WITHOUT_EVENT_DELIVERY"},
  "checkpoint": {"schema":1,"runId":"RUN_ID","requestHash":"HASH","sessionId":"ses_ID","messageId":"msg_ID","bootId":"BOOT_ID","phase":"running","text":"Hello 🌍"}
}
```

`receipt` also carries the ordinary managed receipt metadata, including its
node ownership epoch. Cloud authenticates the per-run grant, checks the immutable
placement/request/native/boot bindings, and commits its inbox/journal before
responding:

```json
{"schema":1,"run_id":"RUN_ID","event_seq":1}
```

Only an acknowledgement matching the exact first pending sequence releases its
payload and advances the host ACK cursor. Lost acknowledgements replay identical
bytes; out-of-order ACKs never skip events. Cloud deduplicates unchanged retries
and rejects changed payloads for the same sequence. An authenticated deleted
chat may instead acknowledge with `"retired":true`; HTTP 410 requires that
explicit field. Random 401/404/redirect/error responses never retire events.

## Completion, pressure and retention

A guest `succeeded` projection is still a preview. Only the existing managed
run protocol proves controller and tool termination. Before committing its
terminal receipt, the node drains the remaining guest journal. It commits a
final `kind:"receipt"` event with that receipt and the latest checkpoint in the
**same transaction**, before releasing the managed activity hold or guest
receipt pin. This removes dependence on Cloud's next reconciliation poll or a
late guest file read. Completion polling remains bounded by the existing
two-second managed controller interval.

Unreadable transport/database state retains the hold for retry. Missing or
corrupt guest proof, a partial final journal record, a changed boot, or a verified
VM stop produces a final host receipt without a checkpoint; Cloud must report an
interrupted result rather than infer chat success. These checks preserve shared
VM isolation and never replay a prompt or stop a sibling conversation.

Pending event payloads are bounded to 8 MiB per run and 128 MiB per host. Admission
reserves a maximum-size final event (300 KiB) for every open stream. Normal
checkpoints cannot consume that reservation. Exhaustion refuses new admission or
pauses collection without advancing its cursor; it never evicts an unacknowledged
event or silently loses a final receipt. The reservation protects final receipt
insertion **after** the guest backlog has been captured. If an unavailable Cloud
channel fills the outbox first, terminal capture retains the managed hold and
retries after acknowledgement releases space. This can defer VM idle release
after the controller process has exited; an offline phone alone does not cause
it because the host-to-Cloud channel remains independent of the client.
The guest's compact patches keep a long
growing answer proportional to its actual text instead of repeatedly copying
the full answer. Exhausting the separate guest journal limit is a visible
controller failure and requires existing safe cancellation/recovery handling.

Delivery failures back off from one to thirty seconds. Network retries end seven
days after the run deadline, matching Cloud grant retention; remaining payloads
stay on the host for operator recovery. They are not automatically discarded.
Terminal outbox delivery survives explicit VM deletion independently of VM/quota
records. Acknowledging the final event clears its private delivery config and
checkpoint. Deleted VM streams are removed only after final acknowledgement;
retained live-VM receipt identities preserve ordinary idempotency. As with managed
runs, callers must never reuse deleted run or VM IDs.

## Verification

Run `cargo test -p ahvm-store -p ahvm-daemon --lib` from `rust/`. Tests cover
database reopen, ownership fencing, frozen-byte retries, wrong/retired ACKs,
backpressure and final reservation, atomic receipt/outbox failure, VM deletion,
partial/missing/corrupt guest records, boot changes, and callback redaction. The
shared TypeScript-generated fixture exercises Unicode byte prefixes, tool-array
changes, null/removal and malicious patches in both runtimes.

The synthetic long-output test captures and acknowledges 6000 Unicode text
revisions with ten stable tool projections. Its compact guest journal is about
584 KiB, below the 16 MiB limit. This is a mock guest-RPC measurement; real guest
fsync latency, network latency, and Cloud ingestion require separate isolated
qualification.
