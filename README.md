# Agent House

Self-hosted, stateful Linux microVM sandboxes for running code and coding agents.
The runtime is Rust, using the pinned `libkrucible` fork of libkrun.

**Public early access.** The qualified server target is Linux x86_64 with KVM,
systemd and glibc 2.35+. The CLI also builds on macOS. This is not a claim of
production readiness for hostile multi-tenant workloads.

Install on a fresh supported Linux host:

```sh
curl -fsSL https://ahvm.app/install.sh | bash
```

[Product website](https://ahvm.app) · [Installation guide](https://ahvm.app/docs/)

## Current functionality

- Authenticated HTTP API and CLI for sandbox lifecycle and command execution.
- Persistent sessions and interactive terminals with detach/reattach and resize.
- Streaming file uploads with atomic replacement, and paginated downloads.
- Local disk/RAM snapshots, restore-as-new, idle-stop and explicit restart.
- Worker adoption after daemon restart and recovery from saved checkpoints.
- Managed outbound networking, explicit private TCP grants and authenticated previews.

The default [Ubuntu development image](docs/DEVELOPMENT-IMAGE.md) includes Node.js LTS, Bun,
Python, build tools, Claude Code, Codex, OpenCode and Pi. BusyBox is an explicit
minimal build option. There is no implemented Firecracker
backend, automatic wake-on-request, off-host backup or multi-host scheduler.

## Build and run

```sh
make build
make test
make check
```

For the native server bundle and prerequisites, see
[installation and operation](docs/RUST-INSTALL.md). Private submodule access is
required to build the VMM. Start with a fresh data directory; Go state is not
migrated.

```sh
ahvm --token-file /path/to/admin.token create dev --cpus 1 --memory 1024
ahvm --token-file /path/to/admin.token exec dev -- sh -c 'echo hello'
ahvm --token-file /path/to/admin.token shell dev
ahvm --token-file /path/to/admin.token delete dev
```

## Documentation

- [Installation and operations](docs/RUST-INSTALL.md)
- [Cutover and rollback](docs/RUST-CUTOVER.md)
- [File uploads](docs/FILE-UPLOADS.md)
- [Preview ports and private access](docs/NETWORK-ACCESS.md)
- [Network qualification](docs/NETWORK-QUALIFICATION.md)
- [Rewrite plan and history](docs/PLAN-rust-rewrite.md)

The Go implementation and its installers are retired. They remain in Git
history at pre-cutover commit `639aeee`; historical plans and experiment results
are evidence of earlier development, not current installation instructions.
Agent House originated as a fork of [Bhatti](https://github.com/sahil-shubham/bhatti).
