# Web Shell

## Problem

A developer sees a broken preview environment. The GitHub PR comment has
URLs to the app — `spc-pr-682-omni.bhatti.sh` shows a 500 error. They
want to check the logs, restart a service, poke at the database. Today
their only option is:

1. Have the bhatti CLI installed locally
2. Have a valid API key configured
3. Run `bhatti shell spc-pr-682`

Three prerequisites to look at a log file. New team members need
onboarding to bhatti before they can debug a preview. The CLI is for
operators. A web terminal in the browser is for everyone.

The original `web/index.html` was the first thing built in this project,
before Firecracker, before the agent. It has a decent xterm.js terminal
and a sandbox sidebar. But it was never properly integrated:

- **Auth doesn't work.** It passes `?token=` in the WebSocket URL, but
  `ServeHTTP` only accepts `Authorization` headers. Browsers can't set
  headers on WebSocket connections. The terminal literally cannot
  authenticate.
- **Not served by the server.** It's a standalone file in `web/`. The
  Go server has no route for `/`. The file was useful for local dev but
  never deployed.
- **Wrong abstraction for multi-tenancy.** It's a dashboard that shows
  ALL of a user's sandboxes, requires the user's API key, and stores it
  in `localStorage` (XSS-extractable). For a multi-tenant platform where
  previews are created by CI and consumed by different team members,
  you don't want every developer to have the admin API key.

Delete `web/index.html`. It served its purpose as the prototype. The
replacement is a different thing entirely.

---

## Design Principles

### Authorization strength must match blast radius

A shell in a sandbox is root access to a VM running the application with
its real environment — database URLs, API keys, network access to
internal services. The authorization mechanism must be proportional to
what it grants, not just to the lifetime of the container.

For a capability URL (bearer token), the blast radius is bounded by
VM isolation: the attacker gets one sandbox, not the host, not other
sandboxes, not the bhatti API. The credentials *inside* the sandbox
(environment variables, staging DB access) are the real exposure.

### Security is bounded by the distribution channel

A 256-bit token is unguessable. But the token is distributed via some
channel — a PR comment, a Slack message, an internal dashboard. The
system's security is bounded by who can see that channel, not by the
token's entropy.

| Distribution channel | Who sees the token | Acceptable? |
|---|---|---|
| Private channel, small team | 5–10 trusted people | Yes |
| Private channel, large org | Anyone in the org | Evaluate: contractors, departing employees |
| Public channel | The entire internet | No — use identity-gated auth (future) |

The current design (Design A: capability URL) is appropriate when the
token distribution channel is trusted. The architecture includes a seam
for future auth methods without restructuring.

### Shell access is a sandbox capability, not a publish feature

Publishing is about routing: "make this port reachable at this URL."
Shell access is about authorization: "grant interactive access to this
sandbox." These are orthogonal.

You should be able to shell into a sandbox that has no published ports
(debugging a sandbox during setup). You should be able to publish
without granting shell access (production-like preview with no debug
access). The token lives on the sandbox. Publishing is a separate
concern.

### The creator and consumer are different principals

The creator (automation, CI, an operator) has an API key — a
high-privilege credential. The consumer (a developer, a teammate) has a
browser and access to the shared URL. The shell token bridges this gap:
the creator generates a scoped credential, the consumer uses it. The
token grants access by possession (bearer), not by identity.

This means anyone who obtains the token — through a shared link,
through a log, through a referrer leak — gets shell access. There's no
way to distinguish authorized consumers from anyone else who sees the
URL. This is acceptable when the distribution channel is trusted.

### Decision tree for future auth modes

```
                    Does the sandbox have access to
                    sensitive credentials or internal networks?
                           /              \
                         yes               no
                         /                   \
              Is the repo public?        Design A is fine.
                /          \             (capability URL)
              yes           no
              /               \
        Design C            How large is the org?
        (identity-gated)      /            \
                          <20 devs        >20 devs
                            /                \
                      Design A            Design B
                      is fine             (token exchange)
```

**Design A (current):** Capability URL. Token in URL fragment → bearer
access → shell.

**Design B (future):** Token exchange. Token in URL → exchange for
short-lived session cookie → cookie authorizes WebSocket. Limits
exposure window from URL leakage.

**Design C (future):** Identity-gated. Token proves "I was invited" →
OAuth with GitHub → verify org/team membership → session → shell. Full
audit trail, works safely with public repos.

The `handleShellWS` token validation is the authorization seam. Today
it checks a hash. Tomorrow it could check a cookie session or an OAuth
token without changing the WebSocket relay, the HTML page, or the
routing.

---

## Design

Shell access is a first-class sandbox capability. The server generates
a scoped, per-sandbox token. The terminal UI lives at a canonical path
on the API host, independent of published aliases.

```
api.bhatti.sh/_shell/sbx_abc123         → xterm.js terminal (new)
api.bhatti.sh/_shell/sbx_abc123/ws      → WebSocket for terminal I/O (new)
```

### Why on the API host, not the published URL

The original design put the shell at the published URL
(`preview.bhatti.sh/_bhatti/shell`). This was rejected because:

1. **Couples shell to publish.** You can't debug a sandbox that hasn't
   published any ports. You'd have to publish a dummy port just to get
   a shell URL.
2. **Pollutes the public proxy.** The `PublicProxyHandler` is the hot
   path for all proxied traffic. Adding shell routing, token validation,
   and WebSocket handling to it adds complexity where it doesn't belong.
