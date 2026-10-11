# Project environment hooks: guest and node contract

Project controllers run optional `.ahvm/install.sh` and `.ahvm/start.sh` before
loading a provider or launching Pi or OpenCode. Installation can provision any
project dependency or toolchain. A dedicated clean Ubuntu build VM can validate
these hooks without creating a chat, injecting provider credentials, or making a
model request. These hooks alone do not publish a reusable prepared image.

## Guest helper deployment

The reviewed source is `images/ubuntu-dev/project-environment/helper.py`. Future
Ubuntu builds install it at `/opt/ahvm-project-environment/helper.py`, with an
`ahvm-project-environment` command wrapper. Existing VM disks are not upgraded.
A trusted Cloud/node controller may stage the exact source at
`/opt/ahvm-project-environment/<SOURCE_SHA256>.py` through authenticated Forge:

1. As guest root, create the new directory using
   `install -d -o root -g root -m 0755 /opt/ahvm-project-environment`.
2. Upload the fixed reviewed source, then set root ownership and mode 0644 on
   that exact file. Forge uploads inherit the guest's group-writable umask, so
   the explicit permission step is required.
3. Invoke `sudo -n /usr/bin/python3 -I PATH probe` as `ahvm`. Verify the protocol
   is `project-environment-v1` and `helper_sha256` matches the reviewed source.

The helper rejects writable, symlinked, or non-root-owned source/ancestor paths.
Use the new `/opt` directory: the published image's `/usr/local/lib` can contain
ownership inherited from the upstream Node archive. Do not change existing
library ownership to satisfy this helper. `probe` checks cgroup v2 with
`cgroup.kill`, creates protected scopes, and mounts a private 16-MiB per-boot
state tmpfs. It does not execute a project script. Python isolated mode prevents
ambient import path configuration from influencing the root helper.

## Commands and receipts

`RUN32` is a UUID with hyphens removed. `WORKSPACE64` is SHA-256 of the exact
canonical absolute checkout path, and `REVISION64` binds the original source,
base image and both hook digests. Cloud freezes this setup context for a chat:
ordinary agent commits with unchanged hooks do not change it. Changed hooks or
base inputs create a new revision. Each attempt gets a fresh run ID.

```sh
sudo -n python3 -I HELPER install RUN32 WORKSPACE64 REVISION64 \
  --cwd /workspace/chats/CHAT_ID --hook-sha256 SHA64_OR_none --timeout-secs 900
sudo -n python3 -I HELPER start RUN32 WORKSPACE64 REVISION64 \
  --cwd /workspace/chats/CHAT_ID --hook-sha256 SHA64_OR_none --timeout-secs 120
sudo -n python3 -I HELPER result RUN32 WORKSPACE64 REVISION64 \
  --cwd /workspace/chats/CHAT_ID --hook install
sudo -n python3 -I HELPER status RUN32 WORKSPACE64 REVISION64 \
  --cwd /workspace/chats/CHAT_ID --hook install
sudo -n python3 -I HELPER abort RUN32 WORKSPACE64 REVISION64 \
  --cwd /workspace/chats/CHAT_ID
sudo -n python3 -I HELPER replace RUN32 WORKSPACE64 NEW_REVISION64 \
  --cwd /workspace/chats/CHAT_ID --previous-revision OLD_REVISION64
```

Scripts execute using Bash as guest `ahvm`, with the repository as working
directory. They need no executable bit. Checkout path components, `.ahvm` and
script files must be real directories/regular files; symlinks are rejected.
Missing scripts succeed as no-ops. Script contents must match the requested
SHA-256 before launch. Script environment contains only the developer identity,
standard PATH/LANG and the explicit `AHVM_PROJECT_*`/`AHVM_WORKSPACE_PATH`
bindings. Controllers must revoke/remove clone authentication before invoking
hooks and must not inject provider or Cloud callback credentials into them.

Installation holds a VM-wide, cancel-aware lock to avoid overlapping package
manager setup in quota-reused VMs. `status` reports `waiting_install_lock` while
queued and `running` during execution. Installation joins all descendants on
completion. Root-owned successful installation receipts persist under
`/var/lib/ahvm-project-environment` and are reused only for a matching checkout,
revision and install script SHA. A fresh checkout has a different identity and
runs installation again; absolute virtual environments are never copied between
checkout paths. Unknown prior work is not replayed automatically.

Start scripts must return after launching services and checking any readiness
that the project needs. For example, launch `npm run dev &`, wait for its health
endpoint, and return. Start success retains the checkout's separate service
cgroup. A successful script receipt proves script completion; it does not prove
that the service will remain healthy. Future service stdout/stderr is drained
without holding the controller's pipes or accumulating a helper log file;
projects can redirect service logs explicitly. Startup output is limited to
1 MiB plus a final 16-KiB tail and reports `output_truncated`.

