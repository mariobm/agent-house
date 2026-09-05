# Bhatti Reliability Audit

_April 1, 2026 — triggered by the `rory` snapshot corruption incident_

## What happened to rory

Sandbox `rory` (the only sandbox with a persistent volume) was never thermally
transitioned by the thermal manager during daemon PID 573871's 19-hour run. The
hot→warm Activity query to the guest agent either timed out or reported attached
sessions on every 10-second cycle — **silently, with no log**. So rory stayed
hot for hours after the user left.

At daemon shutdown, `SnapshotAll` found rory still hot and took a **Diff
snapshot** (because `has_base_snapshot=true` was persisted from a previous
cycle). The Diff snapshot wrote a new `vm.snap` (full device state) but only
dirty memory pages to `mem.snap`. The virtio ring buffer pages for the
persistent volume's virtio-blk device were stale in `mem.snap`, causing a
mismatch: `vm.snap` said one thing about the available descriptor count, but the
ring memory in `mem.snap` said another. On restore, Firecracker panicked:

```
The number of available virtio descriptors 2598 is greater than queue size: 256!
```

Three compounding failures:
1. **Silent thermal skip** — no log when Activity query fails or sessions block pause
2. **Diff snapshot inconsistency** — `vm.snap` device state vs stale `mem.snap` ring memory
3. **No snapshot verification** — snapshot "succeeded" (FC returned 204) but the artifact was corrupt

---

## Subsystem-by-subsystem audit

### 1. Thermal Manager (`server.go:310-417`)

**What it does:** Transitions idle VMs through hot→warm→cold to reclaim resources.

**Bugs found:**

| # | Severity | Issue | Line |
|---|----------|-------|------|
| 1 | **Critical** | **Activity query timeout is silent.** If `te.Activity()` times out (5s), the sandbox is skipped with `continue` — no log, no counter, no escalation. A stuck agent keeps a VM hot forever, burning host RAM and CPU. This is what happened to rory. | `server.go:396` |
| 2 | **High** | **AttachedSessions blocks pause silently.** If sessions > 0, the VM stays hot with no log. A detached-but-"attached" session (agent bug, zombie process) prevents thermal management permanently. | `server.go:403` |
| 3 | **Medium** | No metric/counter for skipped sandboxes. Impossible to detect thermal management stalls from outside the system. | — |

**Fixes:**
- Log a warning after N consecutive Activity failures for the same sandbox
- After M consecutive failures (e.g. 10), force-pause the VM (the Pause API doesn't need the agent — it's a Firecracker call)
- Log when sessions are blocking thermal transition, with session count

### 2. Snapshot Path (`engine.go:590-665`)

**What it does:** Pauses VM, creates Full or Diff snapshot, kills FC process.

**Bugs found:**

| # | Severity | Issue | Line |
|---|----------|-------|------|
| 4 | **Critical** | **Diff snapshots can produce corrupt artifacts.** The `vm.snap` (device state) is always fully rewritten, but `mem.snap` only has dirty pages updated. If KVM's dirty page tracking misses host-side writes to virtio ring memory, the snapshot is silently corrupt. This is the direct cause of the rory panic. | `engine.go:637` |
| 5 | **High** | **No snapshot verification.** FC returns 204 on `/snapshot/create` but there's no validation that the resulting files are consistent. A corrupt snapshot is only discovered on restore (possibly hours/days later). | `engine.go:637` |
| 6 | **High** | **`SnapshotAll` uses Diff for hot VMs.** Diff snapshots are designed for warm/cold VMs where the dirty page set is small. Taking a Diff snapshot of a hot VM that has been running for hours means the dirty page set is huge AND the ring state is most likely to be inconsistent. | `server.go:255` |
| 7 | **Medium** | **`has_base_snapshot` persists across daemon restarts but the base it refers to may have been overwritten.** After N daemon restarts with Diff snapshots stacking, the `mem.snap` on disk is a patchwork of pages from different snapshot generations. | `engine.go:953` |

**Fixes:**
- **Use Full snapshots in `SnapshotAll`** — shutdown snapshots are infrequent and correctness matters more than speed. Diff is fine for thermal warm→cold (frequent, small dirty set, VM already paused).
- After creating a snapshot, do a lightweight verification: start a throwaway FC process, load the snapshot without `resume_vm`, check for errors, kill it. (~100ms overhead)
- Consider always using Full snapshots for VMs that have been hot (not just warm→cold Diff candidates)
- Add `sync` exec to the guest before pausing hot VMs in Stop() (like SaveImage already does) — this flushes the page cache and reduces in-flight I/O

### 3. Snapshot Restore (`engine.go:756-830`)

**What it does:** Starts new FC process, loads snapshot, waits for agent.

**Bugs found:**

| # | Severity | Issue | Line |
|---|----------|-------|------|
| 8 | **High** | **FC stderr goes to daemon stderr, not captured per-VM.** When FC panics on restore, the error message ("virtio descriptors...") goes to the daemon's stderr but is NOT in the structured log. The `fcPut` for `/snapshot/load` may succeed (204) and then FC crashes 1ms later — the error surfaces only as "agent not ready after resume" with no root cause. | `engine.go:783` |
| 9 | **Medium** | **Zombie FC processes on repeated restore failures.** Each failed restore leaves a zombie. The daemon doesn't reap them or track failed attempts. | `engine.go:786` |
| 10 | **Medium** | **No circuit breaker on restore.** `ensureHot` is called on every API request. If restore keeps failing, every exec/shell/file request spawns a new FC process, fails after 30s, and leaves a zombie. | `engine.go:735` |

