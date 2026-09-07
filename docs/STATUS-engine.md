# Rust engine status — 2026-09-07

PR #9: `rust/engine`. Recovery tracking: [issue #11](https://github.com/mariobm/agent-house/issues/11).

## KVM recovery findings and fixes

The failure was user-visible: a sandbox restored into a fresh worker stopped
answering exec requests. A successful snapshot write or a
`cold restore: resumed` log did not prove that the sandbox was usable.

Two VMM defects combined to hide the cause:

- The x86 MSR snapshot allowlist omitted `IA32_XSS`. After restore, the guest
  faulted in `restore_fpregs_from_fpstate` and eventually panicked. Preserving
  this supervisor extended-state mask fixes execution on `agent_house`.
  Restore now also rejects a partial `KVM_SET_MSRS` result.
- Active console port threads owned queues that the snapshot did not save.
  Restore did not restart those ports, so kernel diagnostics could stall.
  Snapshot now stops and joins console workers, collects their queue state,
  records open ports, and restarts ports after snapshot or restore.

The earlier missing-LAPIC theory was incorrect: `VcpuState` already saves and
restores LAPIC state. The Go test failing before guest readiness was not
proof of a Go restore failure. Cargo environment was not the restore cause.

Checkpoint format and engine device layout are now version **2**. Old
snapshots are intentionally refused; create new snapshots with this worker.
The VMM dependency lives in private `mariobm/libkrucible`, pinned by the
submodule commit. Its upstream remains `sahil-shubham/libkrucible`.

## Regression coverage

`AHVM_KVM_TEST=1` now always tests restore. There is no second restore opt-in.
Missing KVM or required paths fail an explicitly enabled run.

The lifecycle test covers:

- Native Rust worker boot, shell stdout, stderr and nonzero exit status.
- Snapshot, resume, SIGKILL and reap, then a fresh worker for each restore.
- Command execution, console output and a pre-snapshot session RAM marker.
- Disk rollback: copy the qcow2 overlay while the VM is paused, overwrite
  the live disk after snapshot, restore a separate copy, drop guest file
  caches and check the original content.
- Engine sidecar compatibility and actual worker refusal of an old checkpoint.

**Disk ownership matters:** libkrun's `SNAPSHOT` writes RAM and device/CPU
state and leaves vCPUs paused. The caller must copy the root overlay at that
boundary and restore a disposable copy. Reusing the live overlay is not a
persistent disk snapshot. The base image must remain unchanged.

On `ssh agent_house`, the strengthened test passed **10 cycles with 2 vCPUs**
and a 2-second delay before each restore (23.96 seconds on the rebuilt image). A single-vCPU
three-cycle run also passed. The guest image was subsequently rebuilt from
verified BusyBox source using the checked-in script.

Validation: 64 Rust workspace tests passed, including the enabled KVM gate;
workspace/all-target Clippy passed with warnings denied. The VMM dependency
passed 61 device, 27 architecture and 29 VMM tests (one existing ignored),
and Clippy with `--no-default-features --features blk,net`. Optional GPU,
SEV/TDX and other host architectures were not validated by this run.

## Run on the server

Prerequisites: `/dev/kvm`, Rust and the native Linux musl target, a C toolchain,
make, curl, bzip2, binutils, e2fsprogs, and libkrunfw in `/usr/local/lib64`.
The rootfs builder does not need root or mounts. It verifies BusyBox 1.37.0
against a fixed SHA-256 on either download mirror and checks both binaries
for a dynamic loader. It does not install or silently upgrade host packages.

```sh
cd /root/agent-house
export PATH="$HOME/.cargo/bin:$PATH"
scripts/rust-guest-rootfs.sh /tmp/kvm/rust-guest.ext4
make -C libkrucible FEATURE_FLAGS='-p libkrun --no-default-features --features blk,net'
cargo build --manifest-path rust/Cargo.toml --release --locked -p ahvm-vmm
LD_LIBRARY_PATH=/usr/local/lib64 \
AHVM_KVM_TEST=1 AHVM_KVM_CYCLES=10 AHVM_KVM_VCPUS=2 \
AHVM_VMM_BIN="$PWD/rust/target/release/ahvm-vmm" \
AHVM_GUEST_IMAGE=/tmp/kvm/rust-guest.ext4 \
cargo test --manifest-path rust/Cargo.toml -p ahvm-engine --test kvm_lifecycle -- --nocapture
```

