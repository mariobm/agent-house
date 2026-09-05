# Bhatti v4 — Hardening, SDK Readiness, Performance, Test Coverage

Parts 1–20 are done: wire protocol, lohar agent, FC engine, sessions,
thermals, filesystem, CLI, deployment, install. 229 tests on real
Firecracker VMs. ~16k lines of Go.

This plan hardens the codebase for external users and makes the API
suitable for agentic frameworks (pi, Claude Code, etc.). Four phases:

1. **Security & correctness** — fix bugs that cause data loss, unauthorized access, or panics
2. **SDK readiness** — streaming exec, server-side truncation, rootfs tooling
3. **Performance** — diff snapshots, allocation reduction, ring buffer rewrite
4. **Test coverage** — fill gaps in proxy, recovery, benchmarks

Phase 2 comes before Phase 3 because external users hit the API first.
If `bhatti exec dev -- npm install` produces 30 seconds of silence
before dumping output, or a 10MB file read transfers 10MB through the
wire protocol before the consumer truncates to 50KB, the first impression
is broken. Diff snapshots and allocation pools are invisible to users —
they improve existing operations but don't unlock new capabilities.

Research inputs: `docs/pi-learnings.md` (agentic tool patterns),
`docs/sprites-learnings.md`, `docs/slicer-learnings.md`.

### Dependency graph

```
Phase 1 (security & correctness)
  Part 21 (auth fix)           — no deps, one-line fix
  Part 22 (VM state races)     — no deps, structural
  Part 23 (unchecked errors)   — no deps, grep + fix
  Part 24 (resource safety)    — no deps, cleanup patterns
  Part 25 (request limits)     — no deps
  Part 40 (connection timeouts) — no deps, production blocker
  Part 41 (RestoreVM safety)   — no deps, panic fix
  Part 42 (structured logging) — no deps, observability foundation
  Part 43 (health + graceful)  — depends on Part 42

Phase 2 (SDK readiness)
  Part 35 (streaming exec)     — no deps
  Part 36 (file read trunc)    — no deps
  Part 37 (rootfs tooling)     — no deps
  Part 38 (file abort)         — depends on Part 40 (needs context in dial)
  Part 39 (process group kill) — no deps

Phase 3 (performance)
  Part 26 (diff snapshots)     — no deps
  Part 27 (frame alloc)        — no deps
  Part 28 (ring buffer)        — no deps
  Part 29 (thermal tuning)     — depends on Part 40 (timeout), Part 42 (logging)

Phase 4 (test coverage)
  Part 30 (proxy tests)        — no deps
  Part 31 (recovery tests)     — depends on Part 41 (type safety)
  Part 32 (store round-trip)   — no deps
  Part 33 (benchmarks)         — depends on Parts 26–28
  Part 34 (perf workloads)     — depends on Part 33
```

### Execution schedule

```
Day 1:   Parts 21, 23, 25, 40         security fixes + timeouts
Day 2:   Part 22                       per-VM mutex (structural, needs care)
Day 3:   Parts 24, 41, 42             cleanup, RestoreVM safety, slog migration
Day 4:   Parts 35, 43                 streaming exec, health/graceful shutdown
Day 5:   Part 36                       server-side file truncation
Day 6:   Parts 37, 39                 rootfs tooling + process group kill
Day 7:   Part 38                       file abort
Week 2:  Parts 26–29                  performance
Week 2+: Parts 30–34                  test coverage (ongoing as features land)
```

---

# Phase 1 — Security & Correctness

## Part 21 — WebSocket Auth Bypass Fix

### 21.1 The Bug

`pkg/server/server.go` line 231 exempts `/ws` paths from auth:

```go
if token != s.authToken && !strings.HasSuffix(r.URL.Path, "/ws") {
```

Any unauthenticated client can open a WebSocket to `/sandboxes/:id/ws`
and get a shell. The `?token=` query param path already works for WS
clients — the exemption is unnecessary.

### 21.2 Fix

```go
// pkg/server/server.go — ServeHTTP method

if s.authToken != "" && !isStaticPath(r.URL.Path) {
    token := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
    if token == "" {
        token = r.URL.Query().Get("token")
    }
    if token != s.authToken {
        writeJSON(w, http.StatusUnauthorized, map[string]string{"error": "unauthorized"})
        return
    }
}
```

Remove the `&& !strings.HasSuffix(r.URL.Path, "/ws")` clause entirely.

### 21.3 Tests

- `TestWSAuthRequired` — connect to `/sandboxes/:id/ws` without token,
  verify HTTP 401 (not upgrade).
- `TestWSAuthQueryParam` — connect with `?token=correct`, verify upgrade
  succeeds, shell works.
- `TestWSAuthBearerHeader` — connect with `Authorization: Bearer correct`,
  verify upgrade succeeds.
- `TestWSAuthWrongToken` — connect with `?token=wrong`, verify 401.

---

## Part 22 — VM State Race Conditions

### 22.1 Problem

`getVM()` returns a `*VM` pointer under `RLock`, then callers mutate fields
(`Status`, `Thermal`, `SnapMemPath`, `cmd`, `Agent`, etc.) outside any lock.
The thermal manager runs concurrently: `runThermalCycle` calls `Pause`,
`Activity`, `Stop` while the server may be calling `Exec`, `EnsureHot`.

### 22.2 Solution: Per-VM Mutex

Add a `sync.Mutex` to the `VM` struct. Every method that reads or writes
VM state acquires it. The engine-level `sync.RWMutex` protects the `vms`
map only — not individual VM fields.

```go
// pkg/engine/firecracker/engine.go

type VM struct {
    // stateMu protects all mutable fields below. The engine-level
    // sync.RWMutex (e.mu) protects the vms map — not individual VM state.
    //
    // Lock discipline:
    //   - Short operations (Exec, Pause, Resume, Status, FileRead, etc.):
    //     hold stateMu for the entire operation.
    //   - Long-lived operations (Shell, Tunnel):
    //     hold stateMu only to validate state and capture the Agent reference,
    //     then release before the blocking call. The Agent pointer is safe to
    //     use after release because it's only replaced during Start() which
    //     holds stateMu.
    stateMu     sync.Mutex

    ID          string
    Name        string
    // ... existing fields ...
    Status      string
    Thermal     string
    cmd         *exec.Cmd
    cancel      context.CancelFunc
    Agent       *agent.AgentClient
    SnapMemPath string
    SnapVMPath  string
}
```

### 22.3 Lock Discipline

Every exported engine method that touches a VM:

```go
func (e *Engine) Pause(ctx context.Context, id string) error {
    vm, err := e.getVM(id)
    if err != nil { return err }

    vm.stateMu.Lock()
    defer vm.stateMu.Unlock()

    if vm.Thermal != "hot" {
        return fmt.Errorf("sandbox %q is not hot (thermal=%s)", id, vm.Thermal)
    }
    client := fcAPIClient(vm.SocketPath)
    if err := fcPatch(client, "/vm", `{"state":"Paused"}`); err != nil {
        return fmt.Errorf("pause: %w", err)
    }
    vm.Thermal = "warm"
    return nil
}
```

Apply this pattern to: `Pause`, `Resume`, `Stop`, `Start`, `EnsureHot`,
`Destroy`, `Exec`, `Shell`, `ListeningPorts`, `Tunnel`, `SessionList`,
`FileRead`, `FileWrite`, `FileStat`, `FileList`.

For methods that only read (`Status`, `ThermalState`, `Activity`, `VMState`):
use `vm.stateMu.Lock()` as well — a `sync.RWMutex` on the VM isn't worth the
complexity since all critical sections are short.

### 22.4 Thermal + Exec Interlock

