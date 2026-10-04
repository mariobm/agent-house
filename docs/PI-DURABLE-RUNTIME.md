# Pi Durable guest runtime

The Ubuntu recipe adds an experimental, opt-in Pi Durable application runtime
alongside the existing OpenCode 2.0.18 installation. It retains the `ubuntu-dev`
image identifier, Node 24.21.0, existing CLI pins, guest `ahvm` account and
OpenCode default. An application controller selects the harness for each chat;
the image does not start a Pi service, authenticate an account or create an
agent conversation during provisioning. Existing VM disks are not upgraded.

## Installed contract

`/opt/ahvm-pi-durable/node_modules` contains exact `@earendil-works/pi-durable`,
`@earendil-works/pi-ai` and `@earendil-works/chord` **1.0.2**. Their complete
dependency tree is pinned by the committed npm lock and installed as guest
`ahvm` with `npm ci --omit=dev --ignore-scripts`. The installed tree then becomes
root-owned. Node's built-in `node:sqlite` supplies SQLite; no native SQLite
package or package build script is required. The Pi CLI is a separate tool and
retains its existing 0.85.1 pin.

`/usr/local/share/ahvm/pi-durable-runtime.json` has schema `1`, harness
`pi-durable`, exact `packages`, `module_root`, `node_minimum` (22.19.0), the
image's `node_version`, `package_lock_sha256`, `tool_scope_helper` and
`tool_scope_protocol` (1). `ahvm-pi-durable-check` verifies root ownership and
write permissions, package versions, lock hash, ESM imports (including the
Codex provider) and an in-memory SQLite open/close. Success emits JSON with
`available: true`; failure exits nonzero. The probe makes no model or
authorization request and writes no application database.

Guest controllers can dynamically import the pinned packages through absolute
file URLs. The supported Codex provider export is
`@earendil-works/pi-ai/providers/openai-codex`; its provider object supplies
OAuth login/refresh. The 1.0.2 catalog includes `openai-codex/gpt-6.1-sol` with
`openai-codex-responses`. The package's `/oauth` entry is type-only, so it must
not be mistaken for a runtime login module. Account authorization and credential
storage belong to the controller and remain outside the image.

## Tool containment

The guest uses Forge as PID 1. The image mounts cgroup v2 and creates root-owned
`/sys/fs/cgroup/ahvm-pi-tool`, plus a private 16-MiB tmpfs at
`/run/ahvm-pi-tool` for per-boot receipts and admission tombstones. Kernels
without this capability can still boot OpenCode; the Pi helper rejects tool
admission. Controllers must run `sudo -n ahvm-pi-tool probe` before admitting a
Pi run and must check its success independently of the package manifest.

The root-owned Python helper uses isolated mode (`-I`), accepts a 32-character
lowercase hexadecimal run ID and a 64-character lowercase hexadecimal tool ID,
and executes the command after dropping to `ahvm`:

```sh
sudo -n ahvm-pi-tool run RUN32 TOOL64 --cwd /workspace -- /bin/bash -c COMMAND
sudo -n ahvm-pi-tool result RUN32 TOOL64
sudo -n ahvm-pi-tool finish RUN32
sudo -n ahvm-pi-tool abort RUN32
```

Admission and cancellation serialize on a root-only lock. The command child
acknowledges cgroup membership before admission unlocks; an abort cannot return
empty and then admit that child. `abort` seals new launches, invokes
`cgroup.kill`, and returns success only after `cgroup.events` reports no live
descendants. `finish` (also named `check`) seals admission and rejects a live
scope. Other run scopes survive. Tool IDs cannot be reused during a guest boot.
The helper also kills and joins detached/background descendants when a command
leader exits, so a completed bash tool cannot leave a server running.

Command stdout, stderr and process status are tool data. The separate `result`
operation returns a root-owned receipt only after a successful empty join:
`{ "schema": 1, "run_id": "...", "tool_id": "...", "exit_code": 0, "empty": true }`.
Missing, malformed or nonempty receipts fail closed. This distinguishes a
legitimate command exit such as 75 from a helper failure. The controller must
query this receipt, finish the run scope before reporting completion, and issue
`abort` independently if a supervisor is forcibly killed. Killing that
supervisor alone does not prove termination of its tool descendants.