3. **Namespace collision risk.** `/_bhatti/` must never collide with the
   proxied app's routes. Unlikely with the underscore prefix, but
   unnecessary when there's a cleaner option.
4. **One canonical URL per sandbox.** Three published aliases shouldn't
   mean three shell entry points. The shell URL is
   `api.bhatti.sh/_shell/:id` regardless of how many ports are published.

The API host already handles unauthenticated paths (`/health`,
`/metrics`). Adding `/_shell/` follows the same pattern: intercept
before the auth middleware, validate differently.

### Why not cookie-based auth

Rejected because:

- Requires session signing, cookie parsing, CSRF protection, expiry
  management — complexity disproportionate to the use case
- Multi-tenant: a dashboard would show all of a user's sandboxes, but
  the person debugging a preview might not have (or should not have)
  the user's API key
- The token-per-sandbox approach is simpler and sufficient for Design A

### Why not a ticket/exchange mechanism

Considered and rejected for now. The exchange (POST token → get
short-lived ticket → use ticket for WebSocket) adds:

- In-memory ticket store with TTL reaper goroutine
- Extra HTTP round-trip before WebSocket connection
- Server-side state to manage

The security improvement is marginal when the token is already
distributed via a shared channel (the dominant leak vector). The
fragment approach (below) eliminates log leakage without the exchange
complexity.

The exchange becomes worthwhile in Design B (large orgs) where limiting
the token's reuse matters. The architecture supports adding it later —
the `/_shell/:id/ws` handler is the only code that needs to change.

---

## Token Delivery — URL Fragments

The shell token is delivered in the URL fragment, not the query string:

```
https://api.bhatti.sh/_shell/sbx_abc123#token=a7f3...9e2d
                                        ↑
                                  fragment, not ?query
```

**Why this matters:** Fragments are never sent to the server. RFC 3986
§3.5 — the fragment is stripped before the HTTP request is transmitted.

```
Browser sends:  GET /_shell/sbx_abc123 HTTP/1.1
                Host: api.bhatti.sh

Not sent:       #token=a7f3...9e2d
```

This means:

| Vector | Query param (`?token=`) | Fragment (`#token=`) |
|---|---|---|
| Distribution channel | Visible | Visible |
| CDN / proxy logs (Cloudflare, nginx) | **Token logged on every request** | **Never sent to server** |
| Server access logs | **Token in URL** | **Never in URL** |
| Browser history | Full URL with token | Fragment in history (local-only) |
| Referrer header | Mitigated by `no-referrer` | Fragments stripped from Referer per spec |
| Network monitoring | Token in GET request line | Token never on the wire in HTTP |

The token goes from "visible in every access log across your
infrastructure" to "visible only in the distribution channel and the
user's local browser." The fragment is free — same URL structure, three lines
of JavaScript to read `window.location.hash`.

### How the token reaches the server

The fragment never hits the server via HTTP. The JavaScript on the page
reads it and sends it as the **first WebSocket message**:

```
1. Browser loads   GET /_shell/sbx_abc123       (clean URL, no token)
2. JS reads        window.location.hash         → "a7f3...9e2d"
3. JS connects     WS  /_shell/sbx_abc123/ws    (clean URL, no token)
4. JS sends        {"type":"auth","token":"a7f3...9e2d"}   (first message)
5. Server validates token → starts shell relay
```

The WebSocket is TLS-encrypted. The token in the first message has the
same transport security as a POST body — encrypted end-to-end, not
logged by any proxy or CDN. The token never appears in any URL that
any server processes.

---

## End-to-End Flow

### Generating a shell URL

```bash
$ bhatti share dev
Shell: https://api.bhatti.sh/_shell/sbx_a1b2c3d4#token=a7f3...9e2d
```

Or as part of a deploy script with publish:

```bash
# deploy.sh
bhatti publish "$SANDBOX" --port 8000 --alias "$ALIAS_OMNI"
SHARE_OUT=$(bhatti share "$SANDBOX" --json)
SHELL_URL=$(echo "$SHARE_OUT" | jq -r '.url')
```

### PR comment

```markdown
### 🚀 Preview deployed

| Service | URL |
|---------|-----|
| Omni (API) | https://spc-pr-682-omni.bhatti.sh |
| Pando      | https://spc-pr-682-pando.bhatti.sh |
| Pulse      | https://spc-pr-682-pulse.bhatti.sh |
| **Shell**  | https://api.bhatti.sh/_shell/sbx_a1b2c3d4#token=a7f3...9e2d |
```

### Developer clicks the shell link

1. Browser loads `GET /_shell/sbx_a1b2c3d4` (no token sent to server)
2. Server serves the embedded xterm.js HTML page (static, no auth)
3. JS reads `#token=...` from the URL fragment
4. JS opens WebSocket to `/_shell/sbx_a1b2c3d4/ws` (no token in URL)
5. JS sends first message: `{"type":"auth","token":"a7f3...9e2d"}`
6. Server validates token against the sandbox's stored hash
7. Server sends: `{"type":"connected","sandbox":"dev","status":"running"}`
8. If sandbox is cold → wake it (same `ensureHot` path)
9. `Engine.Shell()` → `TerminalConn` → bidirectional relay
10. Developer sees a bash prompt. Debugs the broken preview.

### Destroying the preview

```bash
bhatti destroy "$SANDBOX" --yes
```

Token is stored on the sandbox row. Sandbox destroyed → token hash
gone → `/_shell/` returns 404 on next WebSocket auth attempt.

---

## Token Model

### Per-sandbox