The thermal manager might try to pause a VM while an exec is in flight.
With the per-VM mutex, the pause blocks until the exec completes. This is
correct: the exec holds the lock, sends the request to the agent, reads
the response, releases the lock, then the thermal manager pauses.

Long-running operations (Shell, Tunnel) should NOT hold the lock for
the entire duration. Instead:

```go
func (e *Engine) Shell(ctx context.Context, id string) (engine.TerminalConn, error) {
    vm, err := e.getVM(id)
    if err != nil { return nil, err }

    vm.stateMu.Lock()
    if vm.Thermal != "hot" {
        vm.stateMu.Unlock()
        return nil, fmt.Errorf("sandbox %q is not hot", id)
    }
    agent := vm.Agent  // capture reference under lock
    vm.stateMu.Unlock()

    // Shell call does NOT hold the lock — it's a long-lived connection.
    // The Agent pointer is safe: only replaced in Start(), which holds stateMu.
    return agent.Shell(ctx, []string{"/bin/zsh", "-li"}, map[string]string{
        "TERM": "xterm-256color",
    }, 24, 80)
}
```

### 22.5 Tests

- `TestConcurrentPauseExec` — start 10 goroutines: 5 calling Exec, 5
  calling Pause/Resume in a loop. Run for 3 seconds. No panics, no
  corrupted state.
- `TestConcurrentThermalExec` — one goroutine runs `runThermalCycle` on
  a 100ms tick with `warmTimeout=500ms`. Another goroutine does exec
  every 200ms. Verify all execs succeed (ensureHot fires correctly).

---

## Part 23 — Unchecked json.Unmarshal Errors

### 23.1 Problem

Multiple call sites ignore the return value of `json.Unmarshal`. If the
agent sends malformed JSON (due to a bug, version mismatch, or truncated
frame), the caller silently gets zero-value structs — empty session IDs,
zero file sizes, nil slices.

### 23.2 Files to Fix

**`pkg/agent/client.go`** — 5 sites:

```go
// Before:
json.Unmarshal(payload, &info)

// After:
if err := json.Unmarshal(payload, &info); err != nil {
    conn.Close()
    return nil, nil, fmt.Errorf("unmarshal session info: %w", err)
}
```

Fix in: `ShellSession` (line 423), `SessionAttach` (line 456),
`FileRead` (line 493), `FileStat` (line 584), `FileList` (line 615).

**`pkg/store/store.go`** — 3 sites in `scanTemplate`:

```go
// Before:
json.Unmarshal([]byte(secretsJSON), &t.Secrets)

// After:
if err := json.Unmarshal([]byte(secretsJSON), &t.Secrets); err != nil {
    return nil, fmt.Errorf("unmarshal secrets: %w", err)
}
```

**`cmd/lohar/handler.go`** — 1 site in EXEC_KILL handler (line 100):

```go
// Before:
json.Unmarshal(payload, &req)

// After:
if err := json.Unmarshal(payload, &req); err != nil {
    proto.WriteFrame(conn, proto.ERROR, []byte("bad kill request"))
    return
}
```

**Note:** `routes.go:540` (`json.Unmarshal(msg, &resize)`) is intentionally
unchecked — it's a "try parse, skip if not resize" pattern. Leave it.

### 23.3 Verification

```bash
# After fixes, this should return only the routes.go resize parse:
grep -rn 'json.Unmarshal' --include='*.go' | grep -v 'if err' | grep -v '_test.go'
```

---

## Part 24 — Resource Cleanup Safety in Create()

### 24.1 Problem

`Engine.Create()` has 8 copy-pasted cleanup blocks — each manually calling
`destroyTapDevice`, `e.pool.Release`, `os.RemoveAll`, `fcCmd.Process.Kill`,
`vmCancel`. If a new error path is added and one cleanup is missed, you
leak TAPs, IPs, or Firecracker processes.

### 24.2 Solution: Deferred Cleanup

```go
func (e *Engine) Create(ctx context.Context, spec engine.SandboxSpec) (info engine.SandboxInfo, err error) {
    id := generateID()
    sandboxDir := filepath.Join(e.cfg.DataDir, "sandboxes", id)
    os.MkdirAll(sandboxDir, 0700)

    var (
        guestIP   string
        tapName   string
        vmCancel  context.CancelFunc
        fcCmd     *exec.Cmd
    )
    defer func() {
        if err != nil {
            if fcCmd != nil && fcCmd.Process != nil {
                fcCmd.Process.Kill()
                fcCmd.Wait()
            }
            if vmCancel != nil {
                vmCancel()
            }
            if tapName != "" {
                destroyTapDevice(tapName)
            }
            if guestIP != "" {
                e.pool.Release(guestIP)
            }
            os.RemoveAll(sandboxDir)
        }
    }()

    // ... rest of Create() uses named return `err` ...
}
```

### 24.3 Verification

- `TestCreateFailureCleanup` — inject an invalid kernel path. Verify TAP
  is deleted, IP is released, sandbox dir is removed.

---

## Part 25 — Request Body Size Limits

### 25.1 Fix

```go
func readJSON(r *http.Request, v any) error {
    r.Body = http.MaxBytesReader(nil, r.Body, 1<<20) // 1MB
    defer r.Body.Close()
    return json.NewDecoder(r.Body).Decode(v)
}
```

### 25.2 Tests

- `TestRequestBodyTooLarge` — POST to `/sandboxes` with a 2MB body,
  verify 413 or 400 response, not a hang or OOM.

---

## Part 40 — Connection & API Timeouts

### 40.1 Problem

`dialControl()` and `dialForward()` call `net.Dial` with no timeout. If the
guest is unresponsive (kernel panic, CPU starvation, network partition),
the caller hangs forever. One hung VM blocks the thermal manager's
`Activity()` query, which runs sequentially over all sandboxes — a single
dead VM freezes thermal management for the entire fleet.

`fcAPIClient()` creates an `http.Client` with no `Timeout` and no dial
timeout. If Firecracker hangs, `Stop()`, `Pause()`, `Resume()` all hang
forever.

### 40.2 Agent Client: Context-Aware Dial

Thread context through all dial methods. Every caller already has a context
available.

```go
// pkg/agent/client.go

func (c *AgentClient) dialControl(ctx context.Context) (net.Conn, error) {
    var d net.Dialer
    var conn net.Conn
    var err error
    if c.tcpAddr != "" {
        conn, err = d.DialContext(ctx, "tcp", net.JoinHostPort(c.tcpAddr, fmt.Sprint(proto.VsockPortControl)))
    } else if c.isVsock {
        conn, err = c.dialVsockPort(ctx, c.controlSock, proto.VsockPortControl)
    } else {
        conn, err = d.DialContext(ctx, "unix", c.controlSock)
    }
    if err != nil {
        return nil, err
    }
    if err := c.sendAuth(conn); err != nil {
        conn.Close()
        return nil, err
    }
    return conn, nil
}
```

Update all callers: `Exec`, `Shell`, `SessionList`, `Activity`,
`SessionKill`, `ShellSession`, `SessionAttach`, `FileRead`, `FileWrite`,
`FileStat`, `FileList`. Each already receives `ctx context.Context`.

Export `DialControl` (capital D) for use by `ExecStream` in Part 35:

```go
func (c *AgentClient) DialControl(ctx context.Context) (net.Conn, error) {
    return c.dialControl(ctx)
}
```

### 40.3 Firecracker API Client: Timeout

```go
func fcAPIClient(socketPath string) *http.Client {
    return &http.Client{
        Timeout: 10 * time.Second,
        Transport: &http.Transport{
            DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
                return net.DialTimeout("unix", socketPath, 5*time.Second)
            },
        },
    }
}
```

### 40.4 Thermal Manager: Context with Timeout

The thermal cycle should not hang on a single sandbox. Add a per-sandbox
timeout:

```go
func (s *Server) runThermalCycle(te ThermalEngine, cfg ThermalConfig) {
    sandboxes, err := s.store.ListSandboxes()
    if err != nil { return }
    for _, sb := range sandboxes {
        // ... existing checks ...

        ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
        activity, err := te.Activity(ctx, sb.EngineID)
        cancel()
        if err != nil { continue }

        // ... rest unchanged ...
    }
}
```

### 40.5 Tests

- `TestDialControlTimeout` — point AgentClient at a non-listening address,
  use a context with 100ms timeout, verify error returns promptly.
- `TestThermalCycleUnresponsiveVM` — one responsive VM, one unresponsive.
  Verify thermal cycle completes for the responsive one within reasonable
  time (not blocked by the dead one).

---

## Part 41 — RestoreVM Type Assertion Safety

### 41.1 Problem

`RestoreVM` uses bare type assertions on a `map[string]interface{}` that
came from JSON unmarshaling:

```go
RootfsPath: state["rootfs_path"].(string),
CID:        uint32(state["vsock_cid"].(int)),      // JSON numbers are float64!
VcpuCount:  int64(state["vcpu_count"].(float64)),
MemSizeMib: int64(state["mem_size_mib"].(int)),     // also float64 from JSON!
```

This **will** panic in production. JSON unmarshals all numbers as `float64`.
The type assertions are inconsistent — `vsock_cid` asserts `int` (wrong),
`vcpu_count` asserts `float64` (right), `mem_size_mib` asserts `int` (wrong).
Whether it panics depends on whether the state was loaded from SQLite
(where Go's driver returns `int`) or from JSON (where everything is
`float64`). Daemon restart after crash → panic → restart loop.

### 41.2 Fix: Safe Extraction Helpers

```go
// pkg/engine/firecracker/engine.go

func stateStr(m map[string]interface{}, key string) string {
    if v, ok := m[key].(string); ok {
        return v
    }
    return ""
}

func stateInt(m map[string]interface{}, key string) int {
    switch v := m[key].(type) {
    case int:     return v
    case int64:   return int(v)
    case float64: return int(v)
    case uint32:  return int(v)
    }
    return 0
}

func stateInt64(m map[string]interface{}, key string) int64 {
    switch v := m[key].(type) {
    case int:     return int64(v)
    case int64:   return v
    case float64: return int64(v)
    case uint32:  return int64(v)
    }
    return 0
}

func stateUint32(m map[string]interface{}, key string) uint32 {
    switch v := m[key].(type) {
    case int:     return uint32(v)
    case int64:   return uint32(v)
    case float64: return uint32(v)
    case uint32:  return v
    }
    return 0
}
```

Rewrite `RestoreVM`:

```go
func (e *Engine) RestoreVM(id, name, status string, state map[string]interface{}) {
    vm := &VM{
        ID:         id,
        Name:       name,
        Status:     status,
        RootfsPath: stateStr(state, "rootfs_path"),
        SocketPath: stateStr(state, "socket_path"),
        VsockPath:  stateStr(state, "vsock_path"),
        CID:        stateUint32(state, "vsock_cid"),
        TapDevice:  stateStr(state, "tap_device"),
        GuestIP:    stateStr(state, "guest_ip"),
        GuestMAC:   stateStr(state, "guest_mac"),
        VcpuCount:  stateInt64(state, "vcpu_count"),
        MemSizeMib: stateInt64(state, "mem_size_mib"),
        SnapMemPath: stateStr(state, "snap_mem_path"),
        SnapVMPath:  stateStr(state, "snap_vm_path"),
    }
    // ... rest unchanged ...
}
```

### 41.3 Tests

- `TestRestoreVMFloat64` — pass state with all values as `float64`
  (simulating JSON unmarshal). Verify no panic, correct values.
- `TestRestoreVMInt` — pass state with values as `int` (simulating
  SQLite driver). Verify no panic, correct values.
- `TestRestoreVMMissingKeys` — pass incomplete state map. Verify no
  panic, zero-value defaults.

---

## Part 42 — Structured Logging (log/slog)

### 42.1 Motivation

The codebase uses `log.Printf` and `fmt.Fprintf(os.Stderr, ...)` everywhere.
Production infrastructure needs log levels, structured fields, and machine-
parseable output. No reason to pull in zerolog/zap — `log/slog` is in the
standard library since Go 1.21, and the project uses Go 1.25.

### 42.2 Logger Setup

```go
// cmd/bhatti/main.go

func setupLogger(level string, jsonOutput bool) {
    var lvl slog.Level
    switch strings.ToLower(level) {
    case "debug": lvl = slog.LevelDebug
    case "warn":  lvl = slog.LevelWarn
    case "error": lvl = slog.LevelError
    default:      lvl = slog.LevelInfo
    }

    opts := &slog.HandlerOptions{Level: lvl}
    var handler slog.Handler
    if jsonOutput {
        handler = slog.NewJSONHandler(os.Stderr, opts)
    } else {
        handler = slog.NewTextHandler(os.Stderr, opts)
    }
    slog.SetDefault(slog.New(handler))
}
```

Configuration in `config.yaml`:

```yaml
log_level: info     # debug | info | warn | error
log_format: text    # text | json
```

### 42.3 Migration Pattern

Replace `log.Printf` with structured slog calls. Key principle: every log
line includes the sandbox ID and operation name.

**Server — thermal manager:**

```go
// Before:
log.Printf("thermal: pause %s: %v", sb.Name, err)
log.Printf("thermal: %s → warm (idle %v)", sb.Name, idle.Round(time.Second))

// After:
slog.Warn("thermal pause failed", "sandbox", sb.Name, "error", err)
slog.Info("thermal transition", "sandbox", sb.Name, "from", "hot", "to", "warm",
    "idle", idle.Round(time.Second))
```

**Server — handlers:**

```go
// Before:
log.Printf("ws upgrade error: %v", err)
log.Printf("json encode error: %v", err)

// After:
slog.Error("websocket upgrade failed", "error", err)
slog.Error("json encode failed", "error", err)
```

**Server — request logging (middleware):**

Add a lightweight request logger. Not every request — only errors and
slow requests (>1s):

```go
func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Request) {
    start := time.Now()
    rw := &statusWriter{ResponseWriter: w}

    // ... existing auth check ...

    s.mux.ServeHTTP(rw, r)

    dur := time.Since(start)
    if rw.status >= 500 || dur > time.Second {
        slog.Warn("request",
            "method", r.Method,
            "path", r.URL.Path,
            "status", rw.status,
            "duration", dur.Round(time.Millisecond),
        )
    }
}

type statusWriter struct {
    http.ResponseWriter
    status int
}

func (w *statusWriter) WriteHeader(code int) {
    w.status = code
    w.ResponseWriter.WriteHeader(code)
}
```

**Engine — lifecycle events:**

```go
slog.Info("sandbox created", "id", id, "name", name, "cpus", vcpuCount, "memory_mb", memMB)
slog.Info("sandbox destroyed", "id", id)
slog.Info("snapshot created", "id", id, "type", snapshotType, "duration", dur)
slog.Debug("agent dial", "id", id, "transport", "tcp", "addr", guestIP)
```

**Lohar (guest agent):**

Lohar already logs to stderr which Firecracker captures. Replace
`fmt.Fprintf(os.Stderr, ...)` and `logf()` with slog. Guest logs are
debug-level — only visible when bhatti daemon is run with `--log-level=debug`.

```go
// Before:
logf("init session started (pid %d)", cmd.Process.Pid)

// After:
slog.Info("init session started", "pid", cmd.Process.Pid)
```

### 42.4 Files to Change

