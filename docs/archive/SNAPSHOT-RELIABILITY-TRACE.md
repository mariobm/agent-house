# Snapshot Reliability Trace

Step-by-step trace of every operation in the stop → snapshot → restart →
resume flow, with failure analysis for each step.

---

## Flow 1: Stop() — Hot/Warm → Cold (snapshot to disk)

Called by: thermal manager (warm→cold), `SnapshotAll` (shutdown), user
`bhatti stop`.

**Source:** `lifecycle.go:Stop()`

### Step 1: Get VM and lock

```go
vm, err := e.getVM(id)
vm.stateMu.Lock()
```

**Failure:** VM not found → returns error. Lock contention if `Checkpoint()`
or another `Stop()` is running → blocks until released. The thermal manager
blocks on this, stalling the cycle for other VMs.

**Issue:** No try-lock. The thermal manager should skip contended VMs rather
than blocking. Currently if `Checkpoint()` takes 30 seconds (large disk
copy), the thermal cycle thread is blocked for all other VMs.

### Step 2: Flush guest page cache

```go
if vm.Thermal == "hot" && vm.Agent != nil {
    syncCtx, syncCancel := context.WithTimeout(context.Background(), 10*time.Second)
    vm.Agent.Exec(syncCtx, []string{"sync"}, nil, "")
    syncCancel()
}
```

**Failure:** Agent unreachable (process crashed inside VM, network issue) →
10s timeout → logged as warning, continues. This is correct — a stale
snapshot is better than no snapshot.

**Issue:** `syncCancel()` is called after `Exec` returns, but the deferred
cancel pattern would be cleaner. Not a bug, just style.

### Step 3: Pause VM (if hot)

```go
if vm.Thermal != "warm" {
    pauseCtx, pauseCancel := context.WithTimeout(ctx, 5*time.Second)
    defer pauseCancel()
    if err := fcPatch(pauseCtx, client, "/vm", `{"state":"Paused"}`); err != nil {
        return fmt.Errorf("pause: %w", err)
    }
}
```

**Failure:** FC API unreachable (process died between getVM and now) → 5s
timeout → returns error. **The VM is still hot but the caller thinks Stop
failed.** Correct behavior — caller can retry.

**Failure:** Already paused (warm) → skip pause. Correct.

**Failure:** Context from caller already expired → immediate error. Correct.

### Step 4: Create Full snapshot

```go
if err := fcPut(ctx, client, "/snapshot/create", fmt.Sprintf(
    `{"snapshot_type":%q,"snapshot_path":%q,"mem_file_path":%q}`,
    snapshotType, snapVMRef, snapMemRef)); err != nil {
    return fmt.Errorf("create %s snapshot: %w", snapshotType, err)
}
```

**Failure:** FC returns error (disk full, I/O error, internal FC error) →
returns error. **VM is paused but not killed.** The caller gets an error,
and the thermal manager's retry logic will try again next cycle. The VM
stays warm (paused), which is correct.

**Failure:** Context timeout → snapshot may be partially written. The FC
process is still alive (paused). The partial mem.snap/vm.snap files exist
on disk. **No cleanup of partial files.** On the next `Stop()` attempt,
`os.Remove` isn't called on the old files before the new snapshot — FC
will overwrite them (it uses O_CREAT|O_TRUNC). So this is actually OK.

**Issue (jailed mode):** In jailed mode, FC writes to chroot-relative
paths (`/vm.snap`, `/mem.snap`). If the snapshot fails partway, these
partial files sit in the jail. The next resume attempt via `startVM()`
will call `startFCJailed()` which `os.RemoveAll()`s the entire jail dir
first, cleaning up the partial files. So this is also OK.

### Step 5: Move snapshot files from jail to sandbox dir (jailed mode)

