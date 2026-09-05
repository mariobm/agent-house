# Fix: Sandbox stuck in 'unknown' after thermal snapshot timeout

Issue: [#4](https://github.com/sahil-shubham/bhatti/issues/4)

---

## Summary

When a warm→cold thermal snapshot times out, the sandbox is immediately
marked `unknown` in the store. The VM is still alive (Firecracker process
running, vCPUs paused) but bhatti considers it unrecoverable. All
subsequent operations fail — exec returns 500, proxy returns 502.
The only recovery is destroy + recreate, losing all in-VM state.

---

## Root Cause

Three bugs compound into one unrecoverable failure.

### Bug 1: `fcPut`/`fcPatch` ignore the caller's context

The thermal cycle creates a 30-second context:

```go
// server.go:327
stopCtx, stopCancel := context.WithTimeout(context.Background(), 30*time.Second)
if err := s.engine.Stop(stopCtx, sb.EngineID); err != nil {
```

`Stop()` receives this `ctx` but never uses it. It passes to `fcPut`:

```go
// engine.go:622
if err := fcPut(client, "/snapshot/create", ...); err != nil {
```

And `fcPut` creates the HTTP request **without context**:

```go
// engine.go:1344
func fcPut(client *http.Client, path, body string) error {
    req, _ := http.NewRequest("PUT", "http://localhost"+path, ...)
```

The 30-second deadline never reaches the HTTP request. Dead code.

### Bug 2: Hardcoded 10-second client timeout is too short for full snapshots

The only timeout that fires is `http.Client.Timeout`:

```go
// engine.go:1333
func fcAPIClient(socketPath string) *http.Client {
    return &http.Client{
        Timeout: 10 * time.Second,
```

10 seconds is fine for `PATCH /vm` (sub-millisecond) but not for
`PUT /snapshot/create`. A Full snapshot writes entire VM memory to
disk — under I/O contention, it can exceed 10 seconds. The user's
error confirms this:

```
error="create Full snapshot: Put \"http://localhost/snapshot/create\":
       context deadline exceeded (Client.Timeout exceeded while awaiting headers)"
```

Note: diff snapshots are fast (~50ms for idle VMs). But the first
snapshot after boot is always Full. And if the base snapshot file
is missing, `Stop()` falls back to Full (engine.go:618). The error
says "Full snapshot."

### Bug 3: First failure → permanent `unknown`, no retry, no recovery

```go
// server.go:328-332
if err := s.engine.Stop(stopCtx, sb.EngineID); err != nil {
    stopCancel()
    slog.Error("thermal snapshot failed — marking unknown", ...)
    s.store.UpdateSandboxStatus(sb.ID, "unknown")
    continue
}
```

After the timeout, the VM is still alive. The Firecracker process is
running, vCPUs paused (warm). A `PATCH /vm {"state":"Resumed"}` would
bring it back. But instead the code marks it `unknown` and abandons it.

After `unknown`:
- The thermal cycle skips it (line 310: `sb.Status != "running"`)
- `ensureHot` only fixes store status for cold→running (line 400:
  `if wasCold`), not for unknown→running
- No code path transitions `unknown` back to `running`

The sandbox is permanently stuck. Engine says warm, store says unknown.

---

## The Fix

Five focused changes.

### 1. Thread context through `fcPut` and `fcPatch`

Make the existing 30-second `stopCtx` actually work.

**`pkg/engine/firecracker/engine.go`:**

```go
// Before:
func fcPut(client *http.Client, path, body string) error {
    req, _ := http.NewRequest("PUT", "http://localhost"+path, strings.NewReader(body))

// After:
func fcPut(ctx context.Context, client *http.Client, path, body string) error {
    req, _ := http.NewRequestWithContext(ctx, "PUT", "http://localhost"+path, strings.NewReader(body))
```

Same for `fcPatch`. Every call site gains a `ctx` argument.

Call sites and their context source:

| Function | Calls | Context source |
|----------|-------|----------------|
| `Create` | `fcPut` ×10 | `ctx` parameter (already available) |
| `SaveImage` | `fcPatch` ×3 | `ctx` parameter (already available) |
| `Stop` | `fcPatch` ×1, `fcPut` ×1 | `ctx` parameter |
| `Pause` | `fcPatch` ×1 | `ctx` parameter |
| `Resume` | `fcPatch` ×1 | `ctx` parameter |
| `Start` | `fcPut` ×1 | `ctx` parameter |

### 2. Remove hardcoded `http.Client.Timeout` from `fcAPIClient`

With context-based timeouts on every call, the 10-second client timeout
is redundant and creates confusion (two timeouts racing). Remove it.

```go
func fcAPIClient(socketPath string) *http.Client {
    return &http.Client{
        Transport: &http.Transport{
            DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
                d := net.Dialer{Timeout: 5 * time.Second}
                return d.DialContext(ctx, "unix", socketPath)
            },
            DisableKeepAlives: true,
        },
    }
}
```

The 5-second dial timeout remains — it catches "socket doesn't exist"
fast. But the request-level timeout comes from context, not the client.

Each `Stop()` call needs appropriate context timeouts:
- Pause: 5 seconds (`PATCH /vm` is instant)
- Snapshot: use the caller's context deadline (30s from thermal cycle,
  or whatever the caller passes). The thermal cycle's 30s should be
  bumped to 60s since full snapshots on large VMs under contention
  can take 10-30s.