| File | `log.Printf` count | `fmt.Fprintf(os.Stderr` count |
|------|---------------------|-------------------------------|
| `pkg/server/server.go` | 3 | 0 |
| `pkg/server/routes.go` | 2 | 0 |
| `cmd/lohar/handler.go` | 1 | 1 |
| `cmd/lohar/tty.go` | 0 | 2 |
| `cmd/lohar/main.go` | 0 | ~5 |
| `cmd/bhatti/main.go` | ~3 | 0 |

### 42.5 Verification

```bash
# After migration, no bare log.Printf should remain:
grep -rn 'log.Printf' --include='*.go' | grep -v '_test.go'
# Expected: 0 results

# No logf() calls:
grep -rn 'logf(' --include='*.go' | grep -v '_test.go' | grep -v 'func logf'
# Expected: 0 results
```

---

## Part 43 — Health Check & Graceful Shutdown

### 43.1 Health Check Endpoint

```go
// pkg/server/routes.go

func (s *Server) routes() {
    s.mux.HandleFunc("/health", s.handleHealth)
    // ... existing routes ...
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
    sandboxes, _ := s.store.ListSandboxes()
    count := len(sandboxes)
    writeJSON(w, 200, map[string]any{
        "status":    "ok",
        "sandboxes": count,
        "uptime":    time.Since(s.startTime).Round(time.Second).String(),
    })
}
```

Add `startTime time.Time` to `Server` struct, set in `New()`.

No auth required on `/health` — add to `isStaticPath`:

```go
func isStaticPath(path string) bool {
    return path == "/" || path == "/index.html" ||
        path == "/health" || strings.HasPrefix(path, "/static/")
}
```

### 43.2 Graceful Shutdown

```go
// cmd/bhatti/main.go — serve command

httpServer := &http.Server{
    Addr:    cfg.ListenAddr,
    Handler: srv,
}

go func() {
    slog.Info("server listening", "addr", cfg.ListenAddr)
    if err := httpServer.ListenAndServe(); err != http.ErrServerClosed {
        slog.Error("server error", "error", err)
        os.Exit(1)
    }
}()

// Wait for SIGTERM/SIGINT
sigCh := make(chan os.Signal, 1)
signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)
sig := <-sigCh
slog.Info("shutting down", "signal", sig)

// Drain HTTP connections (5s timeout)
shutCtx, shutCancel := context.WithTimeout(context.Background(), 5*time.Second)
defer shutCancel()
httpServer.Shutdown(shutCtx)

// Stop background goroutines
srv.Close()

// Stop Firecracker engine (kill VMs, clean TAPs)
if fc, ok := eng.(*firecracker.Engine); ok {
    fc.Shutdown()
}

slog.Info("shutdown complete")
```

### 43.3 Tests

- `TestHealthEndpoint` — verify 200 response with expected fields.
- `TestHealthNoAuth` — verify health check works without auth token.

---

# Phase 2 — SDK Readiness

Driven by research into pi's agentic tool patterns (see `docs/pi-learnings.md`).
These changes make bhatti usable as a backend for coding agent frameworks.

## Part 35 — Streaming Exec (NDJSON)

### 35.1 Motivation

Pi's `BashOperations.exec` takes an `onData` callback that receives output
chunks as they arrive. Bhatti's current `POST /exec` buffers the entire
stdout/stderr and returns it all at once. For a `npm install` that takes
30 seconds, the consumer sees nothing until completion.

### 35.2 Design: Content-Negotiated NDJSON

Same endpoint, streaming when requested via `Accept` header. Zero breaking
changes. Falls back to existing buffered JSON response.

```
POST /sandboxes/:id/exec
Accept: application/x-ndjson
Content-Type: application/json

{"cmd": ["npm", "install"]}
```

Response (each line flushed immediately):

```
{"type":"stdout","data":"Installing dependencies...\n"}
{"type":"stderr","data":"npm warn deprecated ...\n"}
{"type":"stdout","data":"added 847 packages in 12s\n"}
{"type":"exit","exit_code":0}
```

### 35.3 Why NDJSON over SSE or WebSocket

**SSE** requires GET (browsers enforce this), but exec is semantically POST.
Would need a two-step dance (POST to create, GET to stream).

**WebSocket** adds a mandatory WS client dependency to every SDK consumer.
The existing `/ws` shell endpoint already handles the bidirectional case
(stdin, kill, resize). NDJSON handles the 95% case: fire a command, stream
output, get exit code.

**NDJSON on POST** works with vanilla HTTP everywhere. Python: `requests`
with `stream=True`. Node: `fetch` with `body.getReader()`. Go:
`bufio.Scanner` on `resp.Body`. Curl: `curl -N`.

### 35.4 Server Implementation

```go
// pkg/server/routes.go

func (s *Server) handleSandboxExec(w http.ResponseWriter, r *http.Request, id string) {
    // ... existing validation, ensureHot ...

    if r.Header.Get("Accept") == "application/x-ndjson" {
        s.handleSandboxExecStream(w, r, sb, req)
        return
    }
    // ... existing buffered path unchanged ...
}
```

New streaming handler:

```go
func (s *Server) handleSandboxExecStream(w http.ResponseWriter, r *http.Request, sb *store.Sandbox, req execReq) {
    flusher, ok := w.(http.Flusher)
    if !ok {
        errResp(w, 500, "streaming not supported")
        return
    }

    w.Header().Set("Content-Type", "application/x-ndjson")
    w.Header().Set("Transfer-Encoding", "chunked")
    w.Header().Set("X-Content-Type-Options", "nosniff")
    w.WriteHeader(200)

    enc := json.NewEncoder(w) // reuse encoder across all events

    se, ok := s.engine.(engine.StreamExecEngine)
    if !ok {
        // Fallback: buffer then emit as single NDJSON events
        result, err := s.engine.Exec(r.Context(), sb.EngineID, req.Cmd)
        if err != nil {
            enc.Encode(map[string]any{"type": "error", "data": err.Error()})
            flusher.Flush()
            return
        }
        if result.Stdout != "" {
            enc.Encode(map[string]any{"type": "stdout", "data": result.Stdout})
            flusher.Flush()
        }
        if result.Stderr != "" {
            enc.Encode(map[string]any{"type": "stderr", "data": result.Stderr})
            flusher.Flush()
        }
        enc.Encode(map[string]any{"type": "exit", "exit_code": result.ExitCode})
        flusher.Flush()
        return
    }

    // Streaming path: engine feeds us frames as they arrive
    se.ExecStream(r.Context(), sb.EngineID, req.Cmd, func(event engine.StreamEvent) {
        enc.Encode(event)
        flusher.Flush()
    })
}
```

### 35.5 Engine Interface Addition

```go
// pkg/engine/engine.go

// StreamEvent is emitted during streaming exec.
type StreamEvent struct {
    Type     string `json:"type"`               // "stdout", "stderr", "exit", "error"
    Data     string `json:"data,omitempty"`      // output text
    ExitCode *int   `json:"exit_code,omitempty"` // only for type="exit"
}

// StreamExecEngine is optionally implemented by engines that support
// streaming exec output.
type StreamExecEngine interface {
    ExecStream(ctx context.Context, id string, cmd []string, onEvent func(StreamEvent)) error
}
```

### 35.6 Firecracker Engine Implementation

The agent already sends STDOUT/STDERR/EXIT frames in a loop. Instead of
buffering, forward each frame as a StreamEvent:

