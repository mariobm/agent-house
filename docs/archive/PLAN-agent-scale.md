# Scaling to 50 Agents — Reliability + Declarative Config

Server: agni-01 (Ryzen 9 3900, 128GB RAM, 1.7TB NVMe)
Current: 3 hot keep_hot sandboxes, 10 cold. v1.1.1.
Target: 50 keep_hot agents for SPC on agni-01 (+ second server later).

Two tracks, no dependencies between them. Ship in either order.

---

## What breaks at 50 agents

With the current system, 50 keep_hot agents would work — the hardware
fits it (50 × 1GB = 50GB of 128GB, 50 vCPUs with cgroup limits on 24
threads). But five things become painful:

1. **Agent crashes silently.** hermes gateway dies at 3am, VM stays hot,
   nobody notices until Slack goes quiet. `bhatti exec rory -- pgrep hermes`
   is the only check. Doesn't scale past 3 agents.

2. **Manual recovery.** The nohup hack (`nohup hermes gateway &`) doesn't
   restart on crash. Kowshik's runbook has 10 manual steps per agent. At
   50 agents, one bad deploy means 50 manual recoveries.

3. **No reproducible setup.** Agent config lives in shell history and a
   markdown doc. Creating agent #51 means reading the doc and running
   10 commands. No diffing, no version control, no "what changed?"

4. **SnapshotAll timeout.** 50 hot VMs × 30s each / 10 concurrent = 150s.
   systemd TimeoutStopSec is 120s. Some VMs get SIGKILLed without snapshot.

5. **Thermal cycle is sequential.** One slow agent query (5s timeout) blocks
   evaluation of all other sandboxes. At 50 hot sandboxes with 10s tick
   interval, a few timeouts cause the cycle to back up.

### What does NOT break

- **RAM.** 50 × 1GB = 50GB. 128GB available. Plenty of headroom.
- **CPU.** Agents are idle 95% of the time. cgroup cpu.max (already set
  by jailer) prevents any single VM from monopolizing.
- **IP space.** /24 per user = 253 IPs. 50 agents for one user is fine.
- **SQLite.** WAL mode handles 50 concurrent reads. Writes are serialized
  but each is microseconds. Not a bottleneck until 500+.
- **Snapshot/resume.** The reliability fixes in v1.1.0 handle volumes
  correctly through checkpoint → resume → stop → start cycles.

---

## Track A: Reliability at Scale

Make 50 keep_hot agents run without manual babysitting.

### A.1 Init restart policy

**Problem:** `hermes gateway` started via `--init` is fire-and-forget.
If the process exits (crash, OOM, unhandled exception), it stays dead.
Today kowshik runs the gateway with nohup in a manual exec, not via
init, specifically because init has no restart.

**Change:** Add `init_restart` field to sandbox spec. Lohar watches the
init process and restarts it on exit.

**API:**

```json
POST /sandboxes
{
  "init": "hermes gateway run",
  "init_restart": "on-failure",
  "keep_hot": true
}
```

Values:
- `never` — default, current behavior
- `on-failure` — restart if exit code != 0, up to 5 times with backoff
- `always` — restart on any exit, up to 5 times with backoff

Backoff: 1s, 2s, 4s, 8s, 16s. After 5 consecutive failures within
10 minutes, stop restarting and mark the session as `crashed`. Reset
the counter if the process runs for >10 minutes (it recovered).

**Files:**

`cmd/lohar/tty.go` — `runInitSession()`:
```go
func runInitSession(script, user string, restart string) {
    for attempt := 0; ; attempt++ {
        sess := createAndRunInit(script, user)
        exitCode := <-sess.Done

        if restart == "never" {
            return
        }
        if restart == "on-failure" && exitCode == 0 {
            return
        }
        if attempt >= 5 {
            logf("init: giving up after %d restarts", attempt)
            return
        }
        delay := time.Duration(1<<attempt) * time.Second
        if delay > 16*time.Second {
            delay = 16 * time.Second
        }
        logf("init: exited %d, restarting in %v (attempt %d/5)",
            exitCode, delay, attempt+1)
        time.Sleep(delay)
    }
}
```

`pkg/engine/firecracker/configdrive.go` — add `InitRestart` to SandboxConfig:
```go
type SandboxConfig struct {
    // ... existing fields ...
    InitRestart string `json:"init_restart,omitempty"` // "never", "on-failure", "always"
}
```