`AHVM_KVM_RESTORE_DELAY_SECS` optionally adds downtime between cycles.
`AHVM_KVM_READY_SECS` changes the readiness budget. Logs and bundles remain
under `/tmp/ahvm-kvm-<test-pid>` for inspection. Run KVM tests serially.

## Scope still remaining

PR #9 provides the worker, engine types, compatibility sidecar, supervision,
mock backend, and real KVM lifecycle validation. Branch `rust/engine-backend`
adds the missing Phase 3 piece: a real krucible `Backend`
(`rust/ahvm-engine/src/krucible.rs`) — create/exec/stop/start, online
snapshot + frozen-overlay registry, compat-gated restore/fork, hermetic
worker spawn, and crash recovery by pid adoption (`open` re-adopts live
`state.json` pids; dead ones surface `Failed`). Gated by
`tests/kvm_backend.rs`: full trait flow plus drop-and-reopen adoption and
adopted destroy, green on KVM in ~2s. Daemon, netd and CLI are still later
phases; thermal manager/scheduler ride with the daemon (agreed order:
backend → daemon API → thermal/scheduler → acceptance test; Phases 5/6
separate; Go retired only after Rust passes conformance).

Submodule note: main pins libkrucible `87f1dfa`, a tree-identical re-commit
of `e9dcedc` (VMM PR #1). Hosts whose deploy key cannot fetch the fork can
keep building from `e9dcedc` with zero functional difference; run
`git submodule update` where network allows to clear the dirty flag.

Review round on PR #12 (all fixed on `rust/engine-backend`): per-sandbox
operation reservations (second op on a busy id fails fast with Conflict);
pid-identity adoption via recorded `/proc` starttime with reuse-safe
adopted signalling; generational snapshot publish (bundle.new → rename
swap, registry tmp → rename, crash-debris sweep on open); stored-bundle-
authoritative restore (caller manifest is only a lookup handle);
split connect vs 300s exec budgets; guest `truncated` flag preserved;
start() reconciles liveness instead of trusting cache (plus a start()
contract: out-of-band killers must observe death via status() first —
kill/reap race is indistinguishable from alive for any supervisor);
custom `kernel_image` explicitly rejected. Verification caught one more
own bug along the way: the reconcile edit briefly double-locked the map
(self-deadlock, found via gdb futex trace); single-scope fix, KVM gate
green since (backend 18s incl. 16s slow-exec, lifecycle 1.5s).

The imago qcow2 feature-name table has nondeterministic byte ordering. It
does not break disk correctness; normalization remains future dedup work.
Issue #3's Go config-fetch path remains separate from the Rust regression.

PR #12 follow-up hardening: reservations now have a dedicated mutex, so
completion cannot leak a reservation when the sandbox map is contended.
Snapshot publication flushes artifacts and directory entries; reopening
recovers `bundle.old` when a crash interrupted the two-rename publish,
then discards the uncommitted generation. Registry copies are flushed
before publication too.

Adopted-worker signalling on Linux now opens a pidfd, verifies the recorded
starttime after opening it, and sends SIGTERM through that handle. PID reuse
between verification and signalling cannot redirect the signal. Kernels
without pidfd support return an error; there is no numeric-PID fallback.
Live records without a starttime refuse adoption with InvalidState rather
than being marked dead (which could start a duplicate VM). This includes
legacy records and current macOS records. Stop these workers through the
supervisor that owns them before upgrading/reopening; do not delete their
records while they are alive. Owned-worker supervision remains supported
on macOS; safe adoption there needs a separate platform implementation.