```go
if e.cfg.jailed() && vm.jailRoot != "" {
    for _, name := range []string{"vm.snap", "mem.snap"} {
        src := filepath.Join(vm.jailRoot, name)
        dst := filepath.Join(sandboxDir, name)
        os.Remove(dst)
        if err := os.Rename(src, dst); err != nil {
            if err := copyBlock(src, dst); err != nil {
                slog.Error("move snapshot from chroot", ...)
            }
        }
    }
}
```

**Failure:** Rename fails (cross-device) → falls back to copyBlock.
copyBlock fails → **logged as error but not returned.** The function
continues to kill FC. The snapshot files may be missing or partial in
the sandbox dir. On the next `Start()`, `verifySnapshotArtifacts` or
FC itself will detect the problem.

**⚠️ BUG: The copy failure is logged but not returned as an error.**
The function proceeds to kill FC even though the snapshot artifacts
weren't successfully moved. The VM is now dead with no valid snapshot.
Should return an error here, keeping the VM paused (alive) so the user
can intervene.

### Step 6: Verify snapshot artifacts

```go
if err := verifySnapshotArtifacts(vm.SnapVMPath, vm.SnapMemPath, vm.MemSizeMib, snapshotType); err != nil {
    slog.Error("snapshot sanity check failed", ...)
}
```

**Failure:** Verification fails → **logged as error but not returned.**
The function continues to kill FC. Same problem as step 5: the VM is
about to be killed but the snapshot is known to be bad.

**⚠️ BUG: Should return error and keep VM paused.** A corrupt snapshot
that we KNOW is corrupt should not lead to killing the FC process. The
whole point of verification is to catch this before the point of no return.

### Step 7: Kill FC process

```go
killFC(vm.cmd, 3*time.Second)
vm.cancel()
```

**Failure:** SIGTERM ignored → 3s timeout → SIGKILL. Always succeeds.
`vm.cancel()` cancels the context that started the FC process.

**Point of no return.** After this, the VM is dead. Only the snapshot
files can bring it back.

### Step 8: Update VM state

```go
vm.Status = "stopped"
vm.Thermal = "cold"
```

No failure possible — in-memory state update.

**Note:** TAP device is NOT destroyed (comment in code explains why:
snapshot contains virtio-net state referencing the TAP). This is correct.

---

## Flow 2: SnapshotAll() — Graceful shutdown

Called by: SIGTERM handler in `main.go`.

**Source:** `server.go:SnapshotAll()`

### Step 1: List all running sandboxes

```go
sandboxes, err := s.store.ListAllSandboxes()
```

**Failure:** DB error → logs warning, returns. No sandboxes snapshotted.
VMs continue running. When the daemon exits, the kernel kills all
child processes (FC instances). The VMs die without snapshots. On restart,
`recoverVMs` will find sandboxes marked "running" with no valid snapshot
files → marks them "unknown".

### Step 2: Parallel Stop with bounded concurrency (max 10)

```go
err := s.engine.Stop(ctx, sb.EngineID)
if err != nil {
    // Retry once
    err = s.engine.Stop(retryCtx, sb.EngineID)
}
```

**Failure:** First attempt fails → retries once with fresh 60s context.
Good — transient FC API timeouts are common under load.

**Failure:** Retry also fails → **leaves VM running.** Does NOT kill FC.
Correct — an unsnapshotted live VM is better than a dead sandbox with no
snapshot. But when the daemon exits, systemd sends SIGKILL to the cgroup,
killing the FC processes anyway. The VM is lost.

**Issue:** The 60-second timeout per sandbox means 50 VMs at max
concurrency of 10 could take 5 × 60 = 300 seconds. systemd's default
`TimeoutStopSec` is 90 seconds. If SnapshotAll takes too long, systemd
will SIGKILL the daemon before all VMs are snapshotted.

### Step 3: Save state to DB

```go
s.saveVMState(sb.ID, sb.EngineID)
s.store.UpdateSandboxStatus(sb.ID, "stopped")
```

**Failure:** DB write fails → state not persisted. On restart,
`recoverVMs` won't find valid FC state → sandbox marked "unknown".
The snapshot files exist on disk but aren't referenced in the DB.