`cmd/lohar/main.go` — pass restart policy:
```go
if cfg != nil && cfg.Init != "" {
    go runInitSession(cfg.Init, cfg.User, cfg.InitRestart)
}
```

`pkg/server/sandbox_handlers.go` — accept in create request:
```go
type createRequest struct {
    // ... existing fields ...
    InitRestart string `json:"init_restart,omitempty"`
}
```

`cmd/bhatti/sandbox_cmd.go` — CLI flag:
```
--init-restart string   Restart policy for init (never|on-failure|always)
```

**Tests:**

- `TestInitRestartOnFailure` — init script `exit 1`, verify lohar
  re-execs, visible via `SessionList` showing init session alive.
- `TestInitRestartAlwaysOnCleanExit` — init script `exit 0` with
  `always` policy, verify re-exec.
- `TestInitRestartNeverDefault` — init script `exit 1` with no
  restart policy, verify session stays dead.
- `TestInitRestartBackoff` — init script that exits immediately
  5 times, verify increasing delay between restarts, verify gives
  up after 5 attempts.
- `TestInitRestartResetAfterStable` — init runs for 15s then crashes,
  verify counter resets (not counted as rapid failure).

### A.2 Health checks

**Problem:** Even with init restart, we need to know when an agent is
unhealthy. The init process might be alive but wedged (hermes connected
but not processing messages). And non-init processes (servers, cron jobs)
aren't covered by init restart.

**Change:** Optional health check per sandbox. The server runs checks
on a separate goroutine (not the thermal cycle — health checks are
for keep_hot sandboxes, thermal is for non-keep_hot).

**API:**

```json
POST /sandboxes
{
  "health_check": {
    "cmd": ["pgrep", "-f", "hermes gateway"],
    "interval_seconds": 60,
    "timeout_seconds": 10,
    "retries": 3
  }
}
```

On 3 consecutive failures:
- Log an error with sandbox name and last check output
- Set `health_status` to `unhealthy` in the store
- POST to a configurable webhook URL (if set) — SPC wires this to Slack

On recovery (check passes after being unhealthy):
- Set `health_status` back to `healthy`
- POST recovery webhook

**Store change:** Add `health_check_json TEXT` and `health_status TEXT`
columns to sandboxes table.

**Server change:** New `healthCheckLoop` goroutine, started alongside
`StartThermalManager`. Iterates keep_hot sandboxes with health checks
defined. Runs `engine.Exec(ctx, id, cmd)` with the timeout. Tracks
consecutive failure count in `sync.Map` (same pattern as thermal
failure tracking).

**Metrics:** `/metrics` gains:
```json
{
  "health": {
    "healthy": 48,
    "unhealthy": 2,
    "no_check": 5
  }
}
```

**CLI:**
```bash
bhatti create --name rory \
  --init "hermes gateway run" \
  --init-restart on-failure \
  --health-cmd "pgrep -f 'hermes gateway'" \
  --health-interval 60 \
  --keep-hot
```

**Tests:**

- `TestHealthCheckPassesOnHealthy` — sandbox with init running, health
  check succeeds, status is "healthy".
- `TestHealthCheckFailsOnCrash` — kill init process, health check fails
  3 times, status becomes "unhealthy".
- `TestHealthCheckRecovery` — after restart (via init_restart), health
  check passes, status returns to "healthy".
- Unit tests for the health check goroutine with mock engine.

### A.3 Parallel thermal cycle

**Problem:** `runThermalCycle` iterates sandboxes sequentially. One
slow `Activity()` query (5s timeout on unreachable agent) blocks all
subsequent evaluations.

**Change:** Evaluate sandboxes in parallel with bounded concurrency.

**`pkg/server/server.go` — `runThermalCycle()`:**

```go
func (s *Server) runThermalCycle(te ThermalEngine, cfg ThermalConfig) {
    sandboxes, err := s.store.ListAllSandboxes()
    if err != nil {
        return
    }

    sem := make(chan struct{}, 10)
    var wg sync.WaitGroup
    for _, sb := range sandboxes {
        if sb.Status != "running" || sb.KeepHot {
            continue
        }
        wg.Add(1)
        go func(sb store.Sandbox) {
            defer wg.Done()
            sem <- struct{}{}
            defer func() { <-sem }()
            s.thermalEvalOne(te, cfg, sb)
        }(sb)
    }
    wg.Wait()
}
```

Extract the per-sandbox logic into `thermalEvalOne` — same code, just
moved into a method. No logic change.

