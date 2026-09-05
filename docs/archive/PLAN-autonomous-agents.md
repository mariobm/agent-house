# Supporting Long-Running Autonomous Agents in Bhatti

## The Problem

Users want to run autonomous agents inside bhatti sandboxes — agents like Hermes and NanoClaw that:

1. **Maintain persistent connections** to external platforms (Slack WebSocket, Discord gateway, Telegram long-poll)
2. **Execute scheduled tasks** (cron jobs, one-shot delays, interval-based work)
3. **React to external events** (incoming Slack messages, webhooks, email)
4. **Run indefinitely** — hours, days, weeks

These conflict directly with bhatti's thermal cycle, which is designed to reclaim resources from idle VMs.

## Why the Thermal Cycle Breaks Autonomous Agents

The thermal cycle's decision flow for pausing a hot VM:

```go
if thermal == "hot" && idle > 30s && activity.AttachedSessions == 0 {
    te.Pause(...)  // hot → warm: freezes all vCPUs
}
```

Where `idle` = `time.Since(lastActivity)`, and `lastActivity` is only updated by **host-initiated operations**: `EXEC_REQ`, `FILE_READ_REQ`, `FILE_WRITE_REQ`, TTY stdin.

An autonomous agent scenario:

```
1. User creates sandbox with init: "hermes gateway"
2. Hermes starts, connects to Slack via WebSocket (Socket Mode)
3. Hermes is now idle from bhatti's perspective — no API calls from host
4. 30 seconds pass → thermal cycle pauses the VM (vCPUs frozen)
5. WebSocket heartbeat fails → Slack disconnects the bot
6. Bot is dead. User's Slack channel goes silent.
```

Even with the `AttachedSessions == 0` guard, the init session finishes its startup quickly and there's no *attached client* — the agent runs detached by design. And `ActiveSessions` (running processes) is NOT checked in the thermal transition condition.

The warm → cold transition is even worse:

```
7. 30 minutes of frozen WebSocket → snapshot + kill Firecracker process
8. Guest kernel state is gone. WebSocket is irrecoverable.
9. On next wake, hermes would need to fully reconnect — but nothing triggers the wake.
```

## What Users Are Doing Today

### Hermes Agent (NousResearch)