```go
// pkg/engine/firecracker/engine.go

func (e *Engine) ExecStream(ctx context.Context, id string, cmd []string, onEvent func(engine.StreamEvent)) error {
    vm, err := e.getVM(id)
    if err != nil { return err }

    vm.stateMu.Lock()
    if vm.Thermal != "hot" {
        vm.stateMu.Unlock()
        return fmt.Errorf("sandbox %q is not hot", id)
    }
    ag := vm.Agent
    vm.stateMu.Unlock()

    conn, err := ag.DialControl(ctx)
    if err != nil { return err }
    defer conn.Close()

    if deadline, ok := ctx.Deadline(); ok {
        conn.SetDeadline(deadline)
    }

    req := proto.ExecRequest{Argv: cmd}
    if err := proto.SendJSON(conn, proto.EXEC_REQ, req); err != nil {
        return err
    }

    for {
        msgType, payload, err := proto.ReadFrame(conn)
        if err != nil { return err }
        switch msgType {
        case proto.STDOUT:
            onEvent(engine.StreamEvent{Type: "stdout", Data: string(payload)})
        case proto.STDERR:
            onEvent(engine.StreamEvent{Type: "stderr", Data: string(payload)})
        case proto.EXIT:
            code, _ := proto.ParseExitCode(payload)
            c := int(code)
            onEvent(engine.StreamEvent{Type: "exit", ExitCode: &c})
            return nil
        case proto.ERROR:
            onEvent(engine.StreamEvent{Type: "error", Data: string(payload)})
            return fmt.Errorf("agent: %s", payload)
        }
    }
}
```

### 35.7 Pi SDK Integration Example

```typescript
// How a pi BashOperations would consume the NDJSON stream:
const bhattiBashOps: BashOperations = {
  async exec(command, cwd, { onData, signal, timeout }) {
    const resp = await fetch(`${BHATTI_URL}/sandboxes/${id}/exec`, {
      method: "POST",
      headers: {
        "Accept": "application/x-ndjson",
        "Authorization": `Bearer ${token}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ cmd: ["sh", "-c", command] }),
      signal,
    });

    const reader = resp.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    let exitCode = null;

    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split("\n");
      buffer = lines.pop() || "";
      for (const line of lines) {
        if (!line.trim()) continue;
        const event = JSON.parse(line);
        if (event.type === "stdout" || event.type === "stderr") {
          onData(Buffer.from(event.data));
        } else if (event.type === "exit") {
          exitCode = event.exit_code;
        }
      }
    }
    return { exitCode };
  },
};
```

### 35.8 CLI Integration

The `bhatti exec` command can use the streaming endpoint to show live output
instead of waiting for completion:

```go
// cmd/bhatti/cli.go — cmdExec

// Request streaming
req.Header.Set("Accept", "application/x-ndjson")
resp, _ := http.DefaultClient.Do(req)
scanner := bufio.NewScanner(resp.Body)
var exitCode int
for scanner.Scan() {
    var event struct {
        Type     string `json:"type"`
        Data     string `json:"data"`
        ExitCode int    `json:"exit_code"`
    }
    json.Unmarshal(scanner.Bytes(), &event)
    switch event.Type {
    case "stdout":
        os.Stdout.WriteString(event.Data)
    case "stderr":
        os.Stderr.WriteString(event.Data)
    case "exit":
        exitCode = event.ExitCode
    }
}
os.Exit(exitCode)
```

### 35.9 Tests

- `TestExecStreamBasic` — `echo hello` via NDJSON, parse stream, verify
  stdout event + exit event with code 0.
- `TestExecStreamStderr` — `echo err >&2`, verify stderr event separate
  from stdout.
- `TestExecStreamLongRunning` — `for i in 1 2 3; do echo $i; sleep 0.5; done`.
  Verify events arrive incrementally (not all at once).
- `TestExecStreamExitCode` — `exit 42`, verify exit event has code 42.
- `TestExecStreamFallbackBuffered` — request without `Accept: application/x-ndjson`,
  verify existing buffered JSON response unchanged.
- `TestExecStreamDockerFallback` — Docker engine doesn't implement
  `StreamExecEngine`. Verify NDJSON still works (buffered then emitted
  as events).

---

## Part 36 — Server-Side File Read Truncation

### 36.1 Motivation

Pi's read tool truncates to 2000 lines / 50KB (whichever comes first).
Bhatti's `FileRead` currently transfers the **entire file** from guest to
host. A 100MB log file transfers 100MB through the wire protocol even
though the consumer truncates to 50KB. Server-side truncation avoids
2000x wasted bandwidth.

### 36.2 Protocol Change

Add optional `offset`, `limit`, and `max_bytes` fields to `FILE_READ_REQ`:

```go
// Sent from host to guest:
{
    "path": "/workspace/app.log",
    "offset": 1,        // 1-indexed line number, default 1
    "limit": 2000,      // max lines to return, default 0 (unlimited)
    "max_bytes": 51200   // max bytes to return, default 0 (unlimited)
}
```

Pi truncates at 2000 lines OR 50KB — whichever hits first. Supporting
both `limit` (line count) and `max_bytes` (byte budget) lets the SDK
pass both constraints and have lohar enforce them guest-side.

### 36.3 Lohar Handler Change

```go
// cmd/lohar/files.go — handleFileRead

func handleFileRead(conn net.Conn, payload []byte) {
    var req struct {
        Path     string `json:"path"`
        Offset   int    `json:"offset,omitempty"`    // 1-indexed, default 1
        Limit    int    `json:"limit,omitempty"`     // 0 = unlimited
        MaxBytes int    `json:"max_bytes,omitempty"` // 0 = unlimited
    }
    // ... existing validation ...

    // If limit or max_bytes is set, read line-by-line with truncation.
    if req.Limit > 0 || req.MaxBytes > 0 {
        handleFileReadTruncated(conn, f, info, req.Offset, req.Limit, req.MaxBytes)
        return
    }

    // ... existing full-file streaming path unchanged ...
}

func handleFileReadTruncated(conn net.Conn, f *os.File, info os.FileInfo, offset, limit, maxBytes int) {
    if offset < 1 { offset = 1 }

    // Send FILE_READ_RESP with full file size (so client knows total)
    proto.SendJSON(conn, proto.FILE_READ_RESP, map[string]any{
        "size": info.Size(),
        "mode": fmt.Sprintf("%04o", info.Mode().Perm()),
    })

    scanner := bufio.NewScanner(f)
    scanner.Buffer(make([]byte, 256*1024), 256*1024) // 256KB max line
    lineNum := 0
    sentLines := 0
    sentBytes := 0

    for scanner.Scan() {
        lineNum++
        if lineNum < offset { continue }
        if limit > 0 && sentLines >= limit { break }

        line := scanner.Bytes()
        // Append newline (scanner strips it)
        out := make([]byte, len(line)+1)
        copy(out, line)
        out[len(line)] = '\n'

        // Check byte budget before sending
        if maxBytes > 0 && sentBytes+len(out) > maxBytes {
            // Send partial line up to budget, then stop
            remaining := maxBytes - sentBytes
            if remaining > 0 {
                proto.WriteFrame(conn, proto.STDOUT, out[:remaining])
            }
            break
        }

        proto.WriteFrame(conn, proto.STDOUT, out)
        sentLines++
        sentBytes += len(out)
    }

    exit := proto.ExitPayload(0)
    proto.WriteFrame(conn, proto.EXIT, exit[:])
}
```

### 36.4 API Change

The HTTP endpoint passes query parameters through:

```
GET /sandboxes/:id/files?path=/app.log&offset=1&limit=2000&max_bytes=51200
```

```go
// pkg/server/routes.go — handleSandboxFiles GET case

offset, _ := strconv.Atoi(r.URL.Query().Get("offset"))
limit, _ := strconv.Atoi(r.URL.Query().Get("limit"))
maxBytes, _ := strconv.Atoi(r.URL.Query().Get("max_bytes"))
```

These get threaded through `FileEngine.FileRead` to the agent client.

### 36.5 Agent Client & Engine Interface Change

```go
// pkg/agent/client.go