This is 30 minutes of work. The pattern already exists in `SnapshotAll`.

**Tests:** `TestConcurrentThermalCycle` — mock engine, 20 sandboxes,
verify all are evaluated within one tick interval even when 5 have
slow activity responses.

### A.4 SnapshotAll timeout

**Problem:** 50 hot VMs × 30s / 10 concurrent = 150s. TimeoutStopSec
is 120s.

**Changes:**
1. Bump `TimeoutStopSec=300` in bhatti.service (install script change)
2. Add progress logging to SnapshotAll:
```go
slog.Info("snapshot-all: progress",
    "done", snapped.Load()+failed.Load(),
    "total", len(running))
```

5 minutes of work.

---

## Track B: Declarative Config

Make 50 agents reproducible, version-controllable, and diffable.

### B.1 `bhatti.yaml` schema

```yaml
volumes:
  rory-data:
    size: 5120

sandboxes:
  rory:
    image: spc-agents-hermes
    cpus: 2
    memory: 4096
    keep_hot: true
    init: |
      ln -s /opt/data/.hermes /home/lohar/.hermes
      sudo chown -R root:root /opt/hermes
      sudo chmod -R a-w /opt/hermes
      hermes gateway run
    init_restart: on-failure
    health_check:
      cmd: ["pgrep", "-f", "hermes gateway"]
      interval: 60
    volumes:
      - rory-data:/opt/data
    env:
      AGENT_NAME: rory
    secrets:
      - SLACK_BOT_TOKEN
      - OPENAI_API_KEY
    publish:
      8080: rory-files
```

**Design decisions:**

- `env` is for non-sensitive values (committed to git). `secrets` is
  a list of secret names — values managed via `bhatti secret set` and
  resolved at create time. The config drive gets the real values; the
  yaml never has them.
- `init` supports multiline (YAML block scalar). Becomes `sh -c "..."`.
- `volumes` uses the `name:mount` shorthand from the CLI.
- `publish` maps port → alias. The full URL is derived from the server's
  proxy zone.
- `health_check.cmd` is an exec-form array (no shell interpretation).
  `interval` in seconds.

**What's NOT in the yaml:**
- Secret values (managed separately)
- Sandbox IDs (assigned by bhatti)
- IP addresses (assigned by the engine)
- Snapshot names (created imperatively, referenced by name if needed)

### B.2 `bhatti export`

Dump current state as `bhatti.yaml`. This is the adoption path — run
export, check it into git, now you have IaC without changing anything.

**Implementation:** CLI command, no server changes. Calls existing APIs:

```go
var exportCmd = &cobra.Command{
    Use:   "export",
    Short: "Export current state as bhatti.yaml",
    RunE: func(cmd *cobra.Command, args []string) error {
        // GET /volumes → build volumes section
        // GET /sandboxes → for each, get details, build sandbox section
        // GET /sandboxes/{id}/ports → build publish section
        // GET /secrets → list names (not values)
        // Emit YAML
    },
}
```

The output is valid `bhatti.yaml` that `bhatti apply` can consume. It
won't have `secrets` references (it doesn't know which env vars came
from secrets), but it captures everything else.

**Edge case:** env vars that were set from secrets at create time are
now baked into the sandbox config. `export` emits them as plain `env`
entries. The user manually moves sensitive ones to the `secrets` list
and runs `bhatti secret set` for each. This is a one-time migration.

### B.3 `bhatti diff`

Show what `apply` would do, without doing it.

```
$ bhatti diff
+ volume/uyir-data (5120MB)
+ sandbox/uyir (spc-agents-hermes, 2 vCPU, 4096MB, keep_hot)
~ sandbox/rory: memory 2048 → 4096 (requires stop/start)
  sandbox/kk-579c8e19: no changes
- sandbox/cli-test-create: in cluster but not in yaml (destroy? use --prune)
```

`+` = create, `~` = update, `-` = exists in bhatti but not in yaml
(only shown, never auto-deleted).

**Implementation:** Parse yaml, fetch current state from API, compare.
Pure CLI logic — no server changes.

### B.4 `bhatti apply`

Read `bhatti.yaml`, diff against current state, execute changes.

**Resolution order** (respects dependencies):
1. Create missing volumes
2. Create missing sandboxes (volumes must exist first)
3. Update changed sandboxes (stop/start, edit, etc.)
4. Create/remove publish rules
5. Report results

**Update logic per field:**