The helper supplies a cancellation boundary inside the existing VM. The guest
`ahvm` account retains its existing passwordless sudo; it is a convenience
account rather than protection from a guest administrator. The controller must
preserve leases, ownership checks, replay policy and persistent SQLite ownership.

## Build and qualification

Use the normal clean Ubuntu builder with a qualified static Forge:

```sh
FORGE_BIN=/path/to/ahvm-forge scripts/ubuntu-dev-rootfs.sh /tmp/ubuntu-dev.ext4
```

Ubuntu security packages remain resolved at build time, so the complete image
is not byte-for-byte reproducible. Durable's dependency lock is frozen; the
installed OS package list and Forge checksum remain recorded in the image.
No signed catalog or production default is changed by building this artifact.

On 2026-10-04, a clean candidate built from the exact recipe below passed the
complete KVM gate in **19.97 seconds** on `agent_house`, using the installed
0.3.13 cloud runtime and its static Forge, one 2-vCPU/4-GiB test VM, and a private
loopback daemon. The gate VM was deleted; a fresh disposable integration VM was
created from the same candidate. The `scripts/test-dev-image.py` gate covers the
existing authenticated OpenCode API, tools, apt/npm/pip, HTTPS, compilation,
PTY and VM stop/start persistence, plus Durable's SQLite receipt deduplication,
one deterministic faux-provider turn, persisted answer/receipt after reopen,
Codex catalog/imports and mocked headless OAuth admission/cancellation. It
makes zero external model requests and creates no real authorization or login.

`scripts/test-pi-tool-scope.py` additionally qualifies `setsid` with double-fork,
sibling preservation, detached-child cleanup on command completion, sealed
admission, eight admission/abort races, root-only trusted exit receipts,
tool-identity reuse rejection, unprivileged use and invalid IDs. All tool
processes run as `ahvm`; failed helper checks cannot be counted as tool success.

The 16-GiB logical candidate allocated approximately 2.8 GiB. It remains private
at `/var/tmp/ahvm-pi-durable-20261004/ubuntu-dev-qualified.ext4`; build and gate
evidence is in `qualified-build.log`, `qualified-gate.log`, `source.sha256` and
`image.sha256` in that same host directory. The current signed catalog and
production image/default were not changed. Exact SHA-256 evidence:

| Input | SHA-256 |
|---|---|
| Candidate raw image | `f57e25e4c50ff12287f4b24828f0f7ddd542a7f2cdf3e5eec47adabb1d9a7856` |
| Tool helper | `c41d55a54f134d069a888f1cae3e74a5574e3a3bb73de12052cecb9db454985e` |
| Guest init | `8cc8d70ba54b8e2db69eb12b333ad6bfea05cd570a96b5d67cf8a637f1a2421d` |
| Durable npm lock | `a6d19b2eb685e180fd228ff11731324e9cfccbb4f8bac2f9e163314a945928aa` |
| Runtime probe | `b4dfb5f366bc311261654dd8d47e1b731b71893eb60fe8ff7d2d4a2880469426` |
| Tool scope gate | `a82640ffd10f00a6021150895ace4efeb89ebbea9f9a68ab53522f9dbef9186f` |
| Durable gate | `7f44119d6f6b52d4f0607604b2fbd1936cdd53ec6f1b0862a9ccf092b377b445` |
| Static Forge | `fda75aa8f5a695e32ecc0590e126a9d40caa0e6e4c73e925d5e05290d7eba08d` |

An initial direct Pi AI probe of
`opencode/muse-spark-1.3-contributor-free` returned HTTP 401 with a dummy bearer
key. Removing the Authorization header returned HTTP 403 `FreeTierError`: the
provider restricts its anonymous free tier to OpenCode. Both synthetic probes
used zero tokens and no account credentials. The catalog entry does not grant
anonymous provider access. The Pi pilot instead uses explicit Codex subscription
sign-in; actual authorization, inference, refresh/removal and mobile behavior
remain application qualification work.
