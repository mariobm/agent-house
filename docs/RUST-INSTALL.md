# Rust CLI and Linux installation

For the normal Mac/Linux client and SSH workflow, see [remote hosts and upgrades](REMOTE-HOSTS.md).

```sh
curl -fsSL https://ahvm.app/install.sh | bash
export PATH="$HOME/.local/bin:$PATH"
ahvm host add home --ssh root@YOUR_SERVER_IP --install
```

The client is distributed separately from the server runtime and Ubuntu image.
The server downloads its signed guest image directly from `images.ahvm.app`.
The server requires Linux x86_64/KVM, enabled Landlock ABI 6 (Linux 6.12+),
systemd 254+, unified cgroup v2 with cpu/memory/pids controllers, glibc 2.35+,
and a VM-data filesystem supporting idmapped mounts (qualified on ext4).
The v0.2.0 CLI and daemon use static musl builds; VMM native dependencies retain
the glibc 2.35 baseline. macOS server packaging and Linux arm64 qualification
are separate work. Review [licensing](LICENSING.md) before installation.

The following sections cover manual native builds and direct API configuration.

## Build and install

Packaging builds the [Ubuntu development image](DEVELOPMENT-IMAGE.md) by default
(root or sudo is needed for image provisioning). To reuse an image, set
`AHVM_GUEST_IMAGE=/path/to/ubuntu-dev.ext4`. For a lightweight test bundle,
explicitly set `AHVM_GUEST_PROFILE=minimal`.

Native build dependencies: Rust (including the `x86_64-unknown-linux-musl`
target), C/C++ and musl toolchains, clang/libclang, pkg-config, libzstd-dev,
patchelf, curl, bzip2, make, binutils, e2fsprogs and Python 3. Initialize the
pinned libkrucible submodule and provide the tested `libkrunfw.so.5` installation
in `AHVM_FW_DIR` (default `/usr/local/lib64`). The fork is linked natively into
the VMM. The default builder verifies the Ubuntu and Node archives, installs
the development tools and inserts the just-built forge into the image. Guest init mounts
proc, sysfs, devtmpfs and devpts before execing forge as PID 1, so PTY shells
work without manual guest setup. No Go is used.

```sh
scripts/package-rust.sh /var/tmp/ahvm-rust-bundle
sudo /var/tmp/ahvm-rust-bundle/install.sh /var/tmp/ahvm-rust-bundle --no-start
sudoedit /etc/ahvm-rust/daemon.env
sudo systemctl enable --now ahvm-rust
sudo /opt/ahvm-rust/bin/ahvm --token-file /etc/ahvm-rust/admin.token list
```

The installer verifies bundle checksums, creates an `ahvm-rust` service account,
and installs under `/opt/ahvm-rust`, `/etc/ahvm-rust` and `/var/lib/ahvm-rust`.
Root-owned worker records and jail mount points live separately in
`/var/lib/ahvm-rust-worker-broker`; the daemon cannot modify them.
It refuses existing paths or units: this is a **fresh install**, not an update
or a Go cutover. `--prefix`, `--config-dir`, `--data-dir`, `--unit-name` and
`--user` allow a separate test installation. `--broker-state-dir` selects the
separate root-owned state directory. These directories must not nest inside
one another; ancestors must be root-owned and accessible to the service account.
Installation requires root. The API daemon remains unprivileged; a small
root-owned broker starts each VMM and gateway with distinct host UID/GIDs,
no supplementary groups or capabilities, and a restricted filesystem view.
Fresh installations also
create a private slice and a delegated worker service, enabling per-VM CPU,
memory, swap and task limits. The configured data path
must be on a filesystem with enough space for disk and RAM snapshots.

New Linux VMM workers also apply the Landlock filesystem and signal policy
before any guest runs. Missing managed networking leaves the VM offline.
The broker accepts fixed worker roles for the configured daemon account; it
does not accept arbitrary commands, executable paths or requested identities.
Its root-only configuration is `/etc/ahvm-rust/worker-broker.json`, and the
daemon uses `AHVM_WORKER_BROKER_SOCKET` to connect to it.