| Field | Detect change | Operation |
|-------|--------------|-----------|
| `keep_hot` | Compare with current | `PATCH /sandboxes/{id}` (live) |
| `env` / `secrets` | Compare resolved env | Destroy + recreate |
| `cpus` / `memory` | Compare with current | Stop + recreate |
| `image` | Compare with current | Destroy + recreate |
| `init` / `init_restart` | Compare with current | Destroy + recreate |
| `volumes` added | Compare mount list | Destroy + recreate |
| `volumes` removed | Compare mount list | Destroy + recreate |
| `publish` | Compare with current rules | Add/remove rules (live) |
| `health_check` | Compare with current | `PATCH /sandboxes/{id}` (live) |

Most changes require destroy + recreate because the config drive is
immutable after boot. Volume data survives because volumes are external
to the sandbox.

**Confirmation:** Changes that destroy a sandbox prompt for confirmation
unless `--yes` is passed. Creates and live updates don't prompt.

```
$ bhatti apply
+ volume/uyir-data (5120MB)
+ sandbox/uyir (spc-agents-hermes, 2 vCPU, 4096MB, keep_hot)
~ sandbox/rory: memory 2048 → 4096

Sandbox rory will be destroyed and recreated (memory change).
Volume data (rory-data) is preserved.
Continue? [y/N] y

Creating volume uyir-data (5120MB)... done
Creating sandbox uyir... done (10.0.1.12)
Publishing uyir:8080 → uyir-api.bhatti.sh... done
Destroying sandbox rory... done
Creating sandbox rory (4096MB)... done (10.0.1.4)
Publishing rory:8080 → rory-files.bhatti.sh... done

Applied: 2 created, 1 updated, 0 unchanged
```

**What apply does NOT do:**
- Delete sandboxes or volumes not in the yaml. Use `--prune` to
  prompt for deletion, but never auto-delete.
- Create secrets. Secrets are managed imperatively. `apply` fails
  if a referenced secret doesn't exist.
- Run arbitrary commands inside VMs. Post-create setup beyond init
  is the user's problem.

### B.5 Matching yaml names to existing sandboxes

`apply` matches by sandbox name. If the yaml has `rory` and bhatti
has a sandbox named `rory`, they're the same. If bhatti has a sandbox
not in the yaml, it's unmanaged (shown in diff as `-`, never touched
by apply unless `--prune`).

**No state file.** Unlike Terraform, there's no `.tfstate`. The source
of truth is bhatti's API. The yaml is the desired state. `diff` compares
them. This is simpler and avoids state file corruption, but it means
renames are destroy + create (bhatti sees a new name and a missing old
name, not a rename).

---

## Dependency graph

```
A.1 (init restart)      — lohar + engine + CLI
A.2 (health checks)     — server + store + CLI. Uses A.1 for recovery.
A.3 (parallel thermal)  — server only, 30 min
A.4 (snapshot timeout)  — systemd + logging, 5 min

B.1 (yaml schema)       — design only, defines B.2-B.4
B.2 (export)            — CLI only, reads existing APIs
B.3 (diff)              — CLI only, reads existing APIs
B.4 (apply)             — CLI, calls existing APIs. Needs B.1 finalized.
```

Track A and B are independent. Within each track:

```
A: A.3 + A.4 (quick wins) → A.1 (init restart) → A.2 (health checks)
B: B.1 (schema) → B.2 (export) → B.3 (diff) → B.4 (apply)
```

A.3 and A.4 can ship today. A.1 is the most impactful — it eliminates
the nohup hack and manual recovery. A.2 builds on A.1 (health check
detects crash, init restart recovers it, health check confirms recovery).

B.2 (export) is the most impactful on the B track — it gives you IaC
for your existing 13 sandboxes without changing anything. B.3 and B.4
build on it.

## Estimated effort

| Item | Effort | Risk |
|------|--------|------|
| A.3 Parallel thermal | 30 min | None — same pattern as SnapshotAll |
| A.4 Snapshot timeout | 5 min | None — config change |
| A.1 Init restart | 2 days | Low — lohar change, needs integration test |
| A.2 Health checks | 2 days | Low — server goroutine, uses existing Exec |
| B.1 Yaml schema | Design | — |
| B.2 Export | 1 day | None — reads existing APIs |
| B.3 Diff | 1 day | None — CLI comparison logic |
| B.4 Apply | 3 days | Medium — destroy/recreate sequencing |

Total: ~10 days for both tracks.
