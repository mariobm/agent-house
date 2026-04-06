# Fix: Snapshot/Resume Reliability — 10 Bugs + 4 New Findings

Root cause: Sandbox "rory" (with a persistent volume) failed to resume
after a daemon restart on Apr 5. Traced to missing `volume_attachments`
record — the sandbox was created via snapshot resume, which doesn't
persist volume attachment info.

Trace: `docs/SNAPSHOT-RELIABILITY-TRACE.md`
Tests: already merged, expect 6 failures before fixes.

---

## Summary

The reliability trace walked every step of the stop → snapshot → restart →
resume flow and found 10 bugs. Code review confirmed 9 of 10 and found 4
additional issues. Bug #9 (no fsync) was downgraded to NEGLIGIBLE.

### Bug inventory

| # | Bug | Severity | Where |
|---|-----|----------|-------|
| 1 | `ResumeSnapshot` doesn't populate `vm.Volumes`; `handleSnapshotResume` doesn't create `volume_attachments` | **CRITICAL** | snapshot.go, admin_handlers.go |
| 2 | `Stop()` doesn't return error on snapshot artifact move failure | HIGH | lifecycle.go |
| 3 | `Stop()` doesn't return error on `verifySnapshotArtifacts` failure | HIGH | lifecycle.go |
| 4 | IP pool corruption on `TryAllocate` failure in `ResumeSnapshot` | MEDIUM | snapshot.go |
| 5 | `SaveImage` on cold sandbox hits "connection refused" | MEDIUM | helpers.go |
| 6 | Symlink race in bare-mode resume (latent — jailed mode unaffected) | LOW | lifecycle.go, snapshot.go |
| 7 | Checkpoint errgroup doesn't cancel on failure | LOW | snapshot.go |
| 8 | `recoverVMs` drive_id ordering not guaranteed | LOW | main.go |
| 9 | No fsync after snapshot writes | NEGLIGIBLE | — |
| 10 | Thermal manager blocks on `stateMu` contention | LOW | server.go |
| N1 | `ResumeSnapshot` doesn't populate `vm.Volumes` (distinct from store bug) | **CRITICAL** | snapshot.go |
| N2 | `Destroy()` unlocks `stateMu` before `os.RemoveAll` — race with `EnsureHot` | MEDIUM | lifecycle.go |
| N3 | `handleSnapshotResume` sets `CreatedBy` to user B but engine has `UserID` from manifest (user A) | LOW | admin_handlers.go |
| N4 | `SnapshotAll` calls `UpdateSandboxStatus` instead of `StopSandbox` (no `stopped_at`) | LOW | server.go |

### Test coverage (already merged)

| Test file | Tests | Runs on |
|-----------|-------|---------|
| `snapshot_reliability_test.go` | 10 integration tests (jailed FC) | Pi cluster |
| `ippool_reliability_test.go` | 3 unit tests (pool invariants) | Pi cluster |
| `recovery_reliability_test.go` | 7 unit tests (SQLite + mock) | Every PR (CI) |

**6 tests fail before fixes** — they exercise real code paths, not
simulated bugs. After all fixes, all 20 pass.

---

## Phase 1: Critical — Volume persistence through snapshot resume

Root cause of the rory incident. Fixes Bug #1 and New Bug N1.

### 1.1 Populate `vm.Volumes` in `ResumeSnapshot`

**`pkg/engine/firecracker/snapshot.go` — `ResumeSnapshot()`**

After building the VM struct (~line 418), populate `Volumes` from the
manifest drives:

```go
// Build volume attachment info from manifest
var volumes []VolumeAttachmentInfo
for _, d := range manifest.Drives {
    if d.Role == "volume" {
        volumes = append(volumes, VolumeAttachmentInfo{
            DriveID:  d.DriveID,
            Name:     d.Name,
            FilePath: filepath.Join(sandboxDir, d.SnapshotFile),
            ReadOnly: d.ReadOnly,
        })
    }
}
```

Then in the `VM{}` literal, add `Volumes: volumes`.

The `Mount` field is missing from `ManifestDrive`. Two options:

**Option A** — add `Mount` to `ManifestDrive`. This requires adding
`Mount string `json:"mount,omitempty"`` to the struct, populating it in
`Checkpoint()` from `vol.Mount`, and reading it in `ResumeSnapshot()`.
Existing snapshots on disk won't have the field — `Mount` will be empty
string. That's acceptable: the mount point is in the config drive which
is already part of the snapshot. The field is only needed for the host-side
`vm.Volumes` metadata used by `startVM()` jail path resolution (which keys
on `Name` and `FilePath`, not `Mount`).

**Option B** — leave `Mount` empty. `startVM()` only uses `vol.Name` and
`vol.FilePath` from `vm.Volumes` to build jail paths. `Mount` isn't used
on the resume path (it's baked into the guest config drive). This is
simpler and correct.

**Decision: Option A** — add `Mount` to the manifest for completeness.
Even though `startVM` doesn't use it, the recovery path (`recoverVMs`)
needs `Mount` to rebuild `VolumeAttachmentInfo` from the DB. Keeping it
in the manifest means `VMState()` returns complete info for `saveVMState`.

### 1.2 Create `volume_attachments` in `handleSnapshotResume`

**`pkg/server/admin_handlers.go` — `handleSnapshotResume()`**

After `s.store.CreateSandbox(sb)` and before `s.saveVMState(sbID, ...)`,
iterate manifest drives, look up volumes, and create attachments:

```go
// Create volume_attachments for any volumes in the snapshot.
// Without this, recoverVMs won't find volumes after daemon restart.
for _, d := range m.Drives {
    if d.Role != "volume" || d.Name == "" {
        continue
    }
    vol, err := s.store.GetPersistentVolume(user.ID, d.Name)
    if err != nil {
        slog.Warn("snapshot resume: volume not found in store",
            "volume", d.Name, "sandbox", sbID)
        continue
    }
    mount := d.Mount
    if mount == "" {
        mount = "/vol-" + d.Name // fallback if old snapshot lacks mount
    }
    if err := s.store.AttachPersistentVolume(
        user.ID, d.Name, sbID, mount, d.ReadOnly,
    ); err != nil {
        slog.Warn("snapshot resume: attach volume failed",
            "volume", d.Name, "sandbox", sbID, "error", err)
    }
}
```

**Edge case:** The volume might not exist in the store of the *resuming*
user. If user A checkpointed with volume "data", and user B resumes, B
might not have a volume named "data". In that case we log a warning and
continue — the snapshot has a copy of the volume data in its snapshot dir,
so the VM will work. It just won't have a `volume_attachments` record,
which means the next stop/start requires the snapshot copy (not the live
volume).

**Edge case:** `AttachPersistentVolume` does concurrency checks (no
double RW attach). Since the resumed sandbox is brand new and the volume
wasn't attached to anything, this should always succeed. If it fails
(volume already RW-attached to another sandbox), we warn and continue.

### 1.3 Add `Mount` to `ManifestDrive`

**`pkg/engine/firecracker/snapshot.go`**

```go
type ManifestDrive struct {
    DriveID      string `json:"drive_id"`
    Role         string `json:"role"`
    SnapshotFile string `json:"snapshot_file"`
    Name         string `json:"name,omitempty"`
    ReadOnly     bool   `json:"read_only"`
    Mount        string `json:"mount,omitempty"` // NEW
}
```

In `Checkpoint()`, when building volume drives (~line 167):

```go
drives = append(drives, ManifestDrive{
    DriveID: vol.DriveID, Role: "volume", SnapshotFile: snapFile,
    Name: vol.Name, ReadOnly: vol.ReadOnly,
    Mount: vol.Mount,  // NEW
})
```

### Tests that will pass after this phase

- `TestSnapshotResumeVolumeStopStart`
- `TestSnapshotResumeVolumeInVMState`
- `TestSnapshotResumeNestedCheckpointPreservesVolumes`
- `TestJailedVolumeSnapshotFullCycle`
- `TestRecoverVolumeAttachments` (already passes — tests the store path)

---

## Phase 2: High — Stop() returns errors on artifact failure

Fixes Bug #2 and Bug #3. Without these, Stop() kills the FC process even
when it knows the snapshot is bad — unrecoverable data loss.

### 2.1 Return error on snapshot move failure

**`pkg/engine/firecracker/lifecycle.go` — `Stop()`**

Current code (~line 88):

```go
if err := copyBlock(src, dst); err != nil {
    slog.Error("move snapshot from chroot", "file", name, "error", err)
}
```

Change to:

```go
if err := copyBlock(src, dst); err != nil {
    slog.Error("move snapshot from chroot", "file", name, "error", err)
    return fmt.Errorf("move snapshot %s from chroot: %w", name, err)
}
```

The VM stays paused (alive). The caller can retry on the next thermal
cycle, or the user can manually `bhatti stop` with disk space freed.

### 2.2 Return error on verify failure

**`pkg/engine/firecracker/lifecycle.go` — `Stop()`**

Current code (~line 101):

```go
if err := verifySnapshotArtifacts(vm.SnapVMPath, vm.SnapMemPath, vm.MemSizeMib, snapshotType); err != nil {
    slog.Error("snapshot sanity check failed", "sandbox", id, "error", err, "type", snapshotType)
}
```

Change to:

```go
if err := verifySnapshotArtifacts(vm.SnapVMPath, vm.SnapMemPath, vm.MemSizeMib, snapshotType); err != nil {
    return fmt.Errorf("snapshot sanity check failed: %w", err)
}
```

Same rationale: don't cross the point of no return (killFC) with a
known-bad snapshot.

### Tests that will pass after this phase

- `TestStopSnapshotArtifactsValid` (already passes — happy path)

No new integration test needed for the error paths since they require
injecting disk-full or I/O errors. The code fix is two lines, and the
existing tests verify the happy path. The critical property is that the
`return` exists before `killFC`.

---

## Phase 3: Medium — IP pool corruption

Fixes Bug #4. After a failed `TryAllocate`, the cleanup defer releases
an IP that was never allocated — freeing another sandbox's IP.

### 3.1 Set `guestIP` after `TryAllocate` succeeds

**`pkg/engine/firecracker/snapshot.go` — `ResumeSnapshot()`**

Current code (~line 301):

```go
guestIP = manifest.Network.GuestIP
if err = userNet.Pool.TryAllocate(guestIP); err != nil {
    return info, fmt.Errorf("IP %s required by snapshot is in use: %w", guestIP, err)
}
```

Change to:

```go
if err = userNet.Pool.TryAllocate(manifest.Network.GuestIP); err != nil {
    return info, fmt.Errorf("IP %s required by snapshot is in use: %w",
        manifest.Network.GuestIP, err)
}
guestIP = manifest.Network.GuestIP
```

One line moved. The cleanup defer checks `if guestIP != ""` before
calling `Release` — with `guestIP` still empty on TryAllocate failure,
the Release is skipped.

### Tests that will pass after this phase

- `TestResumeIPConflictNoPoolCorruption`

---

## Phase 4: Medium — SaveImage on cold sandboxes

Fixes Bug #5. `SaveImage` doesn't check thermal state, tries to resume
a dead FC process, returns "connection refused".

### 4.1 Reject cold sandboxes early

**`pkg/engine/firecracker/helpers.go` — `SaveImage()`**

Add after the `stateMu.Lock()` (~line 30):

```go
if vm.Status != "running" {
    return fmt.Errorf("sandbox %q is stopped — start it first", sandboxID)
}
```

This catches both cold (stopped, no FC process) and unknown states.
The error message is clear and actionable.

### Tests that will pass after this phase

- `TestSaveImageOnColdSandbox`

---

## Phase 5: Medium — Destroy race with EnsureHot

Fixes New Bug N2. `Destroy()` unlocks `stateMu` before `os.RemoveAll`,
creating a window where `EnsureHot` can read the VM from the map and
try to start it while files are being deleted.

### 5.1 Delete from `e.vms` before filesystem cleanup

**`pkg/engine/firecracker/lifecycle.go` — `Destroy()`**

Current order:

```go
rootfsDir := filepath.Dir(vm.RootfsPath)
vm.stateMu.Unlock()
os.RemoveAll(rootfsDir)

e.mu.Lock()
delete(e.vms, id)
e.mu.Unlock()
```

Change to:

```go
rootfsDir := filepath.Dir(vm.RootfsPath)

// Remove from map FIRST — prevents getVM() from finding a VM
// whose files are being deleted. Must hold stateMu while doing
// this so no concurrent operation is mid-flight on this VM.
e.mu.Lock()
delete(e.vms, id)
e.mu.Unlock()

vm.stateMu.Unlock()

// Now safe to delete files — no other goroutine can reach this VM
os.RemoveAll(rootfsDir)
```

After the `delete(e.vms, id)`, any concurrent `getVM(id)` returns
"not found" immediately. The `stateMu.Unlock()` then allows any
goroutine that was *already past* `getVM` but blocked on `stateMu`
to proceed — but since we're inside Destroy, the VM state is already
"destroyed" (cmd killed, TAP cleaned). Those goroutines will get errors
from agent calls (connection refused) which is correct.

### Tests

No new integration test — this is a race that's hard to trigger
deterministically. The fix is a reordering of two operations.

---

## Phase 6: Low — Cleanup and quality

Fixes bugs #7, #8, #10, N3, N4. None of these cause data loss in current
production, but they're cheap to fix and prevent future issues.

### 6.1 Checkpoint errgroup cancellation (Bug #7)

**`pkg/engine/firecracker/snapshot.go` — `Checkpoint()`**

Current (~line 178):

```go
g, _ := errgroup.WithContext(ctx)
```

Change to:

```go
g, gCtx := errgroup.WithContext(ctx)
```

And in the goroutine, pass `gCtx` to `copyBlock` — but `copyBlock` uses
`exec.Command("cp", ...)` which doesn't take a context. Wrap it:

```go
g.Go(func() error {
    dst := filepath.Join(tmpDir, c.dstFile)
    cmd := exec.CommandContext(gCtx, "cp", "--reflink=auto",
        "--sparse=always", c.src, dst)
    return cmd.Run()
})
```

This requires a new function or inlining the `cp` call. Simplest: add
`copyBlockCtx`:

```go
func copyBlockCtx(ctx context.Context, src, dst string) error {
    return exec.CommandContext(ctx, "cp", "--reflink=auto",
        "--sparse=always", src, dst).Run()
}
```

Use it in `Checkpoint()` only. The existing `copyBlock` stays unchanged
for call sites that don't need context cancellation.

### 6.2 Recovery drive_id ordering (Bug #8)

**`pkg/store/volume.go` — `AttachedPersistentVolumesForSandbox()`**

Add `ORDER BY` to the query:

```sql
SELECT v.name, v.file_path, va.mount, va.read_only
FROM volume_attachments va
JOIN volumes_v2 v ON v.id = va.volume_id
WHERE va.sandbox_id = ?
ORDER BY va.rowid
```

`rowid` preserves insertion order, which matches the original
`AttachPersistentVolume` call order during sandbox creation. This
is deterministic across restarts.

### 6.3 SnapshotAll uses StopSandbox (Bug N4)

**`pkg/server/server.go` — `SnapshotAll()`**

Change:

```go
s.store.UpdateSandboxStatus(sb.ID, "stopped")
```

To:

```go
s.store.StopSandbox(sb.ID)
```

This sets `stopped_at` timestamp correctly.

### Tests

- `TestRecoveryMultiVolumeOrdering` — already exists, tests ordering
  through VMState → RestoreVM → Start.

No new tests for 6.1, 6.3 — they're one-line fixes with obvious
correctness.

---

## Phase 7: Additional tests for ongoing reliability

Tests that don't target specific bugs but harden the snapshot path
against regressions.

### 7.1 Server-level snapshot resume with volumes

The current integration tests exercise the engine layer directly. We need
a test that goes through the server's `handleSnapshotResume` to verify the
`volume_attachments` creation (Phase 1.2).

**`pkg/server/snapshot_resume_test.go`** — mock engine, real store:

- `TestHandleSnapshotResumeCreatesVolumeAttachments` — POST
  `/snapshots/name/resume`, verify `AttachedPersistentVolumesForSandbox`
  returns the volume.
- `TestHandleSnapshotResumeVolumeNotInStore` — volume in manifest but
  not in store → resume succeeds, warning logged, no attachment created.

### 7.2 End-to-end daemon restart simulation

**`pkg/engine/firecracker/snapshot_reliability_test.go`** — new test:

`TestSnapshotResumeVolumeRecoveryRoundTrip`:
1. Create sandbox with volume, write data
2. Checkpoint
3. Destroy original
4. ResumeSnapshot
5. Get VMState → save to a map
6. Delete from engine (simulate restart)
7. RestoreVM with saved state
8. Start
9. Verify volume data

This tests the full VMState → RestoreVM → Start path for a
snapshot-resumed sandbox, catching any field that doesn't round-trip.

### 7.3 Stress: repeated stop/start with volume

`TestVolumeDataSurvivesRepeatedStopStart` — write data, then
stop/start 5 times, verify data after each cycle. Catches any
accumulation bugs (path growth, stale jail state, etc.).

---

## Implementation Order

```
Phase 1 (volume persistence)  ←── root cause, 6 tests depend on this
  ↓
Phase 2 (Stop error returns)  ←── independent, 2-line fix
  ↓
Phase 3 (IP pool fix)         ←── independent, 1-line fix
  ↓
Phase 4 (SaveImage guard)     ←── independent, 2-line fix
  ↓
Phase 5 (Destroy race)        ←── independent, reorder
  ↓
Phase 6 (cleanup)             ←── independent, low priority
  ↓
Phase 7 (new tests)           ←── after all fixes land
```

Phases 2-5 are independent of each other and can be done in any order
or in parallel. Phase 1 must land first because 6 integration tests
depend on it.

### Estimated effort

| Phase | Lines changed | Time | Risk |
|-------|---------------|------|------|
| 1 | ~40 (snapshot.go, admin_handlers.go) | 1hr | Medium — manifest schema change |
| 2 | 4 (lifecycle.go) | 10min | None |
| 3 | 2 (snapshot.go) | 5min | None |
| 4 | 3 (helpers.go) | 5min | None |
| 5 | 6 (lifecycle.go) | 10min | Low — reordering |
| 6 | ~15 (snapshot.go, volume.go, server.go) | 30min | None |
| 7 | ~200 (new test files) | 1hr | None |
| **Total** | **~270** | **~3hr** | |

### What's NOT in this plan

**Bug #6 (bare-mode symlink race)** — production runs jailed mode.
Bare mode is dev-only. Not worth fixing.

**Bug #9 (no fsync)** — power loss in a sub-second window on a server
with UPS. Negligible risk. Not worth the complexity.

**Bug #10 (thermal stateMu contention)** — self-healing (next cycle
retries). Would require TryLock which Go's sync.Mutex doesn't have
without unsafe tricks or a custom lock. Not worth it for 13 sandboxes.

**Per-VM network namespaces** — listed in PLAN-production-hardening.md
as Phase 2 of jailer integration. Out of scope for reliability fixes.

**Configurable agent WaitReady timeout** — the trace suggests 30s is
too long. True, but changing it is a UX decision, not a reliability fix.
Ship separately.

### What to ship as-is vs what blocks v1.0

**Ship now (no fixes needed):**
- Thermal state machine (hot/warm/cold transitions)
- Jailed mode path handling
- Circuit breaker pattern
- TAP preservation across stop/start
- Recovery verification (file existence + size checks)
- Graceful shutdown ordering
- Network isolation per user

**Must fix before v1.0:**
- Phase 1 (volume persistence) — data loss on restart
- Phase 2 (Stop error returns) — data loss on disk errors
- Phase 3 (IP pool corruption) — network corruption

**Should fix before v1.0:**
- Phase 4 (SaveImage guard) — confusing UX
- Phase 5 (Destroy race) — rare but real

**Can ship after v1.0:**
- Phase 6 (cleanup) — quality of life
- Phase 7 (new tests) — defense in depth