func (c *AgentClient) FileRead(ctx context.Context, path string, w io.Writer,
    offset, limit, maxBytes int) (size int64, mode string, err error) {
    // ... existing dial + auth ...
    if err := proto.SendJSON(conn, proto.FILE_READ_REQ, map[string]any{
        "path": path, "offset": offset, "limit": limit, "max_bytes": maxBytes,
    }); err != nil {
        return 0, "", err
    }
    // ... existing read loop unchanged ...
}
```

```go
// pkg/server/routes.go — FileEngine interface update

type FileEngine interface {
    FileRead(ctx context.Context, id, path string, w io.Writer,
        offset, limit, maxBytes int) (int64, string, error)
    // ... rest unchanged ...
}
```

### 36.6 Backward Compatibility

`offset=0`, `limit=0`, `max_bytes=0` (the defaults) trigger the existing
full-file streaming path in lohar. No behavior change for callers that
don't pass these parameters.

### 36.7 Tests

- `TestFileReadWithLimit` — write 100-line file, read with `limit=10`,
  verify exactly 10 lines returned.
- `TestFileReadWithOffset` — write 100-line file, read with `offset=50`,
  verify lines 50–100 returned.
- `TestFileReadWithOffsetAndLimit` — `offset=50, limit=10`, verify lines
  50–59 returned.
- `TestFileReadWithMaxBytes` — write 100-line file (100 bytes each), read
  with `max_bytes=500`, verify ~5 lines returned (byte-truncated).
- `TestFileReadLimitAndMaxBytes` — `limit=2000, max_bytes=500`, verify
  `max_bytes` wins when hit first.
- `TestFileReadLimitZero` — `limit=0` streams entire file (backward compat).
- `TestFileReadOffsetBeyondEOF` — `offset=9999` on a 100-line file,
  verify empty content, no error.
- `TestFileReadLargeFileWithLimit` — write 100K-line file, read with
  `limit=2000`. Verify only 2000 lines transferred (check byte count).
- `TestFileReadHTTPQueryParams` — `GET /files?path=...&offset=10&limit=50`
  via HTTP, verify correct lines returned.

---

## Part 37 — Rootfs Tooling (rg and fd)

### 37.1 Motivation

Pi's grep tool uses `ripgrep` (rg) and find tool uses `fd-find` (fd).
These are 10–100x faster than system `grep`/`find` on large codebases.
Agents use them constantly.

### 37.2 Change

Add to `scripts/build-rootfs.sh` inside the chroot:

```bash
# Ripgrep + fd-find (used by coding agents for fast search)
apt-get install -y --no-install-recommends ripgrep fd-find

# fd-find installs as 'fdfind' on Ubuntu, symlink to 'fd'
ln -sf /usr/bin/fdfind /usr/local/bin/fd
```

### 37.3 Verification

After rootfs rebuild:

```bash
bhatti exec <sandbox> -- rg --version
bhatti exec <sandbox> -- fd --version
```

---

## Part 38 — Abort In-Flight File Operations

### 38.1 Motivation

Pi's tools all support `AbortSignal` for cancellation. Bhatti's file
operations can't be cancelled mid-stream. A `FileRead` of a 100MB file
runs to completion even if the client disconnects.

### 38.2 Solution

Lohar already closes the connection on host disconnect. The
`proto.WriteFrame(conn, proto.STDOUT, ...)` call returns an error
(broken pipe) when the host has closed its end. The for loop breaks on
write error. This already works on the guest side.

The issue is on the **host side**: the `AgentClient.FileRead` method
reads until EXIT, but if the caller's context is cancelled, the
connection should be closed immediately.

```go
// pkg/agent/client.go — FileRead

func (c *AgentClient) FileRead(ctx context.Context, path string, w io.Writer,
    offset, limit, maxBytes int) (size int64, mode string, err error) {
    conn, err := c.dialControl(ctx)
    if err != nil { return 0, "", err }
    defer conn.Close()

    // Close connection on context cancellation — this makes the lohar
    // write fail with broken pipe, stopping the transfer.
    go func() {
        <-ctx.Done()
        conn.Close()
    }()

    // ... rest unchanged ...
}
```

Note: `dialControl(ctx)` depends on Part 40 (context-aware dial).

### 38.3 Tests

- `TestFileReadAbort` — start reading a large file, cancel context
  after 10ms, verify the read returns quickly (not after full transfer).

---

## Part 39 — Process Group Kill on Abort

### 39.1 Motivation

Pi kills the **entire process tree** on abort:
`process.kill(-child.pid, SIGKILL)`. Bhatti's KILL frame sends `SIGTERM`
to the session's direct process. Child processes (e.g., a shell running
`npm install` which spawns `node`) survive.

### 39.2 Change: Non-TTY Piped Exec

Add `Setpgid: true` so the child process gets its own process group,
then kill the entire group on KILL:

```go
// cmd/lohar/exec.go — handlePipedExec

cmd := exec.Command(req.Argv[0], req.Argv[1:]...)
cmd.SysProcAttr = &syscall.SysProcAttr{
    Setpgid: true,
}
```

```go
// KILL handling in piped exec
case proto.KILL:
    if cmd.Process != nil {
        // Kill entire process group, not just the shell
        syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
    }
    return
```

### 39.3 Change: TTY Sessions

For TTY sessions, the process group is already set up by `Setsid: true`
(new session = new process group). Update the KILL handler in
`readHostInput` to use process group kill:

```go
// cmd/lohar/tty.go — readHostInput

case proto.KILL:
    sess.mu.Lock()
    if sess.Cmd != nil && sess.Cmd.Process != nil {
        syscall.Kill(-sess.Cmd.Process.Pid, syscall.SIGKILL)
    }
    sess.mu.Unlock()
    return
```

Also update the `EXEC_KILL` handler in `handler.go`:

```go
case proto.EXEC_KILL:
    // ... existing session lookup ...
    s.mu.Lock()
    if s.Cmd != nil && s.Cmd.Process != nil {
        syscall.Kill(-s.Cmd.Process.Pid, syscall.SIGKILL)
    }
    s.mu.Unlock()
```

### 39.4 Tests

- `TestKillProcessGroup` — exec `sh -c "sleep 3600 & echo $!; wait"`,
  read the child PID from stdout, send KILL, verify both the shell AND
  the sleep are dead (`kill -0 $childPid` fails).

---

# Phase 3 — Performance

## Part 26 — Diff Snapshots

### 26.1 Background

Full snapshots write the entire VM memory to disk. 512MB VM on Pi 5 SD
card (~40 MB/s) = ~12 seconds. Diff snapshots write only dirty pages
since the last snapshot. Idle sandbox diffs: ~10–50MB → ~0.5–2s.

### 26.2 VM State Change

```go
type VM struct {
    // ... existing fields ...
    hasBaseSnapshot bool  // true after first Full snapshot
}
```

### 26.3 Stop() Change

```go
snapshotType := "Full"
if vm.hasBaseSnapshot {
    // Verify base snapshot files exist — if deleted/corrupted, fall back to Full.
    if _, err := os.Stat(vm.SnapMemPath); err != nil {
        slog.Warn("base snapshot missing, falling back to full",
            "id", id, "path", vm.SnapMemPath, "error", err)
        vm.hasBaseSnapshot = false
    } else {
        snapshotType = "Diff"
    }
}

if err := fcPut(client, "/snapshot/create", fmt.Sprintf(
    `{"snapshot_type":%q,"snapshot_path":%q,"mem_file_path":%q}`,
    snapshotType, vm.SnapVMPath, vm.SnapMemPath)); err != nil {
    return fmt.Errorf("create snapshot: %w", err)
}

