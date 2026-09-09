# PLAN: Rust-only rewrite (post PR #2 merge)

Goal: replace the entire Go control plane with Rust. `libkrucible` (348 Rust
files) already is Rust and stays as-is. Everything else — ~56k lines of Go
measured on the `codex/rename-bhatti-to-ahvm` branch — gets ported.

## 0. Why this order matters

Bottom-up, one crate at a time, each phase proven against the running Go
system before moving on. No big-bang. The KVM integration suite stays green
throughout; it is the rewrite's safety net, not its victim.

## 1. Inventory (measured, PR branch, incl. tests)

| Area | Files | LOC | Notes |
|---|---|---:|---|
| `cmd/forge` (guest PID1 agent) | 39 | ~13,600 | Must stay tiny, static, no libc. Hardest size discipline |
| `pkg/server` (REST/WS, proxy, thermal) | 34 | ~12,100 | tokio+axum target |
| `cmd/ahvm` (CLI + daemon serve) | 25 | ~8,200 | clap target |
| `pkg/engine/krucible` | 39 | ~6,500 | Owns VMM lifecycle; kills cgo when ported |
| `pkg/store` (SQLite) | 19 | ~4,900 | rusqlite target, schema must stay compatible |
| `pkg/agent` (wire protocol) | 7 | ~2,300 | Port first; freeze the bytes |
| `pkg/dns` | 4 | ~2,100 | trust-dns target |
| `pkg/gateway` (L7 secrets) | 9 | ~1,600 | |
| `pkg/oci` | 6 | ~1,200 | oci-client target |
| `cmd/ahvm-netd` (gVisor gateway) | 6 | ~1,000 | **Highest risk, see §4** |
| `cmd/vmm` (cgo helper) | 2 | ~350 | Deleted by design, not ported |
| `pkg/forward`, `pkg/configdrive`, misc | 3 | ~200 | |
| `cmd/krucible-mkimage`, `krucible-probe` | 2 | ~100 | |

## 2. Target layout (one workspace, `rust/`)

```text
rust/
  ahvm-proto/    new frame codec + message types (no legacy readers)
  ahvm-store/    rusqlite, schema redesigned (no production data to migrate)
  ahvm-forge/    guest agent (static musl, size-budgeted, replaces cmd/forge)
  ahvm-engine/   VMM lifecycle, links libkrucible as a native crate (no cgo),
                 spawns one worker process per VM (keeps today's isolation)
  ahvm-netd/     userspace gateway (replaces gVisor; biggest unknown, §4)
  ahvm-daemon/   axum REST/WS + thermal manager + scheduler
  ahvm-cli/      clap CLI, same workflows (output free to improve)
  ahvm-oci/      image pull/flatten
  ahvm-dns/      embedded responder
```

Guiding rule: nothing needs to be byte-identical for its own sake. Wherever
the Rust version can be more efficient or cleaner, do it better — frozen
vectors below exist as parity-test references, not as a freeze on the new
design. Nothing is carried over: no production data exists, so wire frames,
storage schema, snapshots, CLI output, and internal APIs are all free to be
redesigned better. No migration shims, no legacy readers.

## 3. Phases (each shippable, each gated)

### Phase 0 — Vectors + conformance harness (1–2 wks)
- Record protocol vectors (frames, config JSON) as parity-test references.
- Black-box conformance suite at CLI level: same workflows must work on both
  trees (behavioral parity; output text may improve).
- CI builds both trees; conformance must pass on Go before Rust exists.
- **Exit:** harness green on Go; vectors recorded in repo.

### Phase 1 — `ahvm-proto` + `ahvm-store` (2–3 wks)
- New protocol design, fuzzed (proptest-style roundtrips + adversarial inputs).
- rusqlite store with a clean redesigned schema; parity tests assert the same
  workflows persist and query the same facts, not byte compatibility.
- **Exit:** fuzz clean 1h; store roundtrip tests green.

### Phase 2 — `ahvm-forge` guest agent (3–4 wks)
- Port exec/PTY/files/sessions/systemctl-shim; static musl; strip; size budget
  (e.g. <8 MB) enforced in CI.
- Validate with the ported guest unit suite + live boot driven by a Rust
  test engine against the Rust daemon (no mixed-version runs).
- **Exit:** Rust daemon + Rust forge passes agent KVM suite; binary size in budget.

### Phase 3 — `ahvm-engine` (4–6 wks)
- Native libkrucible dependency (path/crates.io); per-VM worker processes;
  port snapshot/restore/thermal/recovery. **Delete `cmd/vmm` + all cgo.**
- This is where this week's vsock flake lives: add a deterministic
  regression test for back-to-back SIGKILL→relaunch (fixed-CID reuse) and
  keep a bounded retry until the fork-level fix lands.
- Snapshot format redesigned for chunked S3-backed storage (see §7);
  no import path from Go bundles (nothing in production to import).
- **Exit:** full KVM suite green on Rust engine + Go daemon.

### Phase 4 — `ahvm-daemon` (4–6 wks, split in two)
- (a) axum REST/WS, auth, persistent sandbox records; exec/files/sessions
  over the engine backend. (b) Thermal manager, scheduler; lifecycle
  transitions, concurrency limits, restart recovery.
- API surface free to improve, with the conformance suite (not byte diffs)
  as the gate. Acceptance on a KVM host: create → exec → snapshot → kill
  worker → restore → exec, plus daemon restart with state preserved.