The shell token is stored on the **sandbox**, not on any publish rule.
A sandbox can have multiple published aliases but one shell token. All
access goes through `/_shell/:sandbox-id` — aliases are irrelevant.

### Generation

```
token = hex(random 32 bytes)   → 64-char hex string, 256 bits entropy
hash  = SHA256(token)          → stored in DB
```

256 bits of entropy. Brute-forcing at 1 billion attempts/sec would take
3.7 × 10^59 years.

### Storage

```sql
ALTER TABLE sandboxes ADD COLUMN shell_token_hash TEXT;
```

Single column on the existing table. Empty string means shell access is
not enabled. No separate table, no token rotation history, no expiry
timestamp. The token lives exactly as long as the sandbox.

Why no expiry: sandboxes are typically ephemeral. They're destroyed when
no longer needed. Adding expiry means the shell link stops working while
the sandbox is still live — confusing. If you need to revoke, destroy
and recreate, or use `bhatti share --revoke`.

### Every call generates a fresh token

`bhatti share` (and `POST /sandboxes/:id/shell-token`) always generates
a new token and invalidates the previous one. No create-vs-rotate
distinction.

This avoids the "token shown once, can't retrieve" problem from the
original design. Automation scripts call `bhatti share` on every run,
get a fresh URL, and distribute it however they like (PR comment, Slack
message, internal dashboard). Old tokens die immediately — the caller
already has the new URL.

### Token lifecycle

| Event | What happens |
|-------|-------------|
| `bhatti share dev` | Generate new token. Store hash. Return token + URL. Previous token (if any) immediately invalidated. |
| `bhatti share dev` (again) | Same: new token, old one dies, new URL returned. |
| `bhatti share dev --revoke` | Set `shell_token_hash = ''`. Shell access disabled. Active WebSocket sessions are forcibly disconnected. No new connections accepted. |
| `bhatti destroy dev` | Sandbox row deleted → token hash gone → shell URL returns error. |

---

## API

### Authenticated endpoints (on `api.bhatti.sh`, behind auth middleware)

```
POST   /sandboxes/:id/shell-token    → generate fresh token (always rotates)
DELETE /sandboxes/:id/shell-token    → revoke shell access
```

**POST response:**

```json
{
  "token": "a7f3c8e1...9e2d",
  "url": "https://api.bhatti.sh/_shell/sbx_a1b2c3d4#token=a7f3c8e1...9e2d"
}
```

POST always generates a new token. Previous token immediately
invalidated. Calling POST is idempotent in effect — you always get a
valid, working URL.

**DELETE response:** `204 No Content`

These sit next to existing sandbox sub-routes in `handleSandbox`:

```go
case "shell-token":
    s.handleShellToken(w, r, id)  // POST or DELETE
```

Same routing pattern as `stop`, `start`, `exec`, `sessions`.

### Publish convenience

The `bhatti publish --shell` flag remains as convenience. When set, the
publish handler also calls the shell token generation logic and includes
the URL in the response:

```json
{
  "id": "pub_abc",
  "port": 8000,
  "alias": "preview",
  "url": "https://preview.bhatti.sh",
  "shell_token": "a7f3c8e1...9e2d",
  "shell_url": "https://api.bhatti.sh/_shell/sbx_a1b2c3d4#token=a7f3c8e1...9e2d"
}
```

This is sugar — it calls the same `POST /shell-token` logic internally.
The shell URL points to the API host, not the published alias.

### Unauthenticated endpoints (on `api.bhatti.sh`, before auth middleware)

```
GET  /_shell/:id        → static xterm.js HTML page (no auth)
WS   /_shell/:id/ws     → terminal WebSocket (auth via first message)
```

These are intercepted in `ServeHTTP` before the Bearer token check,
alongside `/health` and `/metrics`.

---

## Server Changes

### Routing — `/_shell/` bypasses auth

In `ServeHTTP`, the `/_shell/` prefix is handled before the auth check:

```go
// server.go — ServeHTTP

// Unauthenticated endpoints
if cleanPath == "/health" || cleanPath == "/metrics" {
    s.mux.ServeHTTP(w, r)
    return
}

// Web shell (unauthenticated — token validated on WebSocket)
if strings.HasPrefix(cleanPath, "/_shell/") {
    s.handleWebShell(w, r, cleanPath)
    return
}

// Auth required from here...
```

### handleWebShell

Routes to HTML page or WebSocket handler:

```go
func (s *Server) handleWebShell(w http.ResponseWriter, r *http.Request, cleanPath string) {
    // /_shell/sbx_abc123     → serve HTML
    // /_shell/sbx_abc123/ws  → WebSocket

    trimmed := strings.TrimPrefix(cleanPath, "/_shell/")
    parts := strings.SplitN(trimmed, "/", 2)
    sandboxID := parts[0]

    if sandboxID == "" {
        errResp(w, 404, "not found")
        return
    }

    if len(parts) == 1 || parts[1] == "" {
        s.serveShellHTML(w, r)
        return
    }
    if parts[1] == "ws" {
        s.handleShellWS(w, r, sandboxID)
        return
    }
    errResp(w, 404, "not found")
}
```

### serveShellHTML

Serves the embedded static page. No token validation — the security
boundary is the WebSocket, not the page load.