The installer reserves 1,048,576 UID/GID values starting at 1,073,741,824.
**This is lifetime launch capacity, not a limit on simultaneous VMs.** Each
VMM or gateway launch consumes a fresh identity, including restarts and failed
launch intents. Identities are never automatically reused. Use
`--worker-id-base` and `--worker-id-count` for another reserved range; installation
rejects overlaps with users, groups, subordinate IDs and previous AHVM installations.
Reservations are retained in `/etc/ahvm-worker-ranges/` even after uninstall.
Do not remove them or reset the broker allocation counter to reclaim IDs.

Older installations need an explicit migration and all legacy workers stopped.
An ordinary host upgrade refuses before stopping services if broker configuration
is missing. See [worker isolation and migration](WORKER-ISOLATION.md) for the
migration procedure, replicated NBD and GPU device configuration, and the
remaining security limits. Existing VM disks do not need to be deleted.

New sandboxes use Ubuntu with Node.js LTS, Bun, Python and the AI CLIs already
installed. No image selection is required. See [development image usage and
qualification](DEVELOPMENT-IMAGE.md). The explicit minimal profile is intended
for lightweight tests.

For Cloud-managed agent streaming, explicitly add
`AHVM_AGENT_EVENT_ORIGIN=https://YOUR_CLOUD_ORIGIN` to the host's
`/etc/ahvm-rust/daemon.env` (or the configured installation's environment file)
before starting the upgraded daemon. The destination must be an HTTPS origin.
`ahvm health` advertises `managed-agent-events-v1` only with a valid configured
origin; installations without it retain the existing managed-run protocol.
See [event delivery and rollout](MANAGED-AGENT-EVENTS.md) for the private grant,
replay, pressure and retention contract. The installer does not choose a Cloud
destination automatically.

## Connection and commands

Build a standalone client with:

```sh
cargo build --manifest-path rust/Cargo.toml --release -p ahvm-cli
```
 The binary is `rust/target/release/ahvm`. Use
`AHVM_ENDPOINT` (default `http://127.0.0.1:8080`) and `AHVM_TOKEN_FILE`, or
`AHVM_TOKEN`. Token files avoid putting credentials in process arguments.
Copy a token to a user-owned mode-600 file if using the client without sudo.
Use HTTPS or an SSH tunnel for remote control; no TLS verification bypass is
provided. The client refuses redirects and does not retry mutations.

```sh
export AHVM_TOKEN_FILE="$HOME/.config/ahvm/admin.token"
ahvm health
ahvm create dev --cpus 1 --memory 4096
ahvm list
ahvm exec dev -- sh -c 'printf "hello\n"; exit 7'
ahvm files put dev ./hello.txt /workspace/hello.txt
ahvm files get dev /workspace/hello.txt ./download.txt
ahvm files list dev /workspace
ahvm shell dev                     # Ctrl-] detaches; session survives
ahvm session list dev
ahvm session attach dev SESSION_ID
ahvm session read dev SESSION_ID --follow --from-seq 0
printf 'echo hello\n' | ahvm session input dev SESSION_ID
ahvm snapshot create dev checkpoint
ahvm snapshot restore SNAPSHOT_ID copy
ahvm stop dev
ahvm start dev
ahvm delete copy
ahvm delete dev
```

`exec` preserves argv and returns the guest exit status; use `--` before guest
arguments. `--json` returns structured output; session reads emit one JSON
object per chunk with the authoritative `next_seq` cursor. File downloads page
and replace local files only after success. Uploads stream binary data in 64 KiB
chunks, from a file or stdin, with no fixed total size cap. The guest atomically
replaces the destination only after the complete transfer; an interrupted
transfer preserves the original. Use matching daemon and guest artifacts; see
[upload protocol and limits](FILE-UPLOADS.md). Snapshot deletion
currently removes the record, not its stored bundle. The client HTTP timeout
(default 600 seconds) is configurable with `--timeout`; it does not extend the
guest exec limit of 300 seconds. Cancelling the client does not cancel exec.

Interactive shell/attach requires a terminal. It forwards raw input, sends PTY
resize updates and restores local terminal settings on normal detach or errors.
Interactive attach uses the WebSocket stream and keeps the VM active while
connected, including while waiting for input. After detaching, the configured
idle policy applies. `session read --follow` uses REST for noninteractive consumers. Ctrl-C goes to the guest; Ctrl-] detaches. The guest
session remains listed until explicitly deleted.

