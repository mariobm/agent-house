# Durable node lifecycle operations

Controllers that can lose HTTP responses should use the authenticated
`lifecycle-operations-v1` protocol advertised by `/v1/healthz`.

```http
POST /v1/operations/6c62f9ee-15d1-4c81-987c-58eab3d640b6
Authorization: Bearer <node token>
Content-Type: application/json

{"action":"create","sandbox_id":"dev","cpus":2,"memory_mb":4096}
```

`start`, `stop` and `delete` use the same body with only `action` and
`sandbox_id`. IDs are 1–64 ASCII letters, digits, underscores or hyphens.
The create form currently supports the default non-desktop image.

The node persists an owner-bound receipt before executing. Reusing an ID with
another request conflicts; repeating the same request reads its receipt, including
after sandbox deletion. Only one pending journal operation may own a sandbox.
Legacy create/start/stop/delete and idle sweeps respect that reservation.

The executing task survives a disconnected HTTP request. Fast operations return
HTTP 200 with a receipt; after 20 seconds the handler returns HTTP 202 and the task
continues. Snapshot serialization has its own 300-second budget, separate from
five-second PAUSE/RESUME/STATUS calls. Read `GET /v1/operations/<id>` to check it:

```json
{"id":"6c62f9ee-15d1-4c81-987c-58eab3d640b6","sandbox_id":"dev","state":"done","status":201,"sandbox_state":"running"}
```

- `pending`: still executing; no terminal outcome or reliable absence claim.
- `done`: the handler finished; `status` records its HTTP result.
- `interrupted`: the previous daemon died before recording its result. The old
  request will never be executed again under this key. Inspect `sandbox_state`
  before deciding whether the desired state was achieved or a new request is needed.

For terminal receipts, `sandbox_state` reflects the backend and metadata at read
time: `running`, `stopped`, `failed`, `absent`, or `untracked`. Only `absent`
confirms that both backend and metadata are missing. `untracked` requires operator
recovery; never release a cloud reservation just because the metadata row is gone.
An unhealthy control channel can make receipt reads fail temporarily.

After restart, verified surviving non-desktop workers are queried over their
control socket. Paused guests are resumed before being reported Running. A failed
probe leaves the VM Failed; `start` retries control recovery without creating a
second worker. Desktop workers have no snapshot control socket and skip this step.

Keep `daemon.db` and its receipts with the sandbox state during backups/upgrades.
Do not delete receipts to retry a request: retained keys prevent delayed requests
from resurrecting deleted VMs. Run one daemon against a state directory. This is
not automatic replay of interrupted work or a transaction spanning guest disk
writes; exec and file operations do not use this lifecycle protocol.
