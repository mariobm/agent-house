# agent-house fork notes

AHVM (Agent House Virtual Machine) is a fork of
[Bhatti](https://github.com/sahil-shubham/bhatti), merged as base at upstream
`0e70b75`.

## Remotes

- `origin` — ours (private `mariobm/agent-house`), push here.
- `upstream` — `https://github.com/sahil-shubham/bhatti.git`, pull-only.
  Historical upstream. Do not merge its retired Go runtime back into the Rust tree.

## Policy

- Keep `libkrucible` VMM fork pinned; do not diverge it without a reason.
  Exception for issue #11: the pinned private `mariobm/libkrucible` repository
  preserves IA32_XSS and active console queues during cold recovery. The
  original pin deterministically broke restored guest execution on KVM.
  See `docs/STATUS-engine.md` for the reproducer and validation.
- Networking recovery exception (Phase 5): preserve virtio-net queues/features
  across cold restore and reconnect UnixstreamPath backends after netd restarts.
  Both failures reproduced with Rust and Go; Rust recovery now passes the
  repeated TCP/DNS gate in `experiments/net-spike/QUALIFICATION.md`.
- Preview backpressure exception (Phase 5 qualification): a slow HTTP reader
  reproducibly blocked the shared vsock transport and unrelated VM RPCs. The
  Unix proxy now buffers partial nonblocking writes within a 256 KiB credit
  window. See `docs/NETWORK-QUALIFICATION.md` for failure and recovery evidence.
- Build differentiation above the engine: durability (S3 offload of `bundle/`),
  multi-host routing, identity, quotas, observability, packaging.
- Research context: `MICROVM_PLATFORM_RESEARCH.md`, `AWS_MICROVM_PLATFORM_PLAN.md`.

## Server

The private deployment target is `agent_house`. Use `make test` and `make check`
locally and the packaged KVM gate on the server. No host-specific secrets in
this repo. See `docs/RUST-CUTOVER.md` for installation and rollback.

## CI dependency access

Workflows use `.github/actions/checkout-libkrucible` to fetch the exact
submodule commit from private `mariobm/libkrucible`. The `LIBKRUCIBLE_SSH_KEY`
Actions secret in `agent-house` contains its dedicated read-only deploy key;
checkout removes credentials after fetching. Rotate the secret and the
`agent-house-actions-readonly` deploy key together. The default
`GITHUB_TOKEN` cannot read a different private repository.
