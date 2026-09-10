# Rust CLI and Linux installation

The Phase 6 bundle contains `ahvm`, `ahvm-daemon`, `ahvm-vmm`, `ahvm-netd`, static
`ahvm-forge`, the native library closure and a minimal BusyBox ext4 guest image.
The initial server target is Linux x86_64 with KVM and systemd. Build on the
oldest Linux/glibc you intend to support; the bundle records the build host's
glibc version and still uses the destination's glibc/loader. macOS CLI builds
work; macOS server packaging and Linux arm64 qualification are separate work.

## Build and install

Native build dependencies: Rust (including the `x86_64-unknown-linux-musl`
target), C/C++ and musl toolchains, clang/libclang, pkg-config, libzstd-dev,
patchelf, curl, bzip2, make, binutils, e2fsprogs and Python 3. Initialize the
pinned libkrucible submodule and provide the tested `libkrunfw.so.5` installation
in `AHVM_FW_DIR` (default `/usr/local/lib64`). The fork is linked natively into
the VMM. The build script downloads and checksum-verifies BusyBox 1.37.0, builds
it statically and inserts the just-built forge into the image. Guest init mounts
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
It refuses existing paths or units: this is a **fresh install**, not an update
or a Go cutover. `--prefix`, `--config-dir`, `--data-dir`, `--unit-name` and
`--user` allow a separate test installation. Keep ancestors accessible to the
service account. Installation requires root; workers run under the dedicated
account with KVM group access and no new privileges. The configured data path
must be on a filesystem with enough space for disk and RAM snapshots.

The image intentionally supplies only BusyBox and forge: shell, standard tools,
HTTP server and basic networking. It is not an Alpine package-manager image or
a browser/development environment. Richer image distribution is separate from
this minimal install gate.

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
and replace local files only after success. Uploads are currently atomic
single-frame writes, limited to **512 KiB**; larger inputs fail before a request.
Do not use this limit as a large-upload performance result. Snapshot deletion
currently removes the record, not its stored bundle. The client HTTP timeout
(default 600 seconds) is configurable with `--timeout`; it does not extend the
guest exec limit of 300 seconds. Cancelling the client does not cancel exec.

Interactive shell/attach requires a terminal. It forwards raw input, sends PTY
resize updates and restores local terminal settings on normal detach or errors.
Interactive attach uses the WebSocket stream; idle connections do not keep
VMs active. `session read --follow` uses REST for noninteractive consumers. Ctrl-C goes to the guest; Ctrl-] detaches. The guest
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

`journalctl -u ahvm-rust` shows daemon startup/errors; per-worker logs live under
the data directory. `LimitNOFILE=65536` accommodates the measured transient VMM
descriptor peak; `TasksMax=4096` bounds service tasks. Adjust sandbox quotas and
host capacity together; these settings do not promise a particular VM count.
The installer chooses an IPv4 resolver from the host's resolv.conf; override
`AHVM_DNS_RESOLVER` if needed.

Workers survive a daemon restart (`KillMode=process`) so the new daemon can adopt
them. **Stopping the systemd service does not stop sandboxes.** Stop/delete them
through the CLI before host maintenance or uninstall. To remove a test install,
first delete all its sandboxes, disable/stop its unit, remove that unit and its
three install/config/data directories, then run `systemctl daemon-reload`.
Do not remove a service account while it still owns workers or other installs.

The manual Rust release workflow uses a dedicated self-hosted `ahvm-build` runner
and never runs on PRs. It builds/checks a native bundle and can publish a
`rust-v*` tag only when explicitly requested. The existing Go release path remains
until Phase 7. Before cutover, test a fresh install and restart/recovery through
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

Verified on `agent_house`: **13.07 seconds, two 1-vCPU/256 MiB guests maximum**.
Exec exit codes, 256 KiB binary upload/download, DNS, private access and copy
isolation, preview browser bootstrap/revocation, snapshots, stop/start, worker
SIGKILL recovery and daemon adoption passed. Real WebSocket PTY input/output,
resize, detach/reattach and exit status passed; an idle attached terminal still
allowed automatic VM stop. The daemon and workers ran under the dedicated
service account. The **11 CLI contract tests** passed on macOS and Linux;
Clippy and formatting passed. Installer checks reject existing paths and a
tampered bundle before installation. The manual release workflow has not been
executed or published; the native bundle build and fresh install were tested
locally on the server. Phase 5's separate two-4-GiB
network qualification remains documented in `NETWORK-QUALIFICATION.md`.