**Issue:** `saveVMState` does NOT persist `vm.Volumes`. The
`FirecrackerState` struct has no `Volumes` field. Volume info is
recovered from `volume_attachments` table instead. This works IF
the volume attachments were created in the first place (see Bug #1
in the issues section).

---

## Flow 3: recoverVMs() — Daemon startup recovery

Called by: `main.go` at startup.

**Source:** `cmd/bhatti/main.go:recoverVMs()`

### Step 1: List all sandboxes

```go
sandboxes, err := st.ListAllSandboxes()
```

**Failure:** DB error → logs warning, returns. No VMs recovered. All
sandboxes remain in their DB state.

### Step 2: Load FC state for each sandbox

```go
fcState, err := st.LoadFirecrackerState(sb.ID)
```

**Failure:** No FC state row → `continue`. The sandbox exists in the
store but has no Firecracker state (maybe created by a different engine,
or partially created). Silently skipped.

**Failure:** Rootfs path empty → marks sandbox "unknown". Correct —
something went wrong during creation.

### Step 3: Rebuild volume attachments

```go
if attached, err := st.AttachedPersistentVolumesForSandbox(sb.ID); err == nil {
    for i, v := range attached {
        volumes = append(volumes, map[string]interface{}{
            "drive_id":  fmt.Sprintf("vol%d", i),
            "name":      v.VolumeName,
            "file_path": v.FilePath,
            "mount":     v.Mount,
            "read_only": v.ReadOnly,
        })
    }
}
```

**Failure:** DB query error → `err != nil` → `attached` is nil → `volumes`
stays nil. The VM is recovered without volume info. On resume, the jail
won't have volume files → FC fails with "No such file or directory".