```go
//go:embed shell.html
var shellHTML []byte

func (s *Server) serveShellHTML(w http.ResponseWriter, r *http.Request) {
    w.Header().Set("Content-Type", "text/html; charset=utf-8")
    w.Header().Set("Referrer-Policy", "no-referrer")
    w.Header().Set("X-Content-Type-Options", "nosniff")
    w.Header().Set("X-Frame-Options", "DENY")
    w.Header().Set("Cache-Control", "public, max-age=3600")
    w.Header().Set("Content-Security-Policy", strings.Join([]string{
        "default-src 'none'",
        "script-src https://cdn.jsdelivr.net",                   // xterm.js (v5.x does not need unsafe-eval)
        "style-src https://cdn.jsdelivr.net 'unsafe-inline'",   // xterm.css
        "font-src https://cdn.jsdelivr.net",                    // FiraCode
        "connect-src 'self'",                                   // WebSocket + fetch
        "frame-ancestors 'none'",
    }, "; "))
    w.Write(shellHTML)
}
```

### handleShellWS

Upgrades WebSocket **first** (before any sandbox lookup), validates
token via first message, then delegates to the same shell + relay
machinery that the CLI uses.

**Security invariant:** The handler never leaks sandbox existence to an
unauthenticated client. The WebSocket upgrade happens unconditionally —
a 404-vs-101 status code cannot be used to enumerate sandbox IDs. The
sandbox lookup happens *after* the auth message, and all failure paths
(bad token, missing sandbox, shell not enabled) return the same generic
`"unauthorized"` error.

```go
func (s *Server) handleShellWS(w http.ResponseWriter, r *http.Request, sandboxID string) {
    // 1. Rate limit — per-IP cap on WebSocket connection attempts.
    //    Prevents connection-churn DoS (each attempt = goroutine + DB query).
    if !s.shellLimiter.Allow(r.RemoteAddr) {
        errResp(w, 429, "too many requests")
        return
    }

    // 2. Upgrade WebSocket FIRST — before any sandbox lookup.
    //    This prevents sandbox ID enumeration via HTTP status codes.
    conn, err := upgrader.Upgrade(w, r, http.Header{
        "Referrer-Policy": []string{"no-referrer"},
    })
    if err != nil {
        return
    }
    defer conn.Close()

    // 3. Read first message — must be auth within 10 seconds
    conn.SetReadDeadline(time.Now().Add(10 * time.Second))
    _, msg, err := conn.ReadMessage()
    if err != nil {
        return
    }

    var auth struct {
        Type  string `json:"type"`
        Token string `json:"token"`
    }
    if json.Unmarshal(msg, &auth) != nil || auth.Type != "auth" || auth.Token == "" {
        conn.WriteJSON(map[string]string{"type": "error", "error": "unauthorized"})
        return
    }

    // 4. Lookup + validate TOGETHER — same error for all failure modes.
    //    "sandbox not found", "shell not enabled", and "wrong token" are
    //    indistinguishable to the client. Prevents enumeration.
    sb, err := s.store.GetSandboxByID(sandboxID)
    tokenHash := sha256Hex(auth.Token)
    if err != nil || sb.Status == "destroyed" || sb.ShellTokenHash == "" ||
        subtle.ConstantTimeCompare([]byte(tokenHash), []byte(sb.ShellTokenHash)) != 1 {
        conn.WriteJSON(map[string]string{"type": "error", "error": "unauthorized"})
        return
    }

    // ---- authenticated ----

    // 5. Enforce concurrent session limit (cap: 5 per sandbox)
    if !s.shellSessions.Add(sandboxID) {
        conn.WriteJSON(map[string]string{"type": "error", "error": "too many sessions"})
        return
    }
    defer s.shellSessions.Remove(sandboxID)

    // 6. Register for revoke-disconnect. When DELETE /shell-token is
    //    called, all active sessions for this sandbox are torn down.
    revoked := s.shellSessions.Done(sandboxID)

    slog.Info("shell_connect", "sandbox", sb.Name, "sandbox_id", sb.ID,
        "ip", r.RemoteAddr)
    start := time.Now()
    defer func() {
        slog.Info("shell_close", "sandbox", sb.Name, "sandbox_id", sb.ID,
            "duration", time.Since(start).Round(time.Second))
    }()

    // 7. Send sandbox info
    conn.WriteJSON(map[string]any{
        "type":    "connected",
        "sandbox": sb.Name,
        "status":  sb.Status,
    })

    // 8. Wake sandbox (same ensureHot as CLI shell and public proxy)
    if err := s.ensureHot(r.Context(), sb.EngineID); err != nil {
        conn.WriteJSON(map[string]string{"type": "error", "error": "sandbox unavailable"})
        return
    }

    // 9. Create shell session — same Engine.Shell() as CLI path
    //    No session reattach for web shells.
    conn.SetReadDeadline(time.Time{}) // clear the 10s auth deadline
    term, err := s.engine.Shell(context.Background(), sb.EngineID)
    if err != nil {
        conn.WriteJSON(map[string]string{"type": "error", "error": "shell unavailable"})
        return
    }
    defer term.Close()

    // 10. Bidirectional relay — same wsRelay as CLI shell.
    //     Exits when either side closes OR when `revoked` fires.
    go func() {
        <-revoked
        conn.Close() // unblocks ReadMessage and Write in wsRelay
    }()
    wsRelay(conn, term)
}
```

### shellSessions — concurrent limit + revoke-disconnect

Tracks active web shell connections per sandbox. Enforces a cap (5) and
provides a channel to tear down all sessions on revoke.