### 3. `Stop()` skips Pause if VM is already warm

`Stop()` unconditionally calls `fcPatch(client, "/vm", '{"state":"Paused"}')`
before creating the snapshot. If the VM is already warm (paused),
Firecracker may reject the double-pause with a 400 error.

The codebase already has this pattern in `SaveImage` (engine.go:552):

```go
// SaveImage:
wasPaused := vm.Thermal == "warm"
if vm.Thermal == "hot" {
    client := fcAPIClient(vm.SocketPath)
    if err := fcPatch(client, "/vm", `{"state":"Paused"}`); err != nil {
        return fmt.Errorf("pause for save: %w", err)
    }
    vm.Thermal = "warm"
}
```

Apply the same pattern to `Stop()`:

```go
func (e *Engine) Stop(ctx context.Context, id string) error {
    // ...existing getVM, stateMu.Lock, status check...

    client := fcAPIClient(vm.SocketPath)

    // Skip Pause if already paused (warm→cold path).
    // Firecracker may reject Pause on an already-paused VM.
    // SaveImage already uses this pattern (engine.go:552).
    if vm.Thermal != "warm" {
        pauseCtx, pauseCancel := context.WithTimeout(ctx, 5*time.Second)
        defer pauseCancel()
        if err := fcPatch(pauseCtx, client, "/vm", `{"state":"Paused"}`); err != nil {
            return fmt.Errorf("pause: %w", err)
        }
    }

    // ...rest of snapshot creation using ctx...
```

This is critical for the retry path: when the thermal cycle retries
`Stop()` on a warm VM, the Pause step won't fail spuriously.

### 4. Retry counter in thermal cycle instead of immediate `unknown`

Track consecutive snapshot failures per sandbox. Only escalate to
`unknown` after 3 consecutive failures.

**`pkg/server/server.go`:**

Add field to Server:

```go
type Server struct {
    // ...existing fields...
    snapshotFailures sync.Map // engineID → int (consecutive failure count)
}
```

Clear it on user activity:

```go
func (s *Server) touchActivity(engineID string) {
    s.lastActivity.Store(engineID, time.Now())
    s.snapshotFailures.Delete(engineID)
}
```

Rewrite the warm→cold failure path:

```go
if thermal == "warm" {
    ts, ok := s.lastActivity.Load(sb.EngineID)
    if !ok {
        continue
    }
    idle := time.Since(ts.(time.Time))
    if idle > cfg.ColdTimeout {
        stopCtx, stopCancel := context.WithTimeout(
            context.Background(), 60*time.Second)
        err := s.engine.Stop(stopCtx, sb.EngineID)
        stopCancel()

        if err != nil {
            count := 0
            if v, ok := s.snapshotFailures.Load(sb.EngineID); ok {
                count = v.(int)
            }
            count++
            s.snapshotFailures.Store(sb.EngineID, count)

            if count >= 3 {
                slog.Error("thermal snapshot failed 3 times — marking unknown",
                    "sandbox", sb.Name, "id", sb.ID, "error", err,
                    "attempts", count)
                s.store.UpdateSandboxStatus(sb.ID, "unknown")
                s.snapshotFailures.Delete(sb.EngineID)
            } else {
                slog.Warn("thermal snapshot failed — will retry",
                    "sandbox", sb.Name, "id", sb.ID, "error", err,
                    "attempt", count, "max_attempts", 3)
            }
            continue
        }

        // Success — clear failure counter
        s.snapshotFailures.Delete(sb.EngineID)
        s.store.StopSandbox(sb.ID)
        s.saveVMState(sb.ID, sb.EngineID)
        slog.Info("thermal transition", "sandbox", sb.Name,
            "from", "warm", "to", "cold", "idle", idle.Round(time.Second))
    }
    continue
}
```