**⚠️ KNOWN BUG (Bug #1): If the sandbox was created via snapshot resume
(`handleSnapshotResume`), volume attachments were never created in the
store.** `AttachedPersistentVolumesForSandbox` returns empty, `vm.Volumes`
is empty after recovery, and resume fails. This is the root cause of the
rory failure on April 5.

**Issue:** The `drive_id` is rebuilt as `fmt.Sprintf("vol%d", i)`. This
matches the server's assignment (`fmt.Sprintf("vol%d", len(resolvedVolumes))`)
ONLY IF volumes are in the same order. The DB query has no ORDER BY, so
the order depends on SQLite's internal storage order. If two volumes are
attached and the order changes, `drive_id` mappings are wrong. FC will
open the wrong drive for the wrong purpose. **Currently no production
sandboxes have >1 volume, so this hasn't bitten yet.**

### Step 4: Verify snapshot files

```go
snapshotOK := fcState.SnapMemPath != "" && fcState.SnapVMPath != "" &&
    fileExistsAndNonEmpty(fcState.SnapMemPath) &&
    fileExistsAndNonEmpty(fcState.SnapVMPath) &&
    fileExistsAndNonEmpty(fcState.RootfsPath)
```

**Failure:** Any file missing or empty → `snapshotOK = false`. If the
sandbox was "stopped", it's marked "unknown". If "running", also "unknown".
Correct — prevents trying to resume from corrupt snapshots.

**Issue:** Does NOT verify volume files exist. If a volume file is
missing (deleted, moved, disk failure), the snapshot will look OK but
resume will fail when FC tries to open the volume.

### Step 5: Call RestoreVM

```go
provider.RestoreVM(sb.EngineID, sb.Name, "stopped", state)
```

Populates the engine's in-memory VM map with the recovered state.
`vm.Volumes` is populated from the `volumes` slice built in step 3.

**No failure possible** — this is just an in-memory map insertion.

---

## Flow 4: startVM() — Cold → Hot (resume from snapshot)

Called by: `EnsureHot()` (on first API request to a cold sandbox),
auto-wake (for keep_hot sandboxes at startup), user `bhatti start`.

**Source:** `lifecycle.go:startVM()`

### Step 1: Lock and check state

```go
vm.stateMu.Lock()
defer vm.stateMu.Unlock()
if force { vm.restoreFailed = false }
if vm.restoreFailed { return error }
if vm.SnapMemPath == "" { return error }
```

**Failure:** Circuit breaker engaged (`restoreFailed=true`) → returns
error immediately without attempting resume. Use `--force` to clear.

### Step 2: Prepare socket path

```go
newSocketPath := baseSockPath + ".resume"
```

Appends `.resume` to avoid path growth. `strings.TrimSuffix` prevents
accumulation on repeated stop/start cycles.

### Step 3: Build jail file list (jailed mode)

```go
jp := newJailPaths(e.cfg.jailed())
if e.cfg.jailed() {
    jp.resolve("mem.snap", vm.SnapMemPath)
    jp.resolve("vm.snap", vm.SnapVMPath)
    jp.resolve("rootfs.ext4", vm.RootfsPath)
    configPath := filepath.Join(filepath.Dir(vm.RootfsPath), "config.ext4")
    jp.resolve("config.ext4", configPath)
    for _, vol := range vm.Volumes {
        jp.resolve(fmt.Sprintf("vol-%s.ext4", vol.Name), vol.FilePath)
    }
}
```

This registers all files that need to be hard-linked into the jail.
`startFCJailed` will iterate `jp.files` and create hard links.

**Failure mode (Bug #1):** If `vm.Volumes` is empty (because recovery
didn't find volume attachments), no volume files are registered. The
jail won't have them. FC will fail on `/snapshot/load`.

**Issue:** If `vol.FilePath` points to a file that doesn't exist (volume
deleted, disk failure), the `os.Link` in `startFCJailed` will fail. The
fallback `copyBlock` will also fail. `startFCJailed` returns an error and
the resume aborts. This is correct behavior — but the error message will
be "link vol-X.ext4 into jail: no such file or directory" which could be
confusing. Should explicitly check volume files exist before starting FC.

### Step 4: Start new Firecracker process

```go
fcProc, err := e.startFC(newSocketPath, startFCOpts{...})
```

In jailed mode, `startFCJailed`:
1. `os.RemoveAll` the old jail dir (cleans stale state)
2. Creates new jail dir
3. Hard-links all files from `jp.files` into the jail
4. Starts jailer process
5. Waits for API socket

**Failure:** Hard-link fails → falls back to copyBlock. Both fail →
jail dir is cleaned up, error returned. No zombie process.

**Failure:** Jailer exits prematurely (bad cgroup config, permissions) →
detected via `/proc/PID` check → error with jailer stderr. Jail cleaned.

**Failure:** API socket doesn't appear within 2s → `killFC`, cleanup,
error. No zombie.

### Step 5: Create symlinks (bare mode only)

```go
if !e.cfg.jailed() && vm.FCPathOrigin != "" && vm.FCPathOrigin != vm.ID {
    origDir := filepath.Join(e.cfg.DataDir, "sandboxes", vm.FCPathOrigin)
    // Create symlinks from original paths to current files
}
```

**⚠️ RACE (Issue #5 from advisor):** If two sandboxes share the same
`FCPathOrigin` (both created from the same snapshot) and both resume
concurrently, they fight over the symlink directory. One removes the
symlink the other just created.

**Not a problem in jailed mode** — each jail is per-sandbox-ID. agni-01
runs jailed mode, so this doesn't apply to production. But it's a
latent bug for bare-mode users.

### Step 6: Load snapshot

```go
if err = fcPut(ctx, client, "/snapshot/load", fmt.Sprintf(
    `{... "resume_vm":true, "network_overrides":[{"iface_id":"eth0","host_dev_name":%q}]}`,
    ...)); err != nil {
    return restoreFailed("load snapshot: %v", err)
}
```

**Failure:** FC can't load snapshot → `restoreFailed()` sets circuit
breaker, kills FC, returns error. The most common failures:

- "No such file or directory" for a drive: drive file not in jail
  (Bug #1) or volume file deleted.
- "Error restoring devices: MMIO..." — snapshot corruption (vm.snap
  references a device state that doesn't match the drive layout).
- "Failed to restore memory" — mem.snap corruption or size mismatch.

**Issue:** The error includes FC stderr if available, which is good for
debugging. But the circuit breaker means subsequent requests get an
instant error without re-attempting. `--force` clears it.

### Step 7: Clean up symlinks (bare mode)

```go
if symlinkDir != "" {
    os.RemoveAll(symlinkDir)
}
```

Only in bare mode. After FC has opened file descriptors for the drives,
the symlinks are no longer needed. Correct.

### Step 8: Update VM state

```go
vm.SocketPath = apiSocket
vm.cmd = fcProc.cmd
vm.cancel = fcProc.cancel
vm.Status = "running"
vm.Thermal = "hot"
```

**No failure possible** — in-memory state update.

### Step 9: Connect agent

```go
vm.Agent = agent.NewTCPClientWithAuth(vm.GuestIP, vm.Token)
if err := vm.Agent.WaitReady(ctx, 30*time.Second); err != nil {
    return restoreFailed("agent not ready after resume: %v", err)
}
```

**Failure:** Agent doesn't respond within 30s → `restoreFailed()`. FC
process is killed. Circuit breaker engaged.

**Possible causes:**
- Guest kernel panicked on resume (corrupt snapshot)
- Network not working (bridge FDB stale, ARP not resolving)
- Agent process crashed inside guest
- Guest clock jump caused TCP connection timeouts

**Issue (from advisor):** 30 seconds is very long for a warm resume
that should take <100ms. A 5s initial timeout with clear error would
be better UX.

**Issue:** Between steps 6 and 9, there's a `bridge fdb del` for ARP
staleness (step 6.5, not shown above). This is best-effort — if it
fails, the ARP cache might have the old MAC→port mapping and the
agent connection could timeout.

---

## Flow 5: Checkpoint() — Named snapshot (user-initiated)

Called by: `handleSandboxCheckpoint` in admin_handlers.go.

**Source:** `snapshot.go:Checkpoint()`

### Step 1-3: Lock, sync, pause (same as Stop)

Same failure modes as `Stop()` steps 1-3.

### Step 4: Create temp dir and FC snapshot

```go
tmpDir := finalDir + ".tmp"
os.RemoveAll(tmpDir)
os.MkdirAll(tmpDir, 0700)
fcPut(ctx, client, "/snapshot/create", ...)
```

**Failure:** FC snapshot fails → cleanup tmpDir, resume VM. Correct.

### Step 5: Move snapshot from jail (jailed mode)

```go
for _, pair := range [][2]string{...} {
    if err := os.Rename(pair[0], pair[1]); err != nil {
        if err := copyBlock(pair[0], pair[1]); err != nil {
            os.RemoveAll(tmpDir)
            resumeOnError()
            return nil, fmt.Errorf("move checkpoint artifact: %w", err)
        }
    }
}
```

**Failure:** Both rename and copy fail → cleanup tmpDir, resume VM,
return error. **Correct — unlike Stop() step 5, this properly returns
the error.**

### Step 6: Parallel copy drives

```go
g, _ := errgroup.WithContext(ctx)
for _, c := range copies {
    g.Go(func() error {
        return copyBlock(c.src, dst)
    })
}
if err := g.Wait(); err != nil {
    os.RemoveAll(tmpDir)
    resumeOnError()
    return nil, fmt.Errorf("copy block devices: %w", err)
}
```

**Failure:** Any copy fails → all are cancelled, tmpDir cleaned up, VM
resumed. Correct. But `copyBlock` errors are opaque ("exit status 1").

**Issue (from advisor):** If step 6 fails halfway (3 of 5 drives copied),
`os.RemoveAll(tmpDir)` cleans up the partial copies. The VM was paused at
step 3. `resumeOnError()` resumes it. This is correct.

**Issue:** The errgroup context is created with `_` (unused). If one copy
fails, the others continue until they finish — they're not cancelled. This
wastes I/O on copies that will be discarded. Should use the errgroup
context to cancel remaining copies.

### Step 7: Write manifest

```go
os.WriteFile(filepath.Join(tmpDir, "manifest.json"), manifestBytes, 0644)
```

**Failure:** Disk full → cleanup tmpDir, resume VM. Correct.

**Issue:** `FCPathOrigin` tracks which sandbox's paths are baked into
vm.snap. This is critical for resume to create correct symlinks (bare
mode) or jail paths (jailed mode). If `vm.FCPathOrigin` is empty, it
falls back to `sandboxID`. This fallback is correct for directly-created
VMs but may be wrong for VMs that were themselves resumed from a snapshot
(chain: snapshot A → sandbox B → checkpoint → snapshot C → sandbox D;
sandbox D's FCPathOrigin should be A's ID, not B's).

### Step 8: Atomic rename

```go
os.Rename(tmpDir, finalDir)
```

**Failure:** Rename fails (target exists — race condition with another
checkpoint of the same name) → cleanup, resume. The pre-check at step 0
(`os.Stat(finalDir)`) isn't atomic with the rename. TOCTOU race.

### Step 9: Resume VM

```go
if !wasPaused {
    fcPatch(ctx, client, "/vm", `{"state":"Resumed"}`)
    vm.Thermal = "hot"
}
```

**Failure:** Resume fails → error returned. VM stays paused. The
checkpoint succeeded (files on disk are valid). But the source VM is
now warm and won't auto-resume.

**⚠️ Issue (from advisor):** No agent health check after resume. The
checkpoint succeeded, but the source VM might be in a bad state.

---

## Flow 6: handleSnapshotResume() — Create sandbox from named snapshot

Called by: `bhatti snapshot resume <name>`.

**Source:** `admin_handlers.go:handleSnapshotResume()`

### Steps 1-4: Parse request, load snapshot, parse manifest, check limits

Standard HTTP handler stuff. No failure modes of interest.

### Step 5: Call engine ResumeFromManifestJSON

```go
info, err := sr.ResumeFromManifestJSON(r.Context(), snapDir, []byte(snap.ManifestJSON), sandboxName)
```

This calls `ResumeSnapshot()` in `snapshot.go`. See Flow 7 below.

### Step 6: Create sandbox record in store

```go
sbID := genID()
sb := store.Sandbox{ID: sbID, Name: sandboxName, EngineID: info.EngineID, ...}
s.store.CreateSandbox(sb)
```

**Failure:** DB error → destroys the engine sandbox, returns error.
Correct cleanup.

### Step 7: Save VM state

```go
s.saveVMState(sbID, info.EngineID)
```

**⚠️ CRITICAL BUG (Bug #1): No volume attachments created.**
The snapshot manifest contains drives with role="volume". These are
persistent volumes that were attached when the checkpoint was taken.
But `handleSnapshotResume` does NOT:
1. Look up the persistent volume records in `volumes_v2`
2. Create `volume_attachments` entries for the new sandbox

The engine's in-memory `vm.Volumes` is populated correctly by
`ResumeSnapshot()` (it reads the manifest), so the VM works NOW.
But on the next daemon restart, `recoverVMs` queries
`AttachedPersistentVolumesForSandbox(sbID)` which returns empty
(no attachments in DB). `vm.Volumes` is empty after recovery.
Resume fails with "No such file or directory" for the volume.

**Fix:** After `s.store.CreateSandbox(sb)`, iterate `m.Drives`,
find drives with `Role == "volume"`, look up the persistent volume
by name, and call `s.store.AttachPersistentVolume()`.

---

## Flow 7: ResumeSnapshot() — Engine-level snapshot resume

Called by: `handleSnapshotResume` (user-initiated) and
`handleCheckpointResume` (internal).

**Source:** `snapshot.go:ResumeSnapshot()`

### Step 1: Create sandbox dir, set up cleanup defer

```go
id := generateID()
sandboxDir := filepath.Join(e.cfg.DataDir, "sandboxes", id)
os.MkdirAll(sandboxDir, 0700)
defer func() {
    if err != nil {
        killFC, destroyTAP, releaseIP, RemoveAll(sandboxDir)
    }
}()
```

**Failure:** MkdirAll fails (permissions, disk full) → err set, defer
cleans up (sandboxDir may not exist, RemoveAll is a no-op). Correct.

### Step 2: Copy all drives from snapshot dir

```go
for _, drive := range manifest.Drives {
    if err = copyBlock(src, dst); err != nil {
        return ...
    }
}
```

**Failure:** Copy fails → err set, defer cleans up sandbox dir. Correct
but copies are sequential, not parallel. Large snapshots take a while.

**Issue:** Unlike `Checkpoint()` which copies drives in parallel,
`ResumeSnapshot()` copies them sequentially. For a snapshot with 3 drives
(rootfs 10GB + config 1MB + volume 5GB), this means ~15 seconds of
sequential `cp` instead of ~10 seconds parallel.

### Step 3: Allocate IP

```go
guestIP = manifest.Network.GuestIP
if err = userNet.Pool.TryAllocate(guestIP); err != nil {
    return ...
}
```

**Failure:** IP already in use (another sandbox has it, or a previous
failed resume leaked it) → error. Defer releases IP (but it was never
allocated, so the release is a no-op). The IP release in the defer is
guarded by `if guestIP != ""` — but `guestIP` was already set before
`TryAllocate`. If `TryAllocate` fails, `guestIP` is non-empty and the
defer tries to release an IP that was never allocated.

**⚠️ BUG: IP leak on TryAllocate failure.** `guestIP` is set to
`manifest.Network.GuestIP` before `TryAllocate`. If `TryAllocate`
fails, the defer calls `net.Pool.Release(guestIP)` — releasing an IP
that was never allocated. This could corrupt the IP pool by marking a
legitimately-in-use IP as available. On the NEXT resume, that IP would
be double-allocated.

**Fix:** Only set `guestIP` AFTER `TryAllocate` succeeds:
```go
if err = userNet.Pool.TryAllocate(manifest.Network.GuestIP); err != nil {
    return ...
}
guestIP = manifest.Network.GuestIP // only set if allocated
```

### Step 4: Create TAP device

```go
tapName, err = createTapDevice(id, userNet.BridgeName)
```

**Failure:** TAP creation fails → err set, defer destroys TAP (but
`tapName` is empty on failure, so `destroyTapDevice("")` is a no-op).
Correct. IP is released by defer.

### Step 5: Start Firecracker

```go
fcProc, startErr := e.startFC(socketPath, startFCOpts{
    id: id, vcpus: ..., memMB: ..., files: jp.files,
})
```

Same as `startVM()` step 4. Failures handled, cleanup in defer.

### Step 6: Load snapshot

```go
fcPut(ctx, client, "/snapshot/load", ...)
```

**Failure:** Snapshot load fails → FC killed by defer, cleanup. Error
includes FC stderr if available.

**Symlink cleanup (bare mode):** If symlinks were created and snapshot
load fails, `os.RemoveAll(symlinkCleanup)` cleans them. Correct.

### Step 7: FDB flush + agent connect

```go
exec.Command("bridge", "fdb", "del", ...).Run() // best effort
agentClient.WaitReady(ctx, 30*time.Second)
```

**Failure:** FDB flush fails → ignored (best effort). May cause ARP
staleness → agent WaitReady times out → FC killed, cleanup.

### Step 8: Register VM

```go
e.mu.Lock()
e.vms[id] = vm
e.mu.Unlock()
```

**No failure possible.** VM is now live and reachable.

---

## Consolidated Bug List

### Bug #1: handleSnapshotResume doesn't create volume_attachments (CRITICAL)

**Impact:** Any sandbox created via snapshot resume with persistent volumes
will fail to resume after a daemon restart.

**Root cause:** `handleSnapshotResume` in `admin_handlers.go` creates the
sandbox record and saves VM state but never calls
`store.AttachPersistentVolume()` for volumes in the snapshot manifest.

**Evidence:** Old rory (created via `snapshot.resumed` on Apr 3) failed to
resume on Apr 5 after daemon restart with "No such file or directory
/vol-rory-data.ext4".

**Fix:** After creating the sandbox in `handleSnapshotResume`, iterate
manifest drives, identify volumes (role="volume"), look up persistent
volume by name, and create `volume_attachments` entries.

### Bug #2: Stop() doesn't return error on snapshot artifact move failure (HIGH)

**Impact:** VM killed even when snapshot files weren't successfully moved
from jail to sandbox dir. Unrecoverable — no valid snapshot, no live VM.

**Location:** `lifecycle.go:Stop()` step 5 — the `slog.Error` for copy
failure doesn't return an error.

**Fix:** Return error on copy failure. Keep VM paused (alive).

### Bug #3: Stop() doesn't return error on verifySnapshotArtifacts failure (HIGH)

**Impact:** VM killed with known-corrupt snapshot. Same as Bug #2.

**Location:** `lifecycle.go:Stop()` step 6.

**Fix:** Return error on verification failure. Keep VM paused (alive).

### Bug #4: IP pool corruption on TryAllocate failure in ResumeSnapshot (MEDIUM)

**Impact:** If `TryAllocate` fails, the cleanup defer releases an IP that
was never allocated, potentially corrupting the pool.

**Location:** `snapshot.go:ResumeSnapshot()` step 3.

**Fix:** Only set `guestIP` after successful `TryAllocate`.

### Bug #5: SaveImage crashes on cold/stopped sandboxes (MEDIUM)

**Impact:** `SaveImage()` tries to talk to FC API on a cold sandbox where
the process is dead. Returns confusing "connection refused" error.

**Evidence:** Apr 5 18:20:10 — "resume after save: dial unix ... connection
refused" when karkhana-base was already stopped.

**Location:** `helpers.go:SaveImage()`

**Fix:** Check thermal state. Reject cold sandboxes with "sandbox is
stopped — start it first" or call `EnsureHot()` before proceeding.

### Bug #6: Symlink race in bare-mode resume (MEDIUM, latent)

**Impact:** Two sandboxes from the same snapshot resuming concurrently in
bare mode corrupt each other's symlink directories.

**Location:** `lifecycle.go:startVM()` step 5.

**Fix:** Use unique temp dirs instead of the origin dir, or add per-origin
locking. Not an issue in jailed mode (agni-01 uses jailed mode).

### Bug #7: Checkpoint errgroup doesn't cancel on failure (LOW)

**Impact:** When one drive copy fails, remaining copies continue wasting
I/O before being discarded.

**Location:** `snapshot.go:Checkpoint()` step 6. The errgroup context
return value is discarded (`g, _ := errgroup.WithContext(ctx)`).

**Fix:** Use the errgroup context for copyBlock operations.

### Bug #8: recoverVMs drive_id ordering not guaranteed (LOW, latent)

**Impact:** If a sandbox has >1 persistent volume, the `drive_id`
assignment in recovery (`vol0`, `vol1`, ...) depends on DB query order,
which may not match the original order. FC would mount the wrong drive
at the wrong path.

**Location:** `cmd/bhatti/main.go:recoverVMs()` step 3.

**Fix:** Store drive_id in `volume_attachments` or use an ORDER BY.

### Bug #9: No fsync after snapshot artifact writes (LOW)

**Impact:** Power loss between `cp` and daemon exit could leave valid-
looking but incomplete snapshot files. Unlikely but possible on bare metal.

**Fix:** Call `sync` after parallel drive copies in Checkpoint, or use
`cp` with fsync flag.

### Bug #10: Thermal manager blocks on stateMu contention (LOW)

**Impact:** Long-running Checkpoint() blocks the thermal cycle for all
other VMs.

**Fix:** Use TryLock or skip contended VMs.