if !vm.hasBaseSnapshot {
    vm.hasBaseSnapshot = true
}
```

### 26.4 Persistence

```sql
ALTER TABLE sandboxes ADD COLUMN has_base_snapshot INTEGER DEFAULT 0;
```

### 26.5 Tests

- `TestDiffSnapshot` — full snapshot, resume, diff snapshot. Compare
  mem.snap sizes. Resume from diff, verify all data present.
- `TestDiffSnapshotMultipleCycles` — full → diff → diff → diff. All
  data preserved. Measure latencies.
- `TestDiffSnapshotAfterDestroy` — new VM starts with
  `hasBaseSnapshot=false`.
- `TestDiffSnapshotMissingBase` — delete base mem.snap between cycles,
  verify fallback to Full (no error, just a warning log).

---

## Part 27 — Frame Write Allocation Reduction

### 27.1 Rationale

Measure before optimizing. Run benchmarks from Part 33 first. If
`WriteFrame` is not in the hot path (likely — agent sessions do ~100
frames/sec), skip this part. If it shows up in profiles, proceed.

### 27.2 Solution: sync.Pool for Small Frames

```go
var framePool = sync.Pool{
    New: func() any {
        buf := make([]byte, 8192)
        return &buf
    },
}

func WriteFrame(w io.Writer, msgType byte, payload []byte) error {
    frameLen := 1 + len(payload)
    if frameLen > MaxFrameSize {
        return fmt.Errorf("frame too large: %d > %d", frameLen, MaxFrameSize)
    }
    totalLen := 4 + frameLen

    var buf []byte
    if totalLen <= 8192 {
        bp := framePool.Get().(*[]byte)
        buf = (*bp)[:totalLen]
        defer framePool.Put(bp)
    } else {
        buf = make([]byte, totalLen)
    }

    binary.BigEndian.PutUint32(buf[0:4], uint32(frameLen))
    buf[4] = msgType
    copy(buf[5:], payload)
    _, err := w.Write(buf)
    return err
}
```

### 27.3 Tests

- `BenchmarkWriteFrame1KB` — target: 0 allocs for ≤8KB frames.
- `BenchmarkWriteFrame32KB` — verify no regression.
- `BenchmarkFrameRoundTrip` — 100K frames, measure MB/s.

---

## Part 28 — Ring Buffer Rewrite

### 28.1 Problem

Current ring buffer writes byte-by-byte:

```go
for _, b := range p {
    r.buf[r.w] = b
    r.w = (r.w + 1) % r.size
    if r.w == 0 { r.full = true }
}
```

For 64KB scrollback this is fine functionally, but the `copy()`-based
version is cleaner and avoids the per-byte modulo.

### 28.2 Solution: copy()-Based Write

```go
func (r *ringBuffer) Write(p []byte) (int, error) {
    n := len(p)
    if n >= r.size {
        // Data larger than buffer — keep only the last r.size bytes
        copy(r.buf, p[n-r.size:])
        r.w = 0
        r.full = true
        return n, nil
    }

    // How much fits before wrap?
    first := r.size - r.w
    if first > n { first = n }
    copy(r.buf[r.w:], p[:first])
    if first < n {
        copy(r.buf, p[first:])
    }

    oldW := r.w
    r.w = (r.w + n) % r.size
    // Buffer is full if we wrapped past where we started writing
    if !r.full {
        r.full = (r.w <= oldW) && n > 0
    }
    return n, nil
}
```

### 28.3 Tests

- `BenchmarkRingBufferWrite4KB` / `BenchmarkRingBufferWrite32KB`
- `TestRingBufferExactFill` — write exactly `size` bytes, verify
  `full=true`, `Bytes()` correct.
- `TestRingBufferOverwriteMultiple` — write 3x buffer size, verify
  only last `size` bytes remain.
- `TestRingBufferPartialFill` — write less than `size`, verify
  `full=false`, `Bytes()` correct.

---

## Part 29 — Thermal Manager Tuning

### 29.1 Problem

The thermal manager queries every running sandbox's agent every 10 seconds.
`Activity()` opens a TCP connection to the guest agent, sends `ACTIVITY_REQ`,
reads `ACTIVITY_RESP`. For 50 sandboxes, that's 50 TCP connections per cycle.
This doesn't scale and can block if a VM is unresponsive (fixed by Part 40
timeouts, but still wasteful).

### 29.2 Host-Side Activity Cache

```go
type Server struct {
    // ... existing fields ...
    lastActivity sync.Map  // engineID → time.Time
}