On failure, we don't mark `unknown`. We don't touch the VM at all —
it stays warm. The thermal cycle will try `Stop()` again 10 seconds
later. Fix #3 ensures the retry's Pause step won't fail on the
already-paused VM.

After 3 consecutive failures (30 seconds of retries), something is
genuinely wrong and we escalate to `unknown`. The counter resets on
user activity (`touchActivity`), so a user poking the sandbox between
cycles gets a fresh slate.

### 5. `ensureHot` updates store status on any successful wake

Currently `ensureHot` only corrects store status for `cold → running`:

```go
if wasCold {
    if sb, err := s.store.GetSandboxByEngineID(engineID); err == nil {
        s.store.UpdateSandboxStatus(sb.ID, "running")
        s.saveVMState(sb.ID, engineID)
    }
}
```

Change to correct status on **any** successful wake:

```go
if sb, err := s.store.GetSandboxByEngineID(engineID); err == nil {
    if sb.Status != "running" {
        slog.Info("sandbox recovered",
            "sandbox", sb.Name, "from_status", sb.Status)
        s.store.UpdateSandboxStatus(sb.ID, "running")
        s.saveVMState(sb.ID, engineID)
    }
}
```

This is a safety net. If a sandbox reaches `unknown` but the engine
can still resume it (the FC process is alive), the next user interaction
triggers `ensureHot` → `EnsureHot` succeeds → store is corrected to
`running`. Self-healing.

---

## Tests

### Unit tests (`pkg/server/thermal_test.go`)

Mock engine, no Firecracker needed. These test the thermal cycle's
retry and recovery behavior.

**`TestSnapshotFailureRetries`** — verify 3-strike escalation:
- Set sandbox to warm, past cold timeout
- Make mock `Stop()` return error
- Run 1 thermal cycle → store still `running`, failure count 1
- Run 2nd cycle → store still `running`, failure count 2
- Run 3rd cycle → store is `unknown`, counter cleared

**`TestSnapshotFailureCounterResetOnActivity`** — user activity resets:
- Fail twice (count=2)
- Call `touchActivity` (simulates user exec)
- Counter is cleared
- Next failure starts at count=1, not 3

**`TestSnapshotSuccessClearsCounter`** — recovery after transient:
- Fail once (count=1)
- Clear `StopErr`, run cycle → `Stop` succeeds
- Counter is cleared, sandbox is stopped normally

**`TestEnsureHotRecoverFromUnknown`** — auto-recovery:
- Set engine thermal to `warm`, store status to `unknown`
- Call `ensureHot` → succeeds (mock `EnsureHot` sets thermal=hot)
- Store status is `running`

### Mock engine changes

Add `StopErr` to mock engine:

```go
type mockEngine struct {
    // ...existing fields...
    StopErr error
}

func (m *mockEngine) Stop(_ context.Context, id string) error {
    m.mu.Lock()
    defer m.mu.Unlock()
    if m.StopErr != nil {
        return m.StopErr
    }
    // ...existing logic...
}
```

### Integration tests (`pkg/engine/firecracker/thermal_test.go`)

These run on Linux with root + Firecracker. They test the actual
engine behavior — context timeouts, `Stop()` on warm VMs, recovery.

**`TestStopWarmVM`** — verify `Stop()` works on an already-paused VM:

```go
func TestStopWarmVM(t *testing.T) {
    eng := testEngine(t)
    ctx := context.Background()

    info, err := eng.Create(ctx, testSpec("stop-warm"))
    if err != nil {
        t.Fatalf("Create: %v", err)
    }
    defer eng.Destroy(ctx, info.ID)

    // Write state
    execWithTimeout(t, eng, info.ID, []string{"sh", "-c", "echo warm-data > /tmp/data"})

    // Go warm (pause vCPUs)
    if err := eng.Pause(ctx, info.ID); err != nil {
        t.Fatalf("Pause: %v", err)
    }
    if eng.ThermalState(info.ID) != "warm" {
        t.Fatalf("expected warm, got %s", eng.ThermalState(info.ID))
    }

    // Now Stop from warm — this is the warm→cold thermal path.
    // Stop must not fail trying to double-Pause.
    if err := eng.Stop(ctx, info.ID); err != nil {
        t.Fatalf("Stop from warm: %v", err)
    }
    if eng.ThermalState(info.ID) != "cold" {
        t.Fatalf("expected cold after stop, got %s", eng.ThermalState(info.ID))
    }

    // Resume and verify data survived
    if err := eng.Start(ctx, info.ID); err != nil {
        t.Fatalf("Start: %v", err)
    }
    r, _ := execWithTimeout(t, eng, info.ID, []string{"cat", "/tmp/data"})
    if strings.TrimSpace(r.Stdout) != "warm-data" {
        t.Fatalf("data lost after warm→cold→hot: %q", r.Stdout)
    }
    t.Log("✓ Stop from warm state works, data preserved")
}
```