**Fixes:**
- Capture FC stderr per-VM into a buffer or log file. On restore failure, include the FC stderr in the error message.
- After a restore failure, mark the VM with a "restore_failed" flag. Don't retry until explicitly requested (e.g. `bhatti start --force`). Return a clear error: "sandbox snapshot is corrupt, data is safe on disk, please recreate."
- Reap zombie processes in the error path (Kill + Wait already happens, but ensure it covers the panic-after-204 case).

### 4. Name Resolution (`store.go:562`, `routes.go:724`)

**What it does:** Looks up sandboxes by ID. Names are NOT resolved server-side.

**Bugs found:**

| # | Severity | Issue | Line |
|---|----------|-------|------|
| 11 | **Medium** | **`GetSandbox` only matches by `id` column.** Every name-based request hits a 404, then the CLI falls back to listing all sandboxes. This doubles API calls and confuses users who see the 404 in logs. | `store.go:562` |

**Fix:**
- Change `GetSandbox` query to: `WHERE (id = ? OR name = ?) AND created_by = ?`
- Or add a `GetSandboxByName(userID, name)` and try it as fallback in `getUserSandbox`

### 5. Recovery (`engine.go:924-982`, `main.go:recoverVMs`)

**What it does:** On daemon startup, restores sandbox metadata from SQLite into the engine's in-memory map.

**Bugs found:**

| # | Severity | Issue | Line |
|---|----------|-------|------|
| 12 | **Medium** | **Recovered cold VMs with `has_base_snapshot=true` will use Diff on next snapshot.** But the VM was loaded with `enable_diff_snapshots: true` — so dirty tracking IS active. The concern is whether the dirty tracking reset properly covers all pages. | `engine.go:953` |
| 13 | **Low** | **No health check on recovered snapshots.** A corrupt snapshot is only discovered when a user triggers `ensureHot`. Could be hours/days after daemon restart. | — |

**Fixes:**
- On recovery, optionally validate snapshots in the background (try loading each in a throwaway FC process)
- Consider resetting `has_base_snapshot` to `false` on recovery so the first snapshot after restart is always Full

### 6. Guest Agent Communication

**What it does:** Host↔guest communication over TCP for exec, shell, files, activity queries.

**Potential issues:**

| # | Severity | Issue |
|---|----------|-------|
| 14 | **Medium** | **Agent unresponsiveness blocks thermal management** (see #1). If a user's process inside the VM hogs CPU or the agent crashes, the thermal manager can't query activity and silently skips the VM. |
| 15 | **Low** | **Agent activity endpoint doesn't distinguish "agent is alive but busy" from "agent is dead."** The 5s timeout treats both the same. |

**Fixes:**
- Separate the "is the agent alive" check (TCP connect to port 1024) from the "what's the activity" query. If TCP connect fails, the VM's agent is dead — force-pause is safe.
- Add a lightweight `/ping` endpoint to the agent that responds immediately regardless of load.

### 7. Volume Attachment Lifecycle

**What it does:** Persistent volumes are ext4 files attached as virtio-blk drives.

**Bugs found:**

| # | Severity | Issue |
|---|----------|-------|
| 16 | **Medium** | **Volume attachment in DB is not released on sandbox destroy if destroy path fails partway.** The `rory` incident required manual SQL to detach the volume before creating `rory2`. |
| 17 | **Low** | **No fsck on volume after unclean VM death.** The journal replay happens implicitly on next mount, but if the journal is corrupt, data could be lost silently. |

---

## Priority order

### Must fix now (caused the incident)
1. **#6 — SnapshotAll should use Full snapshots.** One-line change, eliminates the Diff snapshot corruption vector for shutdown snapshots.
2. **#1 — Log and escalate Activity query failures.** Prevents VMs from silently avoiding thermal management.
3. **#10 — Circuit breaker on restore failures.** Prevents zombie storms when a snapshot is corrupt.
4. **#8 — Capture FC stderr per-VM.** Makes snapshot restore failures diagnosable without SSH access to the server.

### Should fix soon (latent risks)
5. **#5 — Snapshot verification.** Catches corrupt snapshots at creation time, not hours later.
6. **#2 — Log when sessions block thermal transitions.**
7. **#4 — `sync` before snapshot of hot VMs.** Reduces in-flight I/O at snapshot time.
8. **#11 — Server-side name resolution.** Eliminates the 404 noise and double API calls.
9. **#9 — Reap zombies and track failed restores.**

### Nice to have (defense in depth)
10. **#13 — Background snapshot validation on recovery.**
11. **#12 — Reset `has_base_snapshot` on recovery.**
12. **#14/#15 — Separate agent liveness from activity queries.**
13. **#16 — Robust volume detachment in destroy error path.**
14. **#7 — Diff snapshot chain integrity tracking.**