From the [cron internals](https://hermes-agent.nousresearch.com/docs/developer-guide/cron-internals/):

- **Gateway mode**: a long-running process (`hermes gateway`) that maintains WebSocket connections to Slack/Discord/Telegram and ticks a cron scheduler
- **Cron jobs**: stored in `~/.hermes/cron/jobs.json`, support one-shot delays, intervals, cron expressions, explicit timestamps
- **Execution model**: cron jobs run in **fresh agent sessions** — the gateway is the scheduler, not the executor
- **Delivery targets**: origin chat, local files, platform channels, explicit chat IDs
- **Locking**: prevents overlapping scheduler ticks from double-executing jobs
- **Recursion guard**: cron-run sessions disable the cronjob toolset (prevents runaway job creation)

Key insight: Hermes requires a **long-lived process** on the host that:
- Holds WebSocket connections open (Slack Socket Mode, Discord gateway)
- Ticks a scheduler loop
- Spawns fresh sessions for each job execution

### NanoClaw

From their [Slack skill](https://nanoclaw.dev/skills/slack/):

- Uses `@slack/bolt` (WebSocket Socket Mode) — same pattern as Hermes
- Outbound-only connections (no public URL needed)
- Bot token + App-Level token authentication
- Channel registration in a database
- Messages pipeline integration

Same fundamental need: a persistent WebSocket held open by a process that must not be interrupted.

### The Hack Users Would Resort To

Without native support, users would:

```bash
# Keep-alive loop that pings the sandbox every 20 seconds
while true; do
    bhatti exec my-agent -- true  # Updates lastActivity, prevents pause
    sleep 20
done
```

This defeats the entire thermal model — the VM stays hot burning resources, AND there's a wasteful polling loop on the host. Terrible for a Pi 5 running multiple sandboxes.

## Design: Three Layers

### Layer 1: `keep_hot` Sandbox Flag (Quick Win)

Add an opt-in flag that exempts a sandbox from thermal transitions.

**API change:**

```json
POST /sandboxes
{
  "name": "hermes-agent",
  "init": "hermes gateway",
  "keep_hot": true,
  "cpus": 1,
  "memory_mb": 512
}
```

**Thermal cycle change:**

```go
func (s *Server) runThermalCycle(te ThermalEngine, cfg ThermalConfig) {
    for _, sb := range sandboxes {
        if sb.Status != "running" { continue }
+       if sb.KeepHot { continue }  // skip thermal management entirely
        // ... existing thermal logic
    }
}
```

**Why this is enough for now:**
- Hermes gateway on a Pi 5 uses ~20MB RSS and near-zero CPU when idle (it's blocking on WebSocket read)
- Users who deploy autonomous agents know they're trading resources for persistence
- The flag is opt-in — default behavior is unchanged

**What needs to change:**
- `store.Sandbox` gets a `KeepHot bool` field
- `engine.SandboxSpec` gets a `KeepHot bool` field
- SQLite migration adds `keep_hot INTEGER DEFAULT 0` to sandboxes table
- Thermal cycle checks `sb.KeepHot` before evaluating transitions
- CLI: `bhatti create --keep-hot`
- API: `keep_hot` field in create request

**Resource budget concern:** Even on a Pi 5 (8GB RAM), keeping 4-5 agent VMs hot at 512MB each is fine (2.5GB). The thermal cycle still manages all other sandboxes. Could add a per-user limit on `keep_hot` sandboxes.

### Layer 2: Smarter Activity Detection (Complements Layer 1)

Even without `keep_hot`, the thermal cycle should be smarter about VMs that are genuinely doing work.

**Change the hot → warm guard to include `ActiveSessions`:**

```go
// Current:
if thermal == "hot" && idle > cfg.WarmTimeout && activity.AttachedSessions == 0 {

// Proposed:
if thermal == "hot" && idle > cfg.WarmTimeout && activity.AttachedSessions == 0 && activity.ActiveSessions == 0 {
```

This is a small change with big impact: the init session running `hermes gateway` keeps `ActiveSessions >= 1`, which prevents the VM from being paused. When the gateway crashes or is intentionally stopped, `ActiveSessions` drops to 0 and the thermal cycle kicks in normally.

**But wait** — this breaks thermal transitions for VMs running long-lived servers (e.g., a Next.js dev server started via init). That VM has `ActiveSessions >= 1` too, but the user DOES want it to go warm when they're not using it.

**Resolution: distinguish "service" sessions from "transient" sessions.**

The init session is already special-cased with ID `"init"`. We could introduce an `OperatingMode` concept:

```go
type ActivityInfo struct {
    LastActivityUnix int64 `json:"last_activity_unix"`
    ActiveSessions   int   `json:"active_sessions"`
    AttachedSessions int   `json:"attached_sessions"`
+   KeepAlive        bool  `json:"keep_alive"` // guest-side signal: "don't pause me"
}
```

The guest agent sets `KeepAlive` based on whether the sandbox was created with `keep_hot: true` (passed through the config drive). This is semantically cleaner than the thermal cycle checking a store field — the guest is the authority on whether it should stay alive.

### Layer 3: Event Router (Medium Term — The Right Architecture)

The `keep_hot` flag solves the problem by brute force. For 50 autonomous agents on a single host, we need something smarter: keep the connections on the host, wake VMs only when there's work.

**Architecture:**

```
                     ┌───────────────────────────────────────────┐
                     │  bhatti daemon                            │
                     │                                          │
                     │  ┌──────────────────────────────────────┐ │
                     │  │  Event Router                        │ │
                     │  │                                      │ │
  Slack WebSocket ──►│  │  ┌────────────┐  ┌────────────────┐  │ │
  (Socket Mode)      │  │  │ Connectors │  │ Scheduler      │  │ │
                     │  │  │ - Slack    │  │ - cron exprs   │  │ │
  Discord Gateway ──►│  │  │ - Discord  │  │ - one-shot     │  │ │
                     │  │  │ - Telegram │  │ - intervals    │  │ │
  HTTP Webhooks ────►│  │  │ - Webhook  │  │                │  │ │
                     │  │  └─────┬──────┘  └───────┬────────┘  │ │
                     │  │        │                  │           │ │
                     │  │        ▼                  ▼           │ │
                     │  │  ┌─────────────────────────────────┐  │ │
                     │  │  │       Event Queue               │  │ │
                     │  │  │  (sandbox_id, event, payload)   │  │ │
                     │  │  └──────────────┬──────────────────┘  │ │
                     │  │                 │                     │ │
                     │  │                 ▼                     │ │
                     │  │  ┌─────────────────────────────────┐  │ │
                     │  │  │       Dispatcher                │  │ │
                     │  │  │  1. ensureHot(sandbox)  ~50ms   │  │ │
                     │  │  │  2. exec(sandbox, handler_cmd)  │  │ │
                     │  │  │  3. collect response            │  │ │
                     │  │  │  4. deliver via connector       │  │ │
                     │  │  │  5. let sandbox go cold         │  │ │
                     │  │  └─────────────────────────────────┘  │ │
                     │  └──────────────────────────────────────┘ │
                     │                                          │
                     │  ┌────────────────────────────────────┐   │
                     │  │  Sandbox (cold most of the time)   │   │
                     │  │  - Agent code + dependencies       │   │
                     │  │  - State preserved via snapshot    │   │
                     │  │  - Wakes in ~50ms when event       │   │
                     │  │    arrives                         │   │
                     │  └────────────────────────────────────┘   │
                     └───────────────────────────────────────────┘
```

**How it works:**

1. **Registration**: User creates a sandbox and registers event sources:
   ```json
   POST /sandboxes/{id}/events
   {
     "type": "slack",
     "config": {
       "bot_token": "xoxb-...",
       "app_token": "xapp-..."
     },
     "handler": "hermes run --from-event /dev/stdin",
     "delivery": "stdout"
   }
   ```

2. **Connection management**: The event router (on the host, outside any VM) holds the Slack WebSocket connection. Zero VM resources consumed while idle.

3. **Event dispatch**: When a Slack message arrives:
   - Router looks up which sandbox handles this event source
   - `ensureHot(sandbox)` — ~50ms from cold
   - `exec(sandbox, "hermes run --from-event /dev/stdin")` with the event payload on stdin
   - Agent processes the message, generates a response on stdout
   - Router delivers the response back to Slack
   - Sandbox goes idle → warm → cold (thermal cycle works normally)

4. **Cron**: Same dispatch model. The router holds the schedule. When a job fires:
   - Wake sandbox
   - Run the prompt
   - Deliver results to configured target (Slack channel, file, etc.)
   - Let sandbox sleep

**Why this is superior:**

| Metric | `keep_hot` | Event Router |
|--------|-----------|--------------|
| VMs hot at once | N (one per agent) | 0-1 (only during execution) |
| Memory usage (50 agents) | 50 × 512MB = 25GB | ~100MB (router process) |
| Response latency | ~0ms (already hot) | ~50-200ms (cold wake + exec) |
| Slack heartbeat | ✅ handled by VM | ✅ handled by router |
| Crash recovery | VM restart needed | Router reconnects, VMs unchanged |
| Pi 5 feasibility | ~4-5 agents max | 50+ agents |

**The 50ms wake latency is invisible for messaging.** Slack users won't notice 50-200ms added to a bot response. Even 500ms is fine — humans type slowly.

**Trade-off:** The event router needs connectors for each platform. This is development effort but it's a finite set (Slack, Discord, Telegram, webhook, cron). And the connectors are simple — they're just WebSocket/HTTP clients that forward payloads.

### Layer 3b: Lightweight Sidecar (Alternative to Full Router)

Instead of building platform-specific connectors into bhatti, offer a generic pattern:

```
┌──────────────────────────────────────────────────┐
│  Host                                            │
│                                                  │
│  ┌──────────────────────────────────────────────┐│
│  │  bhatti-events (lightweight sidecar)         ││
│  │                                              ││
│  │  Webhook listener on :9090                   ││
│  │  Event queue (SQLite or in-memory)           ││
│  │  Sandbox wake + dispatch                     ││
│  └──────────────────────────────────────────────┘│
│                                                  │
│  ┌──────────────────────────────────────────────┐│
│  │  User-managed connector (runs on host or     ││
│  │  in a tiny always-on sandbox)                ││
│  │                                              ││
│  │  Slack WS → POST http://localhost:9090/      ││
│  │              events/{sandbox_id}             ││
│  └──────────────────────────────────────────────┘│
└──────────────────────────────────────────────────┘
```

The `bhatti-events` sidecar:
- Generic webhook receiver (HTTP POST → sandbox exec)
- Built into `bhatti serve` as an optional module
- No platform-specific code — connectors are user-provided
- Cron scheduler built-in (it's just a timer → exec dispatch)

The user provides the connector (which can itself be a tiny sandbox with `keep_hot: true` that just holds the WebSocket and posts events).

## Concrete Implementation Plan

### Phase 1: `keep_hot` + Smarter Activity (Ship in days)

Files to change:

1. **`pkg/store/store.go`** — Add `KeepHot` field to `Sandbox`, migration for column
2. **`pkg/engine/engine.go`** — Add `KeepHot` to `SandboxSpec`
3. **`pkg/server/routes.go`** — Accept `keep_hot` in create request, pass through
4. **`pkg/server/server.go`** — Check `KeepHot` in `runThermalCycle`
5. **`cmd/bhatti/cli.go`** — Add `--keep-hot` flag to create command
6. **`cmd/lohar/main.go`** — Pass `keep_hot` through config drive (informational)

Thermal cycle change:

```go
func (s *Server) runThermalCycle(te ThermalEngine, cfg ThermalConfig) {
    for _, sb := range sandboxes {
        if sb.Status != "running" { continue }
        if sb.KeepHot { continue }  // <-- new

        thermal := te.ThermalState(sb.EngineID)
        // ... rest unchanged
    }
}
```

### Phase 2: Webhook + Cron Event Router (Ship in weeks)

New components:

1. **`pkg/server/events.go`** — Event registration API, dispatch logic
2. **`pkg/server/cron.go`** — Cron scheduler (evaluate `github.com/robfig/cron/v3`)
3. **`pkg/store/store.go`** — Event source and cron job tables
4. **API endpoints:**
   - `POST /sandboxes/{id}/events` — register event source
   - `GET /sandboxes/{id}/events` — list sources
   - `DELETE /sandboxes/{id}/events/{event_id}` — remove source
   - `POST /sandboxes/{id}/cron` — create cron job
   - `GET /sandboxes/{id}/cron` — list jobs
   - `DELETE /sandboxes/{id}/cron/{job_id}` — remove job
   - `POST /events/{sandbox_id}` — external webhook endpoint (auth via event-specific token)

### Phase 3: Platform Connectors (Ship incrementally)

Optional built-in connectors, each a goroutine in the daemon:

1. **Slack** — `@slack/bolt`-style Socket Mode client in Go
2. **Discord** — Discord gateway WebSocket
3. **Generic webhook** — already handled by Phase 2
4. **Cron** — already handled by Phase 2

## Summary

| Approach | Complexity | Resource Cost | Agent Scale | When |
|----------|-----------|---------------|-------------|------|
| **`keep_hot` flag** | Low | High (VM per agent) | ~5 on Pi 5 | Phase 1 (days) |
| **Smarter activity** | Low | Medium | Same | Phase 1 (days) |
| **Webhook + cron router** | Medium | Low (host-side only) | 50+ on Pi 5 | Phase 2 (weeks) |
| **Platform connectors** | High | Lowest | 100+ on Pi 5 | Phase 3 (months) |

**Recommendation:** Ship Phase 1 immediately. It's the 80/20 solution — `keep_hot` is a one-line thermal cycle change that unblocks all existing autonomous agent frameworks. Phase 2 is the architecturally correct solution for scale. Phase 3 is nice-to-have polish.

The key insight is that bhatti's thermal cycle is **an asset, not a liability** for autonomous agents — but only if the event/scheduling layer lives on the host side of the thermal boundary. Agents don't need to be always-on; they need to be always-reachable. The 50ms cold-wake makes that possible.
