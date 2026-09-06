# STATUS — Rust rewrite, engine phase (2026-09-07 ~01:15 CEST)

Branch: `rust/engine` (pushed). Main is at PR #8 merge (forge sessions+PTY).

## Done

**Merged to main:** scaffold + `ahvm-proto` v2 codec (#4), `ahvm-store` (#5),
`ahvm-forge` exec/files/auth (#6), store review fixes (#7), forge
sessions+PTY (#8). 42 tests green, clippy clean, macOS + `ahvm-node-01`.

**On `rust/engine` (this branch, pushed):**
- `ahvm-vmm` worker: native libkrun link (no cgo), spec-driven boot,
  `create-overlay`. Builds release on server (6.6 MB). Lessons recorded in
  code: link via `krun = { package = "libkrun" }` re-export (bare extern
  links nothing); pin `imago =0.2.3` + `vm-memory =0.17.1` (fresh resolve
  picks an incompatible pair even upstream would break on).
- `ahvm-engine` core (subagent): `Backend` trait + capabilities, neutral
  spec/info/thermal types, v2 `SnapshotManifest` + compat gate, worker
  manager, mock backend. 19 unit tests.
- Forge vsock transport (subagent): `Conn` enum, `VsockListener`, tests green.
- **PID1 fix (real product bug):** guest forge `exit(1)` on TCP bind failure
  killed PID 1 and halted every VM within seconds. Now exits only if BOTH
  transports fail. Proven: workers stay alive 30+ min.
- Review round 1 (all fixed): null-env leak → explicit empty env; vsock fds
  without CLOEXEC → `SOCK_CLOEXEC`/`accept4` (Linux) + fcntl fallback;
  worker zombies → owning `LiveWorker` (kill+reap, reap-if-exited Drop);
  manifest collision → engine sidecar is `ahvm-manifest.json` (libkrun owns
  `manifest.json`; coexistence + missing-sidecar-refused tests).
- Review round 2 (all fixed): reap-then-signal race → `waitid(P_PID,
  WEXITED|WNOWAIT)` observe-without-reap + fail-closed `Observed` enum
  (never signal unreserved); PTY tail loss on WouldBlock → wait-for-readiness
  in pumps + deterministic unit test; lock-order deadlock (writer-restore vs
  `delete`) → snapshot-then-act ordering; concurrent-create over-limit →
  placeholder reservation under map lock.
- `scripts/rust-guest-rootfs.sh`: reproducible guest image (musl forge as
  `/init.krun` + pinned BusyBox 1.37.0 static + links). Gotcha recorded in
  script: `ldd` exits 1 on static binaries, so under `pipefail` ANY pipeline
  containing ldd reports failure — match on captured text, never on status.
- `ahvm-engine/tests/kvm_lifecycle.rs` (KVM-gated, skips without
  `AHVM_KVM_TEST=1`): boot → `/bin/sh -c 'printf hello'` smoke + stderr/exit
  cases → qcow2 overlay → session RAM marker + fs file → sidecar write →
  3× kill/restore cycles with compat gate + tamper-negative → zombie checks.

## Proven working live (server)

Rust worker boots microVMs; guest forge serves exec/files over bridged
vsock; smoke asserts pass (`printf hello` → exit 0; exit-3 case; file list).
Snapshot writes a real bundle (572 MB memory.img + checkpoint.bin + both
manifests); restore logs `cold restore: resumed from snapshot`.

## NOT done / blocked

1. **KVM gate does not pass end-to-end yet.** Boot under `cargo test`
   hangs pre-guest (~60s budget) while the same binary/spec/overlay boots
   from bash in ~10–30s. Hermetic spawn (`env_clear`) fixed *boot reach*
   (bridges come up, frames reach the guest) but exec then times out.
   Opened as **issue #10** (env hang). Bisected hard: NOT single env var,
   CWD, stdio, contention, cgroups, caps, seccomp, AppArmor, overlay file.
   Prime suspects left: (a) early exec frames (wait_ready polls from t+1s,
   before forge listens) poison per-port muxer proxy state — vsock `id`
   reuse across connections looks racy; (b) restored-guest vsock RX stall
   (frames pushed + IRQ raised, guest never replies).
2. **imago HashMap nondeterminism (real, proven):** `create-overlay` emits
   different header bytes run-to-run (feature-name table is a `HashMap`,
   random order — strings like `dirty`/`corrupt` in output). Valid qcow2
   either way; harmless for boot, POISON for future chunk-dedup (S3 plan).
   Fix options: upstream report, or post-create normalization. NOT the boot
   hang (hung instances boot fine manually).
3. **Issue #3** (Go vsock config-fetch flake): still open by design; the
   deterministic SIGKILL→relaunch regression test belongs to the KVM gate
   above — blocked behind (1).
4. Daemon (`ahvm-daemon`), netd, CLI: not started (Phase 4–6 of
   `docs/PLAN-rust-rewrite.md`).

## Next session, in order

1. Repro the exec-timeout on a KNOWN-good manual worker vs test worker with
   identical timing (delay first exec 20s in test) — separates "early-dial
   poisoning" from "restore/deep" causes. If early-dial: fix is wait-for-ready
   via control-socket/console marker instead of exec polling.
2. If restore-side: `perf kvm_entry` on restored worker (0 entries =
   vCPUs never resume → fork resume bug; ticking = vsock RX bug).
3. Land green KVM gate → PR → `rust/daemon` branch.
4. Report imago HashMap ordering upstream (with det-*.qcow2 repro).

## Hygiene notes (learned the hard way)

- `pkill -f <pattern>` matches its own ssh command line and kills the
  session (exit 255, no output). Use exact PIDs or `[b]racket` patterns.
- Tool transport expands `$VAR` in commands (incl. unquoted heredocs).
  Quote heredoc delimiters; prefer python/printf-built scripts.
- `git checkout -B` without a preceding `fetch` silently pins stale refs;
  always fetch first (bit us 3 times tonight).
- Overlapping background `cargo test` runs poison all observations; run KVM
  tests strictly one at a time on a clean box.