**`TestStopRespectsContext`** — verify context deadline is honored:

```go
func TestStopRespectsContext(t *testing.T) {
    eng := testEngine(t)
    ctx := context.Background()

    info, err := eng.Create(ctx, testSpec("stop-ctx"))
    if err != nil {
        t.Fatalf("Create: %v", err)
    }
    defer eng.Destroy(ctx, info.ID)

    // Call Stop with an impossibly short timeout.
    // The snapshot/create should fail with context deadline exceeded,
    // NOT with the old 10s client timeout.
    shortCtx, cancel := context.WithTimeout(ctx, 1*time.Millisecond)
    defer cancel()
    time.Sleep(2 * time.Millisecond) // ensure deadline has passed

    err = eng.Stop(shortCtx, info.ID)
    if err == nil {
        t.Fatal("expected error with expired context")
    }
    if !strings.Contains(err.Error(), "context deadline exceeded") {
        t.Fatalf("expected context deadline error, got: %v", err)
    }

    // VM should still be alive and usable — Stop() failed before
    // killing the process
    if eng.ThermalState(info.ID) == "cold" {
        t.Fatal("VM should not be cold — Stop() failed")
    }

    // EnsureHot should bring it back to working state
    if err := eng.EnsureHot(ctx, info.ID); err != nil {
        t.Fatalf("EnsureHot after failed Stop: %v", err)
    }
    r, _ := execWithTimeout(t, eng, info.ID, []string{"echo", "alive"})
    if !strings.Contains(r.Stdout, "alive") {
        t.Fatalf("exec after failed Stop: %q", r.Stdout)
    }
    t.Log("✓ Stop respects context, VM recoverable after timeout")
}
```

**`TestStopSucceedsWithAdequateTimeout`** — verify the fix works
end-to-end:

```go
func TestStopSucceedsWithAdequateTimeout(t *testing.T) {
    eng := testEngine(t)
    ctx := context.Background()

    info, err := eng.Create(ctx, testSpec("stop-ok"))
    if err != nil {
        t.Fatalf("Create: %v", err)
    }
    defer eng.Destroy(ctx, info.ID)

    execWithTimeout(t, eng, info.ID, []string{"sh", "-c", "echo data > /tmp/check"})

    // Stop with 60-second timeout (same as the fixed thermal cycle)
    stopCtx, cancel := context.WithTimeout(ctx, 60*time.Second)
    defer cancel()

    start := time.Now()
    if err := eng.Stop(stopCtx, info.ID); err != nil {
        t.Fatalf("Stop: %v", err)
    }
    t.Logf("Stop took %v", time.Since(start))

    // Resume and verify
    if err := eng.Start(ctx, info.ID); err != nil {
        t.Fatalf("Start: %v", err)
    }
    r, _ := execWithTimeout(t, eng, info.ID, []string{"cat", "/tmp/check"})
    if strings.TrimSpace(r.Stdout) != "data" {
        t.Fatalf("data lost: %q", r.Stdout)
    }
    t.Log("✓ Stop with adequate timeout works")
}
```

**`TestVMRecoverableAfterSnapshotFailure`** — the full scenario from
the issue: snapshot fails, VM is still usable:

```go
func TestVMRecoverableAfterSnapshotFailure(t *testing.T) {
    eng := testEngine(t)
    ctx := context.Background()

    info, err := eng.Create(ctx, testSpec("recover"))
    if err != nil {
        t.Fatalf("Create: %v", err)
    }
    defer eng.Destroy(ctx, info.ID)

    // Write data, go warm
    execWithTimeout(t, eng, info.ID, []string{"sh", "-c", "echo important > /tmp/state"})
    if err := eng.Pause(ctx, info.ID); err != nil {
        t.Fatalf("Pause: %v", err)
    }

    // Simulate the issue: Stop with a timeout too short for Full snapshot.
    // Use a very short timeout so the snapshot (Full, first time) fails.
    shortCtx, cancel := context.WithTimeout(ctx, 1*time.Millisecond)
    time.Sleep(2 * time.Millisecond)
    err = eng.Stop(shortCtx, info.ID)
    cancel()

    if err == nil {
        // On fast NVMe, even 1ms might succeed. If Stop succeeded,
        // the test is moot — just resume and verify data.
        t.Log("Stop succeeded even with short timeout (fast disk), resuming")
        eng.Start(ctx, info.ID)
        r, _ := execWithTimeout(t, eng, info.ID, []string{"cat", "/tmp/state"})
        if strings.TrimSpace(r.Stdout) != "important" {
            t.Fatalf("data lost: %q", r.Stdout)
        }
        return
    }
    t.Logf("Stop failed as expected: %v", err)

    // The VM should still be alive (FC process running, vCPUs paused).
    // EnsureHot should resume it.
    if err := eng.EnsureHot(ctx, info.ID); err != nil {
        t.Fatalf("EnsureHot after failed snapshot: %v", err)
    }

    // Data should still be there
    r, _ := execWithTimeout(t, eng, info.ID, []string{"cat", "/tmp/state"})
    if strings.TrimSpace(r.Stdout) != "important" {
        t.Fatalf("data lost after failed snapshot + recovery: %q", r.Stdout)
    }
    t.Log("✓ VM recoverable after snapshot failure, data preserved")
}
```

---

## Implementation Order

### Phase 1: Context threading (Fixes 1-2)

Root cause. Prevents the timeout from happening.

1. Add `ctx` parameter to `fcPut` and `fcPatch`
2. Remove `http.Client.Timeout: 10 * time.Second` from `fcAPIClient`
3. Update all call sites to pass context (with appropriate sub-timeouts)
4. Bump thermal cycle `stopCtx` from 30s → 60s
5. Bump `SnapshotAll` context from 30s → 60s
6. Run existing tests — all `thermal_test.go` and `engine_test.go`
7. Add `TestStopRespectsContext`, `TestStopSucceedsWithAdequateTimeout`

### Phase 2: `Stop()` warm-safe (Fix 3)

Makes retries work. Applies the `SaveImage` pattern to `Stop()`.

1. Add `wasPaused` check to `Stop()`, skip Pause if already warm
2. Add `TestStopWarmVM` integration test

### Phase 3: Retry + recovery (Fixes 4-5)

Resilience for any snapshot failure, not just timeouts.

1. Add `snapshotFailures sync.Map` to Server
2. Rewrite warm→cold failure path with retry counter
3. Clear counter in `touchActivity`
4. Change `ensureHot` to update store status on any successful wake
5. Add unit tests: `TestSnapshotFailureRetries`,
   `TestSnapshotFailureCounterResetOnActivity`,
   `TestSnapshotSuccessClearsCounter`,
   `TestEnsureHotRecoverFromUnknown`
6. Add integration test: `TestVMRecoverableAfterSnapshotFailure`

### Dependency graph

```
Phase 1 (context threading)  — standalone, fixes root cause
     ↓
Phase 2 (Stop warm-safe)     — needed for Phase 3 retries
     ↓
Phase 3 (retry + recovery)   — uses Phase 1 context + Phase 2 warm-safety
```

---

## What's Not in This Plan

**Memory-proportional snapshot timeout formula.** The caller controls
timeout via context. The thermal cycle's 60s is enough for any VM size
on NVMe. If we find edge cases, tune the caller's timeout — don't add
a formula inside `Stop()`.

**`bhatti recover` CLI command.** Fix 5 makes `ensureHot` self-heal.
The next user interaction auto-recovers. No manual command needed.

**`handleSandboxStart` accepting `unknown` status.** With `ensureHot`
auto-recovery, this is redundant. Any exec/shell/proxy triggers
`ensureHot` which corrects the store.

**Resume-and-re-pause dance on failure.** After a failed `Stop()`, the
VM stays warm. The thermal cycle retries 10 seconds later. No need to
explicitly manipulate the VM state between retries.

**Persisting failure count across daemon restarts.** Overkill. If the
daemon restarted, the environment changed. A fresh start is correct.

**Configurable retry count.** 3 is right. Not worth a config knob.