```go
// shellSessionTracker manages active web shell sessions per sandbox.
type shellSessionTracker struct {
    mu       sync.Mutex
    counts   map[string]int            // sandbox ID → active count
    channels map[string][]chan struct{} // sandbox ID → done channels
    limit    int
}

func newShellSessionTracker(limit int) *shellSessionTracker {
    return &shellSessionTracker{
        counts:   make(map[string]int),
        channels: make(map[string][]chan struct{}),
        limit:    limit,
    }
}

// Add increments the count. Returns false if limit reached.
func (t *shellSessionTracker) Add(sandboxID string) bool {
    t.mu.Lock()
    defer t.mu.Unlock()
    if t.counts[sandboxID] >= t.limit {
        return false
    }
    t.counts[sandboxID]++
    return true
}

// Remove decrements the count and cleans up the done channel.
func (t *shellSessionTracker) Remove(sandboxID string) {
    t.mu.Lock()
    defer t.mu.Unlock()
    t.counts[sandboxID]--
    if t.counts[sandboxID] <= 0 {
        delete(t.counts, sandboxID)
    }
}

// Done returns a channel that is closed when DisconnectAll is called.
func (t *shellSessionTracker) Done(sandboxID string) <-chan struct{} {
    t.mu.Lock()
    defer t.mu.Unlock()
    ch := make(chan struct{})
    t.channels[sandboxID] = append(t.channels[sandboxID], ch)
    return ch
}

// DisconnectAll closes all done channels for a sandbox (called on revoke).
func (t *shellSessionTracker) DisconnectAll(sandboxID string) {
    t.mu.Lock()
    chs := t.channels[sandboxID]
    delete(t.channels, sandboxID)
    t.mu.Unlock()
    for _, ch := range chs {
        close(ch)
    }
}
```

### handleShellToken

Authenticated endpoint for generating and revoking shell tokens:

```go
func (s *Server) handleShellToken(w http.ResponseWriter, r *http.Request, id string) {
    sb := s.getUserSandbox(w, r, id)
    if sb == nil {
        return
    }

    switch r.Method {
    case "POST":
        // Generate fresh token (always rotates)
        token := randomHex(32)
        hash := sha256Hex(token)
        if err := s.store.SetShellToken(sb.ID, hash); err != nil {
            errResp(w, 500, "failed to set shell token")
            return
        }
        url := fmt.Sprintf("https://%s/_shell/%s#token=%s", s.apiHost, sb.ID, token)
        writeJSON(w, 200, map[string]string{
            "token": token,
            "url":   url,
        })

    case "DELETE":
        if err := s.store.ClearShellToken(sb.ID); err != nil {
            errResp(w, 500, "failed to revoke shell token")
            return
        }
        // Forcibly disconnect all active web shell sessions.
        s.shellSessions.DisconnectAll(sb.ID)
        w.WriteHeader(204)

    default:
        errResp(w, 405, "method not allowed")
    }
}
```

---

## WebSocket Relay — Shared with CLI Shell

The authenticated CLI shell (`handleSandboxWS` in `exec_handlers.go`)
and the web shell (`handleShellWS` in `shell_handlers.go`) use the same
bidirectional relay. Factor it out of `handleSandboxWS`:

```go
// wsRelay bridges a WebSocket connection and a terminal, with
// ping/pong keepalives and resize handling. Blocks until one side
// closes. Caller is responsible for closing conn and term.
func wsRelay(conn *websocket.Conn, term engine.TerminalConn) {
    var wsMu sync.Mutex
    wsWrite := func(msgType int, data []byte) error {
        wsMu.Lock()
        defer wsMu.Unlock()
        conn.SetWriteDeadline(time.Now().Add(wsWriteTimeout))
        return conn.WriteMessage(msgType, data)
    }

    done := make(chan struct{})
    var closeOnce sync.Once
    closeDone := func() { closeOnce.Do(func() { close(done) }) }

    // Pong resets read deadline (peer is alive)
    conn.SetReadDeadline(time.Now().Add(wsPongTimeout))
    conn.SetPongHandler(func(string) error {
        conn.SetReadDeadline(time.Now().Add(wsPongTimeout))
        return nil
    })

    // Ping ticker — keeps connection alive through proxies
    go func() {
        ticker := time.NewTicker(wsPingInterval)
        defer ticker.Stop()
        for {
            select {
            case <-ticker.C:
                if err := wsWrite(websocket.PingMessage, nil); err != nil {
                    closeDone()
                    return
                }
            case <-done:
                return
            }
        }
    }()

    // Terminal → WebSocket
    go func() {
        defer closeDone()
        buf := make([]byte, 4096)
        for {
            n, err := term.Read(buf)
            if err != nil {
                wsWrite(websocket.CloseMessage,
                    websocket.FormatCloseMessage(websocket.CloseNormalClosure, ""))
                return
            }
            if err := wsWrite(websocket.BinaryMessage, buf[:n]); err != nil {
                return
            }
        }
    }()

    // WebSocket → Terminal (with resize)
    go func() {
        defer closeDone()
        for {
            msgType, msg, err := conn.ReadMessage()
            if err != nil {
                return
            }
            if msgType == websocket.TextMessage {
                var resize struct {
                    Type string `json:"type"`
                    Rows int    `json:"rows"`
                    Cols int    `json:"cols"`
                }
                if json.Unmarshal(msg, &resize) == nil && resize.Type == "resize" {
                    term.Resize(resize.Rows, resize.Cols)
                    continue
                }
            }
            if _, err := term.Write(msg); err != nil {
                return
            }
        }
    }()

    <-done
}
```

Then `handleSandboxWS` (CLI shell) becomes:

