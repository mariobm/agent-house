# ahvm documentation

> [!IMPORTANT]
> **Documentation has moved to <https://ahvm.sh/docs>.**
>
> The website is the canonical reference and is updated with every release.
> The old Firecracker-era Markdown pages that used to live alongside this
> README are **retired** — they've been relocated to [`archive/v1/`](archive/v1/)
> for git history. They describe **ahvm v1 (Firecracker)** and are not
> maintained; the current engine is v2 (krucible).

## Quick links

| | |
| --- | --- |
| **[Quickstart](https://ahvm.sh/docs/quickstart/)** | Install and create your first sandbox |
| **[Concepts](https://ahvm.sh/docs/concepts/)** | Sandboxes, thermal states, the two binaries |
| **[Self-Hosting](https://ahvm.sh/docs/self-hosting/)** | Run ahvm on your own hardware, requirements, backups |
| **[CLI Reference](https://ahvm.sh/docs/reference/cli/)** | Every command with synopsis, flags, exit codes |
| **[API Reference](https://ahvm.sh/docs/reference/api/)** | HTTP API with curl examples and response shapes |
| **[Architecture](https://ahvm.sh/docs/under-the-hood/architecture/)** | System design, data flow, concurrency model |
| **[Decisions & Learnings](https://ahvm.sh/docs/under-the-hood/decisions/)** | Why ahvm is built the way it is |

For agents driving ahvm, start at **<https://ahvm.sh/agents.md>**. The full
machine-readable doc index is at **<https://ahvm.sh/llms.txt>**.

## Where each old doc went

If you landed here from an old link or a stale checkout, each retired page now
lives under [`archive/v1/`](archive/v1/) (frozen v1/Firecracker snapshot), and
the canonical, maintained version is on the website:

| Retired file (now in `archive/v1/`) | Canonical URL |
| --- | --- |
| `archive/v1/index.md`             | <https://ahvm.sh/docs/> |
| `archive/v1/quickstart.md`        | <https://ahvm.sh/docs/quickstart/> |
| `archive/v1/architecture.md`      | <https://ahvm.sh/docs/under-the-hood/architecture/> |
| `archive/v1/guest-agent.md`       | <https://ahvm.sh/docs/under-the-hood/lohar-the-blacksmith/> |
| `archive/v1/networking.md`        | <https://ahvm.sh/docs/under-the-hood/networking/> |
| `archive/v1/wire-protocol.md`     | <https://ahvm.sh/docs/under-the-hood/wire-protocol/> |
| `archive/v1/thermal-management.md`| <https://ahvm.sh/docs/under-the-hood/thermal-states/> |
| `archive/v1/decisions.md`         | <https://ahvm.sh/docs/under-the-hood/decisions/> |
| `archive/v1/api-reference.md`     | <https://ahvm.sh/docs/reference/api/> |
| `archive/v1/cli-reference.md`     | <https://ahvm.sh/docs/reference/cli/> |
| `archive/v1/kernel.md`            | <https://ahvm.sh/docs/contributing/kernel/> |
| `archive/v1/testing.md`           | <https://ahvm.sh/docs/contributing/testing/> |
| `archive/v1/tiers.md`             | <https://ahvm.sh/docs/contributing/adding-a-tier/> |

> **v1 users:** the website's frozen [v1 (Firecracker) docs](https://ahvm.sh/v1/docs/)
> are the maintained place to read these; the `archive/v1/` copies here are a
> git-history snapshot only.

## What's still in this folder

- **`HANDOFF-krucible.md` + `PLAN-krucible-*.md`** — the current engine design
  docs (v2/krucible): the engineer hand-off plus the cold-tier / init-model /
  productionization plans. Contributor-facing; the website is the user-facing
  reference.
- **`archive/`** — historical PLAN docs and investigations preserved for git
  archaeology, plus **`archive/v1/`** (the retired Firecracker-era reference
  pages). Out of date by design. Do not edit.
- **`internal/`** — gitignored local planning notes (launch posts, drafts,
  internal-only plans). Will not appear in clones.

## Editing documentation

To change documentation, edit the corresponding file in
**[`ahvm.sh/src/content/docs/`](https://github.com/mariobm/agent-house.sh)**
and submit a PR there. Every Starlight page also has an "Edit this page" link
at the bottom that links straight to the right file.