Start success is reused for that checkout/revision in the same guest boot.
After a genuine cold boot, installation stays prepared and start executes again.
Changed hooks call `replace` before installation: the helper validates the
previous revision, joins only that checkout's old service scope, and removes its
ready marker. An already-replaced or already-new revision is an idempotent no-op;
a different third revision conflicts. Failed replacement setup can retry with a
new run ID. Sibling checkout scopes are preserved.

Hook timeouts are finite, 1–1800 seconds, defaulting to installation 900 and start
120. Their queue wait counts toward the hook deadline. Failure, timeout or
cancellation kills and joins that attempt's descendants. The helper also watches
the trusted node cancellation marker `/run/ahvm-managed-cancel/<UUID>` directly,
so losing the managed controller does not strand an active hook. `abort` seals
new admission before joining. It does not kill reused services owned by an older
successful setup attempt. A later model turn cancellation may intentionally keep
an already-ready workspace's services available.

Hook stdout/stderr is script data. The separately published root receipt is JSON
with schema 1, hook/run/workspace/revision/path/boot bindings, `hook_present`,
`hook_sha256`, `exit_code`, `empty`, `services_running`, `owner_run_id`, `reused`,
`cancelled`, `timed_out` and `output_truncated`. Query it independently; a script
can legitimately exit 75, which is also the helper's own error code. Before
reporting an unready setup cancelled, the controller must independently invoke
abort, join the helper and verify an empty root receipt. Unknown cleanup remains
unfinished under the existing managed-run protocol.

The cgroup boundary handles ordinary detached/`setsid` descendants. The existing
guest account has passwordless sudo, so it does not contain a guest administrator
who deliberately moves work outside the scope. Dedicated build VM retirement
provides the final VM boundary.

## Durable managed projections

Node health advertises `managed-project-environment-events-v1` only with a valid
configured `AHVM_AGENT_EVENT_ORIGIN`. This advertises the event codec, not guest
helper availability; always perform the guest probe separately.

Finite builds reuse the exact managed controller argv and isolated receipt
lifetime documented in [MANAGED-AGENT-EVENTS.md](MANAGED-AGENT-EVENTS.md). They
use `harness: "environment"` and `kind: "project-environment"`, with build/run
UUIDs, project UUID, request/boot binding, setup provenance, hook presence,
`phase`, `log` and `logTruncated`. There is no provider/model/native chat session
or message identity. Optional source/revision/hook provenance can bind as cloning
resolves; once bound it cannot change. Build phases are `installing`, `starting`,
`ready`, `failed`, `cancelled`. The node's ordinary managed receipt phase remains
separate and authoritative about completion.

Pi/OpenCode checkpoints add an `environment` object with schema/project/revision,
optional hook/source provenance, the same progress information under `stage`,
and optional fixed `detail`. Both forms limit log strings to 16384 UTF-8 bytes,
reject unknown/credential fields, and freeze project/provenance identities.
The shared full checkpoint/patch fixture is
`rust/ahvm-daemon/src/run_events/project-environment-v1.json`.

Event-enabled jobs must write the existing `events.ndjson` journal. A controller
that only atomically publishes `result.json` can use bounded Cloud observation
through the ordinary file API, but must omit `event_delivery`. The host event
channel does not treat a lone snapshot as durable journal proof. A guest ready
projection alone cannot authorize image promotion or agent admission without
its native/root termination proof and the managed receipt.

## Qualification

The Linux guest gate is `scripts/test-project-environment.py`. Run `init HELPER`
in a disposable VM, sync and stop it, preserve only that VM's RAM bundle under a
new name, cold-start, then run `reopen HELPER`. It uses no model/provider request.

On 2026-10-11, the gate passed against published Ubuntu base
`cec23c433dcab4b0d4e0fb297b2e0c94b0a56f3f1dfb2c21f4edd299cad28657` on
`agent_house`, through a separate loopback daemon, normal worker broker/network
isolation, and a 1-vCPU/1-GiB local VM. Initial guest checks took **9.132 s**; guest checks after the completed
cold boot took **0.306 s**. These measurements exclude VM boot time. It checked no-op/repeated hooks, script/path/SHA
validation, changed revision/replacement/retry, finite inherited background
stdout, sibling survival, failed start/timeout joins, direct host cancellation,
cancellation while waiting for installation, actual sudo package installation,
bounded output, and install-preserving/start-replaying cold boot. Only gate
resources were created and retired; production and existing VMs were preserved.