```go
func (s *Server) handleSandboxWS(w http.ResponseWriter, r *http.Request, id string) {
    // ... auth (Bearer token — unchanged) ...
    // ... session reattach logic (unchanged) ...
    // ... get `term` ...
    wsRelay(conn, term)
}
```

And `handleShellWS` (web shell):

```go
func (s *Server) handleShellWS(w http.ResponseWriter, r *http.Request, sandboxID string) {
    // ... first-message auth ...
    // ... ensureHot + Engine.Shell() ...
    wsRelay(conn, term)
}
```

Same relay. Same terminal protocol. Same engine calls. The web shell
is a thin alternative entry point into machinery that already works.

### How the two shell paths relate

```
bhatti shell dev (CLI)                 bhatti share dev (web)
──────────────────────                 ──────────────────────
Bearer token in WS header              Shell token in first WS message
/sandboxes/:id/ws                      /_shell/:id/ws
Session reattach (list→attach→new)     Always new session
handleSandboxWS                        handleShellWS
        │                                      │
        ├──── ensureHot() ◄────────────────────┤  shared
        ├──── Engine.Shell() ◄─────────────────┤  shared
        └──── wsRelay() ◄──────────────────────┘  shared
```

The same `TerminalConn` interface. The same resize protocol (JSON text
messages with `{"type":"resize","rows":N,"cols":N}`). The same binary
frames for terminal data. A developer using the CLI and another using
the web shell get independent sessions in the same VM — the agent
handles concurrent sessions already.

### Why no session reattach for web shells

The CLI shell reattaches to detached sessions because there's a user
identity — "reattach to *my* detached session." With a bearer token
shared among a team, there's no identity. Two developers with the same
token would fight over the same session.

Web shells always create new sessions. If the developer refreshes the
page, they get a new shell. The old session detaches and lives in the
VM until the agent's idle timeout (1 hour) reaps it.

Session reattach for web shells is a future enhancement — store the
session ID in `localStorage`, send it on reconnect, server calls
`Engine.ShellAttach()`. Worth doing if developers complain about losing
scrollback on page refresh.

---

## CLI

### `bhatti share`

New command. Visible in `bhatti --help` alongside `shell`.

```
$ bhatti share dev
Shell: https://api.bhatti.sh/_shell/sbx_a1b2c3d4#token=a7f3...9e2d

$ bhatti share dev --json
{"token":"a7f3...9e2d","url":"https://api.bhatti.sh/_shell/sbx_a1b2c3d4#token=a7f3...9e2d"}

$ bhatti share dev --revoke
Shell access revoked.
```

Every call (without `--revoke`) generates a fresh token. Previous token
immediately invalidated. Automation scripts call it on every run, get a
fresh URL, distribute it however they need.

Implementation: resolves sandbox name → ID, calls
`POST /sandboxes/:id/shell-token`, prints the URL.

### `bhatti publish --shell`

Convenience flag. When set, publish also generates a shell token and
includes the URL in output:

```
$ bhatti publish dev --port 8000 --alias preview --shell
Published: https://preview.bhatti.sh
Shell:     https://api.bhatti.sh/_shell/sbx_a1b2c3d4#token=a7f3...9e2d
```

Calls `POST /sandboxes/:id/shell-token` internally after creating the
publish rule. Same token generation as `bhatti share`.

---

## The Shell HTML Page

Single embedded file. ~150 lines. No framework, no build step.

### Layout

```
┌──────────────────────────────────────────────────────────┐
│ ⚒ dev  ·  running                                        │
├──────────────────────────────────────────────────────────┤
│                                                          │
│  $ tail -f /tmp/omni-error.log                           │
│  [2026-04-09 10:23:14] ERROR: ...                        │
│  ...                                                     │
│                                                          │
│                                                          │
│                                                          │
└──────────────────────────────────────────────────────────┘
```

- **Toolbar:** sandbox name, status (from `connected` WebSocket message)
- **Terminal:** full-height xterm.js
- **No sidebar, no sandbox list, no create form**

### Behavior

1. Page loads → reads `#token=` from URL fragment
2. If no fragment → show centered message with input field for manual
   paste
3. If fragment → connect WebSocket to `/_shell/:id/ws`
4. First WebSocket message: `{"type":"auth","token":"..."}`
5. Wait for `{"type":"connected","sandbox":"...","status":"..."}`
   → render toolbar
6. On connection → send resize message
7. On disconnect → exponential backoff reconnect (re-sends auth),
   capped at 3 attempts. After 3 failures, stop retrying and show:
   "Session ended. The shell token may have been revoked or rotated."
   This avoids infinite retries with a stale (rotated) token.
8. Handle window resize → send resize message over WebSocket

### WebSocket protocol

Same as CLI — no new protocol:

| Direction | Type | Payload | Meaning |
|---|---|---|---|
| Client → Server | Text (first msg only) | `{"type":"auth","token":"..."}` | Authenticate |
| Server → Client | Text | `{"type":"connected","sandbox":"..."}` | Auth success + sandbox info |
| Server → Client | Text | `{"type":"error","error":"..."}` | Auth failure or shell error |
| Client → Server | Binary | raw bytes | Terminal input (keystrokes) |
| Server → Client | Binary | raw bytes | Terminal output |
| Client → Server | Text | `{"type":"resize","rows":N,"cols":N}` | Terminal resize |

After the initial auth exchange, the protocol is identical to the CLI
WebSocket. The `wsRelay` function handles it all.

### Dependencies (CDN)

```html
<script src="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js"></script>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.min.css">
```

