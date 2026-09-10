# Rust cutover (Phase 7)

Phase 7 retires the Go runtime and its build/install/release paths. The current
server target is Linux x86_64/KVM. This phase does not publish a release, expose
public listeners or complete the separate pre-release product improvements.

## Deployment layout

On `agent_house`, the primary installation uses:

- `/opt/ahvm-rust`: complete runtime, firmware and guest image.
- `/etc/ahvm-rust`: protected credentials and daemon configuration.
- `/var/lib/ahvm-rust`: fresh Rust state.
- `ahvm-rust.service`: persistent service, with loopback control/preview listeners.
- `/usr/local/bin/ahvm`: symlink to the primary Rust client.

A separately installed baseline uses the same names with `-rollback` appended,
including `ahvm-rust-rollback.service`. Its credentials and data are separate.
Only one service should be active: they use the same loopback ports. Never run
two daemons against one data directory or share mutable guest disks.

There was no active Go service or installed CLI on this host before cutover.
The older `/root/agent-house` development checkout was left untouched. The saved
baseline is merged Rust commit `639aeee`, before source cleanup. Switching back
to it tests deployment rollback; it is not evidence of Go state migration or
compatibility with arbitrary future schemas or snapshot formats.

## Operate

```sh
ahvm --token-file /etc/ahvm-rust/admin.token health
ahvm --token-file /etc/ahvm-rust/admin.token list
systemctl status ahvm-rust
```

Use root or a securely provisioned token file for the authenticated commands.
Credentials are not included in this repository. Remote access uses SSH or an
SSH tunnel; no public release, domain or firewall change is part of cutover.

## Roll back

First stop every primary sandbox using its CLI and verify the list contains no
running workers. `systemctl stop` alone intentionally leaves workers alive.
Keep the primary artifacts, configuration and data intact for a later return.
The baseline restores its own state; new primary data does not appear there.

```sh
systemctl disable --now ahvm-rust
systemctl enable --now ahvm-rust-rollback
ln -sfn /opt/ahvm-rust-rollback/bin/ahvm /usr/local/bin/ahvm
ahvm --token-file /etc/ahvm-rust-rollback/admin.token health
```

To return, stop baseline sandboxes first, disable its service, enable
`ahvm-rust`, and point the CLI symlink back at `/opt/ahvm-rust/bin/ahvm`.
Use `/etc/ahvm-rust/admin.token` again. Do not overwrite either data directory.
A future upgrade that changes the schema/protocol needs a new tested baseline
and backup plan; binary rollback alone does not reverse data migrations.

## Verification

Verified on `agent_house` on 2026-09-10:

- Candidate runtime/build source: `e821eeb`; VMM pin `39626f9`.
- Fresh packaged KVM acceptance: **14.18 seconds**, at most two 1-vCPU/256 MiB
  guests. Covers exec, 32 MiB uploads, interruption cleanup, terminals, previews,
  networking, snapshot isolation, daemon adoption and worker recovery.
- Baseline → candidate → baseline → candidate: saved marker files in each
  independent installation survived stop/start and both direction changes.
- Primary service left enabled and active; rollback service inactive. Both
  sandbox registries empty after testing. Listeners remain on loopback.
- `make check` and `make test` pass on macOS and Linux, including 11 CLI contract tests.
  Workflow YAML and shell syntax validated; no tracked Go source or Go CI jobs.

The rollback baseline remains installed intentionally. Disposable acceptance
installation and build scratch are removed after collecting test logs. No
release/tag was created and no public endpoint was enabled.

The normal CI uses Rust unit/integration tests without a hypervisor, CLI
contract tests, Clippy and formatting. Private submodule checkout is required
for Cargo workspace resolution. The native bundle remains a manual self-hosted
build. The retired Go implementation is retrievable from Git history; historical
benchmarks and plans do not establish current Rust performance or capabilities.