func (s *Server) touchActivity(engineID string) {
    s.lastActivity.Store(engineID, time.Now())
}
```

Call `touchActivity` at the top of every handler that calls `ensureHot`:
`handleSandboxExec`, `handleSandboxWS`, `handleSandboxFiles`,
`handleSandboxPorts`, `handleSandboxSessions`, `handleSandboxProxyRoute`.

In the thermal cycle, check the host-side timestamp first:

```go
func (s *Server) runThermalCycle(te ThermalEngine, cfg ThermalConfig) {
    sandboxes, err := s.store.ListSandboxes()
    if err != nil { return }

    for _, sb := range sandboxes {
        if sb.Status != "running" { continue }

        thermal := te.ThermalState(sb.EngineID)
        if thermal == "cold" || thermal == "" { continue }

        // Fast path: check host-side cache first. If the sandbox had
        // API activity within warmTimeout, skip the agent query entirely.
        if ts, ok := s.lastActivity.Load(sb.EngineID); ok {
            if time.Since(ts.(time.Time)) < cfg.WarmTimeout {
                continue // definitely active, skip agent query
            }
        }

        // Slow path: ask the agent for authoritative activity info.
        ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
        activity, err := te.Activity(ctx, sb.EngineID)
        cancel()
        if err != nil {
            slog.Warn("thermal activity query failed",
                "sandbox", sb.Name, "error", err)
            continue
        }

        idle := time.Since(time.Unix(activity.LastActivityUnix, 0))

        if thermal == "hot" && idle > cfg.WarmTimeout && activity.AttachedSessions == 0 {
            if err := te.Pause(context.Background(), sb.EngineID); err != nil {
                slog.Warn("thermal pause failed", "sandbox", sb.Name, "error", err)
                continue
            }
            slog.Info("thermal transition", "sandbox", sb.Name,
                "from", "hot", "to", "warm", "idle", idle.Round(time.Second))
        }

        if thermal == "warm" && idle > cfg.ColdTimeout {
            if err := s.engine.Stop(context.Background(), sb.EngineID); err != nil {
                slog.Warn("thermal snapshot failed", "sandbox", sb.Name, "error", err)
                continue
            }
            s.saveVMState(sb.ID, sb.EngineID)
            slog.Info("thermal transition", "sandbox", sb.Name,
                "from", "warm", "to", "cold", "idle", idle.Round(time.Second))
        }
    }
}
```

### 29.3 Tests

- `TestThermalCycleSkipsActiveSandbox` — touch activity, verify no
  agent query during thermal cycle.
- `TestThermalCycleQueriesIdleSandbox` — don't touch activity, verify
  agent is queried.

---

# Phase 4 — Test Coverage

## Part 30 — HTTP/WS Proxy Route Tests

### 30.1 Test Plan

**`pkg/server/proxy_route_test.go`:**

- `TestProxyHTTPGet` — mock HTTP server, proxy through bhatti, verify
  response body and headers.
- `TestProxyHTTPPost` — POST with body, verify forwarded correctly.
- `TestProxyHTTPHeaders` — custom headers round-trip.
- `TestProxyHTTP404` — 404 forwarded (not 502).
- `TestProxyHTTPLargeBody` — 1MB response, no truncation.
- `TestProxyWSUpgrade` — WS echo server through proxy.
- `TestProxyWSBidirectional` — 100 messages each direction.
- `TestProxyWSSandboxNotFound` — 404, not panic.
- `TestProxyInvalidPort` — `/proxy/abc/`, verify 400.
- `TestProxyEnsureHot` — cold sandbox, proxy wakes it.

---

## Part 31 — Daemon Recovery Tests

### 31.1 Test Plan

Extract recovery logic into engine package. Tests (no root needed):

- `TestRecoverStoppedWithSnapshot` — verify VM in engine map.
- `TestRecoverStoppedMissingSnapshot` — verify marked `unknown`.
- `TestRecoverRunningWithSnapshot` — verify marked `stopped`.
- `TestRecoverRunningNoSnapshot` — verify marked `unknown`.
- `TestRecoverCIDConflict` — verify `nextCID` correct.
- `TestRecoverIPPoolReservation` — verify IPs reserved.
- `TestRecoverTypeCoercion` — verify `float64` from JSON handled
  (depends on Part 41 type-safe helpers).

---

## Part 32 — Store Firecracker State Round-Trip Test

- `TestFirecrackerStateRoundTrip` — save + load, verify exact equality.
- `TestFirecrackerStateDefaults` — Docker sandbox returns zero values.
- `TestFirecrackerStateUpdate` — save twice, verify latest values.

---

## Part 33 — Go Benchmarks (testing.B)

**`pkg/agent/proto/frame_bench_test.go`:**
- `BenchmarkWriteFrame1KB` / `BenchmarkWriteFrame32KB`
- `BenchmarkReadFrame1KB`
- `BenchmarkFrameRoundTrip100K`

**`cmd/lohar/session_bench_test.go`:**
- `BenchmarkRingBufferWrite4KB` / `BenchmarkRingBufferWrite32KB`
- `BenchmarkRingBufferBytes`

```bash
go test -bench=. -benchmem -count=5 ./pkg/agent/proto/ > old.txt
# ... make changes ...
go test -bench=. -benchmem -count=5 ./pkg/agent/proto/ > new.txt
benchstat old.txt new.txt
```

---

## Part 34 — Performance Workload Tests

All on real Firecracker VMs (`pkg/engine/firecracker/perf_test.go`):

- `TestPerfSmallFileLatency` — 100 × 1KB write + read, report p50/p95/p99.
- `TestPerfExecPercentiles` — 100 × `true`, report p50/p95/p99/max.
- `TestPerfVMLifecycleStress` — 5 VMs in parallel: boot → exec → snap →
  restore → verify.
- `TestPerfDiffVsFullSnapshot` — full snapshot then diff snapshot, compare
  times and mem.snap sizes.
- `TestPerfWarmExecHTTPLatency` — full HTTP round-trip on warm sandbox,
  report p50/p95.
- `TestPerfParallelFileReads` — 5 concurrent 1KB file reads (common
  agentic pattern), report max latency.
- `TestPerfStreamExecLatency` — NDJSON exec of `echo hello`, measure
  time-to-first-byte vs time-to-exit.

---

## Summary

| Phase | Part | Description | Priority |
|-------|------|-------------|----------|
| 1 | 21 | WS auth bypass fix | 🔴 Critical |
| 1 | 22 | Per-VM mutex for state races | 🔴 Critical |
| 1 | 23 | Check json.Unmarshal errors | 🔴 Critical |
| 1 | 24 | Create() deferred cleanup | 🔴 Critical |
| 1 | 25 | Request body size limits | 🔴 Critical |
| 1 | 40 | Connection & API timeouts | 🔴 Critical |
| 1 | 41 | RestoreVM type assertion safety | 🔴 Critical |
| 1 | 42 | Structured logging (log/slog) | 🟡 Important |
| 1 | 43 | Health check & graceful shutdown | 🟡 Important |
| 2 | 35 | Streaming exec (NDJSON) | 🟡 Important |
| 2 | 36 | Server-side file read truncation | 🟡 Important |
| 2 | 37 | Rootfs tooling (rg, fd) | 🟡 Important |
| 2 | 38 | File operation abort | 🟠 Moderate |
| 2 | 39 | Process group kill | 🟡 Important |
| 3 | 26 | Diff snapshots | 🟡 Important |
| 3 | 27 | Frame write allocation pool | 🟠 Moderate |
| 3 | 28 | Ring buffer copy() rewrite | 🟠 Moderate |
| 3 | 29 | Thermal manager host-side cache | 🟡 Important |
| 4 | 30 | HTTP/WS proxy route tests | 🟡 Important |
| 4 | 31 | Daemon recovery tests | 🟡 Important |
| 4 | 32 | Store FC state round-trip test | 🟠 Moderate |
| 4 | 33 | Go benchmarks (testing.B) | 🟠 Moderate |
| 4 | 34 | Perf workload tests | 🟠 Moderate |

### What changed from the original plan

1. **Phase 2 (SDK) and Phase 3 (performance) swapped.** External users hit
   the API first. If `bhatti exec` produces 30s of silence or file reads
   transfer 100MB to truncate to 50KB, the first impression is broken.
   Diff snapshots are invisible; streaming exec is not.

2. **Part 40 added (connection timeouts).** `dialControl()` and
   `fcAPIClient()` had no timeouts. One hung VM blocks the entire thermal
   manager. Production blocker.

3. **Part 41 added (RestoreVM type safety).** Bare type assertions on
   `map[string]interface{}` will panic when the state comes from JSON
   (all numbers are `float64`) vs SQLite (numbers are `int`). Daemon
   restart after crash → panic → restart loop.

4. **Part 42 added (structured logging).** `log/slog` migration. Replaces
   all `log.Printf` and `fmt.Fprintf(os.Stderr, ...)` with structured,
   leveled logging. Zero new dependencies (stdlib since Go 1.21).

5. **Part 43 added (health check + graceful shutdown).** `GET /health` for
   deployment probes. `http.Server.Shutdown()` to drain connections on
   SIGTERM instead of hard-killing in-flight requests.

6. **Part 24 upgraded to 🔴 Critical.** 8 copy-pasted cleanup blocks in
   `Create()` is the highest-risk code for resource leaks.

7. **Part 36 expanded with `max_bytes`.** Pi truncates at 2000 lines OR
   50KB, whichever comes first. Original plan only had line-level
   truncation. Added byte-budget parameter for parity.

8. **Part 26 (diff snapshots) gains missing-base fallback.** If
   `hasBaseSnapshot=true` but the file is deleted/corrupted, fall back
   to Full instead of erroring.

9. **Part 27 (frame pool) gated on benchmarks.** Measure first with Part
   33 benchmarks. If `WriteFrame` isn't in the hot path (~100 frames/sec),
   skip the pool complexity.

10. **Part 28 (ring buffer) `full` flag logic fixed.** Original had a
    subtle bug in wrap detection. Replaced with simpler comparison.

11. **Part 35 (streaming exec) fixes.** Reuse `json.Encoder` across events
    instead of creating a new one per event. Added `Transfer-Encoding:
    chunked` header to prevent proxy buffering.

12. **Part 39 expanded.** Added TTY session kill and `EXEC_KILL` handler
    changes alongside the piped exec fix. All three kill paths need
    process group kill for consistency.

### Execution order

```
Day 1:   Parts 21, 23, 25, 40         security fixes + timeouts
Day 2:   Part 22                       per-VM mutex (structural, needs care)
Day 3:   Parts 24, 41, 42             cleanup, RestoreVM safety, slog migration
Day 4:   Parts 35, 43                 streaming exec, health/graceful shutdown
Day 5:   Part 36                       server-side file truncation
Day 6:   Parts 37, 39                 rootfs tooling + process group kill
Day 7:   Part 38                       file abort
Week 2:  Parts 26–29                  performance
Week 2+: Parts 30–34                  test coverage (ongoing as features land)
```