And the FiraCode Nerd Font. Cached aggressively by CDN and browser. No
local bundling needed.

---

## Security Analysis

### Threat: token in URL fragment

The shell token appears in the URL fragment. Unlike query parameters,
fragments are never sent to the server, never logged by CDNs/proxies,
and stripped from Referer headers per spec.

| Vector | Risk | Mitigation |
|--------|------|-----------|
| Distribution channel | Visible to anyone who can see the channel (PR, Slack, dashboard) | Intentional. Scoped to trusted distribution channels. |
| Browser history | Fragment stored locally | Token is per ephemeral sandbox. Sandbox destroyed → token worthless. |
| CDN / proxy logs | **Not applicable** — fragment never sent to server | — |
| Server access logs | **Not applicable** — fragment never sent to server | — |
| Referrer header | **Not applicable** — fragments stripped per RFC 7231 | — |
| Shoulder surfing | URL visible in browser bar | Use `bhatti share` to rotate if compromised. |

### Threat: sandbox ID enumeration

The `/_shell/:id` HTML page is served for any ID without auth. The
security boundary is the WebSocket, not the page load. The WebSocket
handler upgrades **unconditionally** before any sandbox lookup — the
HTTP status code (101 Upgrade) is the same whether the sandbox exists
or not. After the auth message, all failure paths (bad token, missing
sandbox, shell not enabled, destroyed sandbox) return the same generic
`{"type":"error","error":"unauthorized"}`. An attacker cannot
distinguish "sandbox exists but wrong token" from "sandbox does not
exist."

### Threat: brute force on WebSocket

Token is 256 bits of entropy. The WebSocket handler closes the
connection on auth failure. No retry within the same connection — the
attacker must complete a new TLS handshake + WebSocket upgrade for each
attempt.

### Threat: connection-churn DoS

Each WebSocket connection attempt allocates a goroutine, performs a
WebSocket upgrade, and triggers a DB query. Without rate limiting, an
attacker could churn connections to exhaust server resources.

Mitigation: `handleShellWS` applies per-IP rate limiting before the
WebSocket upgrade. Default: 10 connections/second/IP. This is generous
for legitimate use (a developer opening tabs) but blocks automated
churn. Uses the same rate-limiting infrastructure as the API.

### Threat: unauthenticated WebSocket lingering

Server sets a 10-second read deadline on the first message. If no auth
message arrives, the connection is closed. Unauthenticated connections
cannot linger.

### Threat: timing attack on hash comparison

Token validation uses `crypto/subtle.ConstantTimeCompare`. The
comparison time does not vary with the number of matching bytes.

### Threat: XSS on the shell page

The shell page renders:
- Sandbox name (from WebSocket `connected` message) — escaped via
  `textContent`, not `innerHTML`
- Terminal output — handled by xterm.js which sanitizes escape sequences

No user-controlled content is rendered as raw HTML. No `localStorage`
for secrets, no cookies.

CSP header restricts script sources to `cdn.jsdelivr.net` only.
`frame-ancestors 'none'` prevents embedding in iframes.

### Threat: path traversal

`/_shell/sbx_abc/../../../etc/passwd` — the `path.Clean()` in
`ServeHTTP` normalizes the path before it reaches `handleWebShell`.
The handler splits on `/` and uses the first segment as a sandbox ID
lookup. Invalid IDs return 404 from the store.

### Threat: error message information leak

All pre-auth failure paths return the same `"unauthorized"` error —
no distinction between "sandbox not found", "shell not enabled", and
"invalid token". Post-auth errors (`"sandbox unavailable"`,
`"shell unavailable"`) are generic and reveal no internal details.
Internal error details (file paths, engine state) are logged
server-side only.

### Threat: concurrent shell exhaustion

Each `Engine.Shell()` allocates a PTY inside the VM. Without limits, a
valid token holder could open many concurrent sessions, exhausting
resources.

Mitigation: `handleShellWS` uses `shellSessionTracker` to track active
web shell connections per sandbox. Cap at 5 concurrent sessions. Return
`"too many sessions"` on the WebSocket if the limit is reached. The
counter is decremented in a `defer` on the handler goroutine, so leaked
connections (browser closed without clean close) are cleaned up when the
pong timeout (90s) fires and the goroutine exits.

### Blast radius

A leaked shell token grants:
- Root shell inside one Firecracker VM
- Access to environment variables (DB URLs, API keys in the sandbox)
- Network access to whatever the sandbox can reach

A leaked shell token does NOT grant:
- Host machine access
- Access to other sandboxes
- Access to the bhatti API (no API key in the sandbox)
- Persistent access (sandbox is ephemeral)

The authorization strength (bearer token) is appropriate when the blast
radius is bounded by VM isolation and the token's distribution channel
is trusted.

---

## Store Changes

### Migration

```sql
ALTER TABLE sandboxes ADD COLUMN shell_token_hash TEXT DEFAULT '';
```

Add to `pkg/store/store.go` in the migration list (`migrations` const).
Uses `DEFAULT ''` to match the existing pattern (`agent_token`,
`fc_path_origin`, etc.). Empty string means shell access is not enabled.
SQLite supports `ALTER TABLE ADD COLUMN` without a full table rebuild.

### Sandbox struct

Add `ShellTokenHash string` to the `Sandbox` struct. Update
`sandboxCols` and `scanSandbox` to include the new column.

### New methods