- **Exit:** conformance suite green on Rust daemon + Rust engine; Go daemon
  retired from CI (kept as reference for one release).

### Phase 5 — Practical sandbox networking (`ahvm-netd`)
- Build the simplest reliable networking layer for agents running code in VMs.
  Go netd is a measured baseline, not a compatibility specification. Keep useful
  capabilities and document intentional changes.
- Start with a one-week spike: evaluate an existing Rust stack (starting with
  smoltcp) against Go netd. Do not implement a custom TCP stack. Deliver a
  prototype, measurements, limitations, and an implementation recommendation.
- Initial target: outbound TCP and DNS; default isolation from host/private/
  metadata destinations and other sandboxes; explicit access exceptions,
  project connections, and deliberately exposed preview ports. Broader UDP
  support is a scope decision based on workloads, not an implicit promise.
- Exercise repository cloning, dependency installation, HTTPS API calls, and a
  preview on `agent_house`. Measure throughput, latency, CPU, memory, and
  connection churn; set acceptance thresholds from measurements before choosing
  a design. Test bounded resource use, unauthorized access, and malformed traffic.
- New connections must work after VM stop/start, snapshot restore, and netd
  restart. Existing connections may break; applications must reconnect.
- Fuzzing and a focused security review are required before deployment for
  untrusted guests. Feasibility probes alone do not establish isolation.
- Escape hatch: integrate Go netd with the Rust daemon as a temporary sidecar,
  with a tracked replacement task. This explicitly defers the Rust-only packaging
  and zero-Go cutover exits below.
- **Exit:** agreed workloads and isolation tests pass on `agent_house`, measured
  performance is acceptable, and operational limitations are documented.
- Spike progress and reproducible probes: [`../experiments/net-spike/README.md`](../experiments/net-spike/README.md).

### Phase 6 — CLI + packaging (2–3 wks)
- clap CLI: same workflows must work (conformance), output and flags free to
  improve; install.sh, systemd units, release workflow, docs.
- **Exit:** install-from-scratch on ahvm-node-01 using only Rust artifacts.

### Phase 7 — Cutover (2 wks)
- Fresh state per host (no migration); flag-day cutover per host with
  rollback = previous binary. Delete Go tree.
- **Exit:** repo is Rust + libkrucible submodule only; CI has zero Go.

## 4. Risks (stated plainly)

1. **netd has no Rust gVisor.** smoltcp is the candidate, not the answer. This
   needs a measured feasibility decision before implementation; keep the
   existing Go service as an integration fallback.
2. **Snapshot/restore correctness during the port** (rory-class incidents).
   Mitigated by bidirectional compat tests, never by optimism.
3. **Async Rust velocity.** tokio+axum is productive but the team thinks in
   Go today; budget learning curve into phases 2–3, not zero.
4. **Upstream drift.** The Go fork keeps moving during the rewrite: either
   freeze feature work on Go (recommended) or pay a weekly rebase.
5. **Guest-agent size discipline.** Rust binaries bloat fast; the CI budget
   is the enforcement, not good intentions.

## 5. Effort

- ~6–9 engineer-months for one strong Rust dev (incl. KVM/virtio learning),
  ~3–4 months with two devs in parallel (engine+daemon split after phase 1).
- Excludes ongoing libkrucible fork maintenance (unchanged either way).

## 6. First step after PR #2 merges

Phase 0 only: record protocol vectors + land the conformance harness on Go.
Nothing gets rewritten until the harness is green — that harness is what makes
the rest safe to do.

## 7. Adopted decisions (from parallel design probes)

- **Backends: trait-abstract now, ship krucible-only.** The Go `Engine`
  interface is already backend-neutral with optional capabilities and opaque
  snapshot manifests — ~1 week to formalize (generalize dead FC state
  columns, per-engine capability gating, per-OS selection). Resurrecting
  Firecracker now would cost ~4–8 weeks for a Linux-only backend with
  disjoint snapshots/networking and zero macOS story, doubling the test
  matrix with no user-visible payoff. krucible stays the preferred and only
  backend; Firecracker returns only on real hostile-multitenancy demand.
  Structurally: Firecracker can never do macOS/HVF, so any dual-backend
  future keeps krucible as the dev-machine path regardless.
- **Durability: local NVMe hot path + S3 async backup (Sprites-style).**
  Full `memory.img` + qcow2 deltas chunked content-addressed (Rabin CDC +
  SHA-256, ~2 MB avg), manifests with strict compat gates (arch, VMM,
  kernel), background push on cold-stop, parallel prefetch on wake,
  refcounted GC, per-chunk XChaCha20-Poly1305. Rollout: single-node backup
  → prefetch-on-wake → multi-node restore. S3 is the durability story that
  makes multi-node and the dashboard's snapshot views real.
- **API: dashboard-first.** Cursor pagination + filters on every list (never
  another unbounded array), one typed event bus (queryable table + SSE
  stream; axum WS only for shells), OpenAPI generated from code and
  CI-gated, per-sandbox detail embedding thermal/activity/ports/rules so a
  detail view costs one call. Auth stays header-keys (no CSRF surface);
  harden WS origins before any browser ships. Drop CLI-only cruft
  (`/_shell` page, proxy path surgery, name-vs-ID ambiguity) from the v2 API.