## Previews and private access

The installer binds control to `127.0.0.1:8080` and previews to
`127.0.0.1:8081` with `preview.localhost`. Configure wildcard DNS and HTTPS on a
**dedicated preview domain** for remote browser use, forwarding Host unchanged
to the preview listener. Never serve preview content on the control API origin.
Do not log bootstrap query tokens at your TLS proxy.

```sh
ahvm preview enable dev 8080
ahvm preview access dev 8080 --base-url https://preview.example.com
ahvm preview list dev
ahvm preview revoke dev 8080
```

Access prints a scoped credential and browser URL; another access request rotates
that credential. The guest app must actually listen on the registered port.
For localhost use `--base-url http://preview.localhost:8081`.

Private access stays host-admin policy, not a tenant CLI capability. Edit
`/etc/ahvm-rust/private-access.json` with exact owner, sandbox and IPv4:port grants
as described in [NETWORK-ACCESS.md](NETWORK-ACCESS.md). Stop affected sandboxes,
edit the policy, restart the daemon and then start the sandboxes. Grants are not
inherited by restored copies. The bootstrap owner is `admin`.

## Operations and release

`journalctl -u ahvm-rust -u ahvm-rust-worker-broker` shows daemon and launcher
startup/errors; per-worker logs live under
the data directory. `LimitNOFILE=65536` accommodates the measured transient VMM
descriptor peak; `TasksMax=4096` bounds service tasks. Adjust sandbox quotas and
host capacity together; these settings do not promise a particular VM count.
The installer chooses an IPv4 resolver from the host's resolv.conf; override
`AHVM_DNS_RESOLVER` if needed.

Workers survive daemon and broker restarts (`KillMode=process`) so they can be
adopted from verified records. **Stopping either service does not stop sandboxes.** Stop/delete them
through the CLI before host maintenance or uninstall. To remove a test install,
first delete all its sandboxes, disable/stop the daemon, broker and delegated
worker services, and remove their units and private slice. Remove the
installation/configuration/data and broker-state directories only after verifying
their worker cgroups are empty, then run `systemctl daemon-reload`.
Retain the identity reservation. Do not remove a service account while it is
used by another installation.

The manual release workflow builds standalone Mac clients and a server-only
archive on the qualified native build runner. Use the signed publisher described
in [REMOTE-HOSTS.md](REMOTE-HOSTS.md) to promote assets and update Homebrew.
Before cutover, test a fresh install and restart/recovery through
the packaged CLI, not binaries from a development checkout.

## Validation

Run `scripts/test-rust-cli.py rust/target/debug/ahvm` for the no-VM client contract
suite. Run the packaged acceptance gate on KVM with a disposable installation:

```sh
sudo scripts/test-rust-install.py /path/to/install /path/to/config /path/to/data ahvm-test-phase6
```

The unit must have an `ahvm-test-` name. The gate temporarily edits its policy
and idle configuration, restarts it, uses at most two guests, restores the
configuration files and deletes its guests afterward. Stored snapshot bundles
remain until the disposable data directory is removed.

Worker-isolation qualification on `agent_house` (2026-10-08): **18.16 seconds,
two 1-vCPU/256 MiB guests maximum**.
32 MiB binary file and stdin uploads passed, with checksum verification and a
full download comparison. Empty upload passed; dropping a real chunked HTTP
request preserved the old destination and removed the guest temporary file.
Exec exit codes, DNS, private access and copy
isolation, preview browser bootstrap/revocation, snapshots, stop/start, worker
SIGKILL recovery and daemon adoption passed. Real WebSocket PTY input/output,
resize, detach/reattach and exit status passed. An attached shell stayed alive
beyond the test idle deadline; detaching allowed automatic VM stop. The daemon
ran under the dedicated service account; VMM and gateway processes used separate
broker-assigned identities. See [worker isolation](WORKER-ISOLATION.md) for
the adversarial and replicated-storage gates, requirements and limitations.
Phase 5's historical network qualification remains documented in
`NETWORK-QUALIFICATION.md`.