```go
// SetShellToken stores the SHA-256 hash of the shell token.
func (s *Store) SetShellToken(sandboxID, hash string) error {
    _, err := s.db.Exec(
        `UPDATE sandboxes SET shell_token_hash = ? WHERE id = ?`,
        hash, sandboxID)
    return err
}

// ClearShellToken clears the shell token hash (revokes shell access).
func (s *Store) ClearShellToken(sandboxID string) error {
    _, err := s.db.Exec(
        `UPDATE sandboxes SET shell_token_hash = '' WHERE id = ?`,
        sandboxID)
    return err
}
```

---

## What Changes, What Doesn't

### Changes

| Component | Change |
|-----------|--------|
| `pkg/store/store.go` | Add `shell_token_hash` column migration |
| `pkg/store/sandbox.go` | `ShellTokenHash` field, `SetShellToken`, `ClearShellToken` |
| `pkg/server/server.go` | Route `/_shell/` before auth (3 lines) |
| `pkg/server/shell_handlers.go` | New file: `handleWebShell`, `handleShellWS`, `handleShellToken`, `serveShellHTML`, `shellSessionTracker` (~150 lines) |
| `pkg/server/shell.html` | New file: embedded xterm.js page (~150 lines) |
| `pkg/server/exec_handlers.go` | Factor `wsRelay` out of `handleSandboxWS` (move ~40 lines into shared function) |
| `pkg/server/sandbox_handlers.go` | Add `"shell-token"` case in `handleSandbox` switch |
| `cmd/bhatti/share_cmd.go` | New file: `bhatti share` command (~40 lines) |
| `cmd/bhatti/cli.go` | Register `share` command (2 lines) |
| `web/index.html` | Delete |

### Does NOT change

| Component | Why |
|-----------|-----|
| `pkg/server/public_proxy.go` | Shell lives on API host, not on published URLs |
| `ServeHTTP` auth middleware | `/_shell/` bypasses it, same as `/health` |
| `handleSandboxWS` | Still works for CLI shells. Refactored to call `wsRelay`. |
| `pkg/engine/*` | `Shell()`, `ShellSession()`, `TerminalConn` unchanged |
| `pkg/agent/*` | Agent protocol unchanged |
| Thermal manager | `ensureHot` called the same way |
| Publish rules table | No schema change |
| Existing `bhatti shell` | Unchanged — authenticated CLI shell |
| Existing `bhatti publish` | Unchanged without `--shell` flag |

---

## Implementation Order

```
Phase 1 — Store + API
  1.1  Add shell_token_hash column to sandboxes         (store)
  1.2  ShellTokenHash field + SetShellToken/ClearShellToken  (store)
  1.3  handleShellToken: POST + DELETE                   (server)
  1.4  Add "shell-token" case in handleSandbox           (server)

Phase 2 — Web shell
  2.1  Factor wsRelay out of handleSandboxWS             (exec_handlers)
  2.2  /_shell/ routing in ServeHTTP                     (server)
  2.3  handleWebShell + serveShellHTML                   (shell_handlers)
  2.4  handleShellWS (first-message auth + relay)        (shell_handlers)
  2.5  shell.html (xterm.js page with fragment auth)     (shell.html)

Phase 3 — CLI
  3.1  bhatti share command                              (share_cmd)
  3.2  --shell flag on bhatti publish                    (publish_cmd)

Phase 4 — Cleanup
  4.1  Delete web/index.html
  4.2  End-to-end test: share → click → shell
```

Phase 1 and 2.1 are independent. Phase 2.2–2.5 depend on Phase 1.
Phase 3 depends on Phase 1. Phase 4 is after the feature works.

### Testing

- Unit: token generation, `subtle.ConstantTimeCompare` validation,
  store round-trip
- Unit: `/_shell/` routing (valid ID, missing ID, trailing path)
- Unit: `wsRelay` — mock TerminalConn, verify relay behavior
- Unit: `shellSessionTracker` — Add/Remove/Done/DisconnectAll
- Integration: `bhatti share` → connect WebSocket → send auth →
  verify shell works
- Integration: invalid token → `{"type":"error","error":"unauthorized"}` →
  connection closed
- Integration: no shell token set → same `"unauthorized"` error
- Integration: nonexistent sandbox ID → same `"unauthorized"` error
  (anti-enumeration: indistinguishable from invalid token)
- Integration: sandbox destroyed → same `"unauthorized"` error
- Integration: cold sandbox → shell wakes it, connects
- Integration: concurrent shell limit enforced (6th connection rejected)
- Integration: revoke (`DELETE /shell-token`) disconnects active sessions
- Integration: `path.Clean` prevents traversal
- Integration: per-IP rate limiting on `/_shell/*/ws`

---

## Future Considerations (not in scope)

- **Session reattach:** Store session ID in `localStorage`, send on
  reconnect, server calls `Engine.ShellAttach()`. Preserves scrollback
  across page refresh.
- **Identity-gated auth (Design C):** OAuth with GitHub at the
  `/_shell/:id/ws` auth step. Verify org/team membership before
  granting shell access. Required for public repos.
- **Token exchange (Design B):** POST token → short-lived session
  cookie → cookie authorizes WebSocket. Limits exposure window for
  large organizations.
- **Read-only shell mode:** A token that allows viewing terminal output
  but not typing. Useful for sharing debug sessions.
- **Idle timeout on WebSocket:** Close connections with no client I/O
  for 30 minutes. The agent's `max_idle_sec` (1 hour) handles the PTY
  side, but the WebSocket should also have a timeout.
- **Per-sandbox shell metrics:** Track connection count, duration,
  bytes transferred. Useful for platform-wide observability.
