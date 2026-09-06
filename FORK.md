# agent-house fork notes

AHVM (Agent House Virtual Machine) is a fork of
[Bhatti](https://github.com/sahil-shubham/bhatti), merged as base at upstream
`0e70b75`.

## Remotes

- `origin` — ours (private `mariobm/agent-house`), push here.
- `upstream` — `https://github.com/sahil-shubham/bhatti.git`, pull-only.
  Sync with: `git fetch upstream && git merge upstream/main`

## Policy

- Keep `libkrucible` VMM fork pinned; do not diverge it without a reason.
  Hard hypervisor work (snapshot format, device model, kernel) comes later.
- Build differentiation above the engine: durability (S3 offload of `bundle/`),
  multi-host routing, identity, quotas, observability, packaging.
- Research context: `MICROVM_PLATFORM_RESEARCH.md`, `AWS_MICROVM_PLATFORM_PLAN.md`.

## Server

Dedicated-server credentials arrive separately. Until then: local builds,
`go vet`/`go test`, docs. No host-specific secrets in this repo.
