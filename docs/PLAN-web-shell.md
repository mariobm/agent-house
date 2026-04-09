# Web Shell via Published URLs

## Problem

A developer sees a broken preview environment. The GitHub PR comment has
URLs to the app — `spc-pr-682-omni.bhatti.sh` shows a 500 error. They
want to check the logs, restart a service, poke at the database. Today
their only option is:

1. Have the bhatti CLI installed locally
2. Have a valid API key configured
3. Run `bhatti shell spc-pr-682`

Three prerequisites to look at a log file. New team members need onboarding
to bhatti before they can debug a preview. The CLI is for operators. A
web terminal in the browser — accessible from the same URL they're already
looking at — is for everyone.

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

## Design

Shell access becomes a feature of `bhatti publish`. When you publish a
sandbox port, you can optionally enable shell access. The server
generates a scoped, per-sandbox token. The terminal UI lives under a
reserved path at the published URL itself.

```
spc-pr-682-omni.bhatti.sh/               → proxied to Django (existing)
spc-pr-682-omni.bhatti.sh/_bhatti/shell   → xterm.js terminal (new)
spc-pr-682-omni.bhatti.sh/_bhatti/ws      → WebSocket for terminal data (new)
```

### Why at the published URL, not a centralized dashboard

1. **No API key in the browser.** The shell token is scoped to one
   sandbox. Leaking it gives access to one ephemeral preview env, not
   the whole platform.
2. **No session/cookie machinery.** Token-in-URL is acceptable because
   the blast radius is a single disposable sandbox. The token dies when
   the sandbox is destroyed.
3. **Multi-tenant safe.** Different teams, different sandboxes, different
   tokens. No risk of one team seeing another's sandboxes.
4. **Fits the workflow.** The PR comment already has the preview URL.
   Add a shell link next to it. No separate app to navigate to.
5. **No centralized UI to maintain.** No sidebar, no sandbox list, no
   template picker. One sandbox, one shell.

### Why not cookie-based auth on the API host

The alternative is a dashboard at `api.bhatti.sh` with cookie sessions.
Rejected because:

- Requires exposing API keys to the browser (to create the session)
- Requires CSRF protection (cookies + mutating endpoints)
- Multi-tenant: the dashboard would show all of a user's sandboxes,
  but the person debugging a preview might not have (or should not have)
  the user's API key. The shell token lets you grant per-sandbox access
  without sharing platform credentials.
- More server complexity: session signing, cookie parsing in auth
  middleware, expiry management, invalidation on restart

The token-per-sandbox approach has none of these problems.

---

## End-to-End Flow

### Deploying a preview (SPC)

```bash
# deploy.sh changes:
PUBLISH_OUT=$(bhatti publish "$SANDBOX" --port 8000 --alias "$ALIAS_OMNI" --shell --json)
SHELL_URL=$(echo "$PUBLISH_OUT" | jq -r '.shell_url')
```

The `--shell` flag tells the server to generate a shell token for the
sandbox (idempotent — reuses existing token if one exists). The response
includes the token and the full shell URL.

### PR comment

```markdown
### 🚀 Preview deployed

| Service | URL |
|---------|-----|
| Omni (API) | https://spc-pr-682-omni.bhatti.sh |
| Pando      | https://spc-pr-682-pando.bhatti.sh |
| Pulse      | https://spc-pr-682-pulse.bhatti.sh |
| **Shell**  | https://spc-pr-682-omni.bhatti.sh/_bhatti/shell?token=a7f3...9e2d |
```

### Developer clicks the shell link

1. Browser loads `/_bhatti/shell?token=X`
2. `PublicProxyHandler` intercepts the `/_bhatti/` prefix
3. Serves the embedded xterm.js HTML (static, no token validation here)
4. The page reads `?token=` from the URL, connects WebSocket to
   `/_bhatti/ws?token=X`
5. Server validates token against the sandbox's stored hash
6. If sandbox is cold → wake it (same `EnsureHot` path as normal proxy)
7. WebSocket upgrade → attach to sandbox shell via `Engine.Shell()`
8. Developer sees a bash prompt. Debugs the broken preview.

### Destroying the preview

```bash
bhatti unpublish "$SANDBOX" --port 8000
bhatti destroy "$SANDBOX" --yes
```

Token is stored on the sandbox row. Sandbox destroyed → token gone →
any existing shell URL returns 404.

---

## Token Model

### Per-sandbox, not per-alias

A sandbox can have multiple published aliases (`spc-pr-682-omni`,
`spc-pr-682-pando`, `spc-pr-682-pulse`). The shell token is stored
on the **sandbox**, not the publish rule. Any alias can be used to
access the shell with the same token.

Why per-sandbox:
- All three aliases point to the same VM. Different tokens for the same
  shell would be confusing.
- `bhatti publish --shell` on a second alias returns the same token
  (idempotent).
- Revocation is simple: destroy sandbox → all tokens invalidated.

### Generation

```
token = hex(random 32 bytes)   → 64-char hex string, 256 bits entropy
hash  = SHA256(token)          → stored in DB
```

256 bits of entropy. Brute-forcing at 1 billion attempts/sec would take
3.7 × 10^59 years. Rate limiting on `PublicProxyHandler` already exists
as additional defense.

### Storage

```sql
ALTER TABLE sandboxes ADD COLUMN shell_token_hash TEXT;
```

Single column on the existing table. Nullable — `NULL` means shell
access is not enabled. No separate table, no token rotation history,
no expiry timestamp. The token lives exactly as long as the sandbox.

Why no expiry: preview sandboxes are ephemeral. They're destroyed when
the PR closes. Adding expiry means the shell link in the PR comment
stops working while the preview is still live — confusing. If you need
to revoke, destroy and recreate.

### Token lifecycle

| Event | What happens |
|-------|-------------|
| `bhatti publish --shell` | Generate token if `shell_token_hash` is NULL. Store hash. Return token. |
| `bhatti publish --shell` (again) | `shell_token_hash` exists → return same token? **No.** We don't store the plaintext. Return a message saying shell is already enabled. The original token was shown once. |
| `bhatti shell-token show` | **Can't.** We only store the hash. Show a message saying the token was displayed when `--shell` was first used. |
| `bhatti shell-token rotate` | Generate new token, overwrite hash, return new token. Old token stops working immediately. |
| `bhatti shell-token revoke` | Set `shell_token_hash = NULL`. Shell access disabled. |
| `bhatti destroy` | Sandbox row deleted → token hash gone → shell URL returns 404. |
| `bhatti unpublish` | Only removes the alias routing. Token stays on the sandbox. Shell still accessible via other aliases if any exist. |

**Problem with "return same token":** We store the hash, not the
plaintext. After the initial `--shell` publish, the token can't be
retrieved. This is the same pattern as API keys — shown once, never
again.

**Workaround:** `bhatti shell-token rotate` generates a fresh token
and shows it. Deploy scripts should capture the token from the initial
`--shell` publish response.

---

## URL Structure

### Reserved path: `/_bhatti/`

All bhatti-internal paths live under `/_bhatti/`. This prefix is
intercepted by `PublicProxyHandler` before proxying to the sandbox app.
The proxied app never sees requests to `/_bhatti/*`.

Why `_bhatti` with underscore:
- RFC 3986 allows underscores in paths
- Leading underscore is a common convention for internal/reserved paths
  (`_next`, `_vercel`, `__webpack_hmr`)
- Extremely unlikely to collide with real app routes
- Short, recognizable

### Endpoints

| Path | Method | Auth | Description |
|------|--------|------|-------------|
| `/_bhatti/shell` | GET | None | Serves the static xterm.js HTML page. Token is validated on the WebSocket, not here. |
| `/_bhatti/shell?token=X` | GET | None | Same page, but token pre-filled in the connect logic. |
| `/_bhatti/ws?token=X` | GET (upgrade) | Token required | WebSocket endpoint for terminal I/O. Validates token, wakes sandbox, attaches shell. |
| `/_bhatti/info?token=X` | GET | Token required | Returns sandbox name, status, published URLs. Used by the shell page to render the toolbar. |

### Why serve the HTML without auth

The HTML is static. It's the same xterm.js + connect code for every
sandbox. No sensitive data is in the page source. If someone hits
`/_bhatti/shell` without a token:

1. The page loads (it's just JavaScript)
2. It tries to connect the WebSocket with no token
3. WebSocket upgrade fails with 401
4. The page shows "Invalid or expired shell token"

This avoids a redirect dance and means the HTML can be aggressively
cached. The security boundary is the WebSocket, not the page load.

---

## PublicProxyHandler Changes

### Current flow

```
Request → resolve alias → find publish rule → find sandbox → EnsureHot → proxy to sandbox:port
```

### New flow

```
Request → resolve alias → find publish rule → find sandbox
  ├─ path starts with /_bhatti/ → handle shell/ws/info
  └─ else → EnsureHot → proxy to sandbox:port (unchanged)
```

The `/_bhatti/` intercept happens after alias resolution (we need the
sandbox ID) but before proxying.

### public_proxy.go changes

**File:** `pkg/server/public_proxy.go`

Current `proxyToAlias` method (called by both domain-mode `ServeHTTP`
and path-based `ServeHTTPPathBased`):

```go
func (h *PublicProxyHandler) proxyToAlias(w http.ResponseWriter, r *http.Request, alias, requestPath string) {
    // ... resolve alias → rule → sandbox → EnsureHot → proxy
}
```

Add a check after alias resolution:

```go
func (h *PublicProxyHandler) proxyToAlias(w http.ResponseWriter, r *http.Request, alias, requestPath string) {
    route, err := h.resolveRoute(alias)
    if err != nil {
        http.Error(w, "not found", 404)
        return
    }

    // Intercept /_bhatti/ paths before proxying
    if strings.HasPrefix(requestPath, "/_bhatti/") {
        h.handleBhattiPath(w, r, route, requestPath)
        return
    }

    // ... existing proxy logic unchanged
}
```

### handleBhattiPath

```go
func (h *PublicProxyHandler) handleBhattiPath(w http.ResponseWriter, r *http.Request, route *resolvedRoute, path string) {
    sb := route.sandbox // already resolved by proxyToAlias

    switch {
    case path == "/_bhatti/shell" || path == "/_bhatti/shell/":
        h.serveShellHTML(w, r)

    case path == "/_bhatti/ws":
        h.handleShellWS(w, r, sb)

    case path == "/_bhatti/info":
        h.handleShellInfo(w, r, sb)

    default:
        http.Error(w, "not found", 404)
    }
}
```

### serveShellHTML

Serves the embedded `shell.html`. No token validation. Static content.

```go
//go:embed shell.html
var shellHTML []byte

func (h *PublicProxyHandler) serveShellHTML(w http.ResponseWriter, r *http.Request) {
    w.Header().Set("Content-Type", "text/html; charset=utf-8")
    w.Header().Set("Referrer-Policy", "no-referrer")
    w.Header().Set("Cache-Control", "public, max-age=3600")
    w.Write(shellHTML)
}
```

### handleShellWS

This is the critical path. Validates the token, wakes the sandbox,
upgrades to WebSocket, attaches a shell session.

```go
func (h *PublicProxyHandler) handleShellWS(w http.ResponseWriter, r *http.Request, sb *store.Sandbox) {
    // 1. Check shell is enabled
    if sb.ShellTokenHash == "" {
        http.Error(w, `{"error":"shell not enabled"}`, 403)
        return
    }

    // 2. Validate token
    token := r.URL.Query().Get("token")
    if token == "" || sha256Hex(token) != sb.ShellTokenHash {
        http.Error(w, `{"error":"invalid token"}`, 401)
        return
    }

    // 3. Wake sandbox if cold/warm (same as regular proxy)
    if err := h.ensureHot(r.Context(), sb.EngineID); err != nil {
        http.Error(w, `{"error":"sandbox unavailable"}`, 503)
        return
    }

    // 4. WebSocket upgrade
    conn, err := upgrader.Upgrade(w, r, http.Header{
        "Referrer-Policy": []string{"no-referrer"},
    })
    if err != nil {
        return
    }
    defer conn.Close()

    // 5. Attach shell (same logic as handleSandboxWS but without
    //    session reattach — public shell always creates a new session)
    term, err := h.engine.Shell(context.Background(), sb.EngineID)
    if err != nil {
        conn.WriteMessage(websocket.TextMessage, []byte("shell error: "+err.Error()))
        return
    }
    defer term.Close()

    // 6. Bidirectional relay with ping/pong (reuse wsRelay helper)
    h.wsRelay(conn, term)
}
```

### handleShellInfo

Returns sandbox metadata for the toolbar.

```go
func (h *PublicProxyHandler) handleShellInfo(w http.ResponseWriter, r *http.Request, sb *store.Sandbox) {
    if sb.ShellTokenHash == "" {
        http.Error(w, `{"error":"shell not enabled"}`, 403)
        return
    }
    token := r.URL.Query().Get("token")
    if token == "" || sha256Hex(token) != sb.ShellTokenHash {
        http.Error(w, `{"error":"invalid token"}`, 401)
        return
    }

    // Get all published URLs for this sandbox
    rules, _ := h.store.ListSandboxPublishRules(sb.ID)
    urls := make([]map[string]any, 0, len(rules))
    for _, r := range rules {
        urls = append(urls, map[string]any{
            "alias": r.Alias,
            "port":  r.Port,
            "url":   publishedURL(r.Alias, h.proxyZone, h.publicProxyAddr),
        })
    }

    writeJSON(w, 200, map[string]any{
        "sandbox":    sb.Name,
        "status":     sb.Status,
        "created_at": sb.CreatedAt,
        "urls":       urls,
    })
}
```

---

## WebSocket Relay

The shell WebSocket in `handleSandboxWS` (the authenticated API endpoint)
already has all the plumbing: ping/pong keepalives, resize handling,
goroutine coordination with `done` channel, session reattach logic.

For the public shell, we need the same relay but without session
reattach (public shells always create new sessions — there's no user
identity to match detached sessions to).

### Factor out wsRelay

Currently `handleSandboxWS` in `exec_handlers.go` is a 120-line
function that mixes auth, session reattach, and relay logic. Factor
the relay into a reusable helper:

```go
// wsRelay bridges a WebSocket connection and a terminal, with
// ping/pong keepalives and resize handling. Blocks until one side
// closes.
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

    // Ping ticker
    go func() { /* same as current */ }()
    // Terminal → WebSocket
    go func() { /* same as current */ }()
    // WebSocket → Terminal (with resize)
    go func() { /* same as current */ }()

    <-done
}
```

Then `handleSandboxWS` becomes:

```go
func (s *Server) handleSandboxWS(...) {
    // ... auth, session reattach logic ...
    // ... get `term` ...
    wsRelay(conn, term)
}
```

And `handleShellWS` in public proxy:

```go
func (h *PublicProxyHandler) handleShellWS(...) {
    // ... token validation, EnsureHot, Shell() ...
    wsRelay(conn, term)
}
```

---

## CLI Changes

### `bhatti publish --shell`

**File:** `cmd/bhatti/sandbox_cmd.go` (or wherever publish lives)

Add `--shell` flag. When set:

1. Check if sandbox already has a `shell_token_hash`
2. If not, generate token, hash it, store hash via
   `PATCH /sandboxes/:id` (new field) or dedicated endpoint
3. Return token + shell URL in output

```
$ bhatti publish my-app --port 8000 --alias my-preview --shell
Published: https://my-preview.bhatti.sh
Shell:     https://my-preview.bhatti.sh/_bhatti/shell?token=a7f3...9e2d
```

The `--json` output:

```json
{
  "alias": "my-preview",
  "port": 8000,
  "url": "https://my-preview.bhatti.sh",
  "shell_url": "https://my-preview.bhatti.sh/_bhatti/shell?token=a7f3...9e2d"
}
```

### `bhatti shell-token`

New subcommand group for managing shell tokens:

```
bhatti shell-token rotate <sandbox>    → generate new token, print it
bhatti shell-token revoke <sandbox>    → disable shell access
```

No `show` command — we don't store the plaintext. The token is shown
once when generated (via `--shell` or `rotate`).

### API changes

**Option A:** New field on existing sandbox PATCH endpoint:

```
PATCH /sandboxes/:id
{ "shell_token": "generate" }
→ { "shell_token": "a7f3...9e2d" }
```

And `{ "shell_token": null }` to revoke. Fits the existing PATCH
pattern (`keep_hot` is already patched this way).

**Option B:** Dedicated endpoints:

```
POST   /sandboxes/:id/shell-token       → generate, return token
DELETE /sandboxes/:id/shell-token       → revoke
POST   /sandboxes/:id/shell-token/rotate → generate new, return token
```

Option B is cleaner — keeps the mutation explicit and the response
shape clear. The token is only returned in the POST response body.

Go with Option B.

---

## The Shell HTML Page

Single embedded file. ~200 lines. No framework, no build step.

### Layout

```
┌──────────────────────────────────────────────────────────┐
│ ⚒ spc-pr-682  ·  running  │  ↗ :8000  ↗ :3000  ↗ :3001 │
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

- **Toolbar:** sandbox name, status, published port links
- **Terminal:** full-height xterm.js, same theme as the old `index.html`
- **No sidebar, no sandbox list, no create form**

### Behavior

1. Page loads → reads `?token=` from URL
2. If no token → show centered message: "Shell token required" with an
   input field (for manual paste)
3. If token → fetch `/_bhatti/info?token=X` for sandbox metadata
4. Render toolbar with sandbox name and published URLs
5. Connect WebSocket to `/_bhatti/ws?token=X`
6. On connection → send resize message
7. On disconnect:
   - Check if sandbox is stopped → show "Sandbox paused"
   - Otherwise → exponential backoff reconnect (same as current)
8. Handle resize events → send resize message over WebSocket

### Dependencies (CDN)

Same as current — xterm.js and fit addon from jsDelivr:

```html
<script src="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js"></script>
<script src="https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js"></script>
```

And the FiraCode Nerd Font. These are cached aggressively by the CDN
and by the browser. No local bundling needed.

### Security headers

```
Referrer-Policy: no-referrer          → prevents token leaking in referrer
X-Content-Type-Options: nosniff       → standard
X-Frame-Options: DENY                 → prevents embedding in iframes
Content-Security-Policy: default-src 'self' https://cdn.jsdelivr.net; ...
```

`Referrer-Policy: no-referrer` is the most important one. Without it,
clicking a link from the shell page sends the full URL (including
`?token=`) to the target site via the `Referer` header.

---

## Security Analysis

### Threat: token in URL

The shell token appears in the URL query string. This means it's
visible in:

| Vector | Mitigation |
|--------|-----------|
| Browser history | Token is per ephemeral sandbox. Sandbox destroyed → token worthless. |
| Server access logs | The `PublicProxyHandler` doesn't log query params. But upstream proxies (Cloudflare, nginx) might. Acceptable: the token only grants shell to one sandbox. |
| Referrer header | `Referrer-Policy: no-referrer` on the shell page. External links don't leak the URL. |
| Shoulder surfing | Same risk as any URL with secrets. Use `bhatti shell-token rotate` if compromised. |
| Shared PR comment | Intentional. The shell link is meant to be shared with the team. If you don't want shell access in the PR, don't use `--shell`. |

**Why this is acceptable:** The token is narrowly scoped (one sandbox),
ephemeral (dies with the sandbox), and the sandbox itself is a preview
environment with no production data. Compare this to the alternative
of putting the user's API key in `localStorage` — that's a master
credential that controls all sandboxes, volumes, secrets, and images.

### Threat: brute force

Token is 256 bits of entropy. `PublicProxyHandler` already has per-IP
rate limiting. Even without it, brute-forcing a 256-bit token is
computationally infeasible.

### Threat: cross-tenant access

The token is validated against the specific sandbox the alias resolves
to. Even if an attacker guesses a valid token, they'd need to know
which alias it belongs to. The alias → sandbox → token_hash chain
prevents cross-sandbox access.

### Threat: XSS on the shell page

The shell page renders:
- Sandbox name (from `/_bhatti/info`) — escaped before insertion into DOM
- Published URLs (from `/_bhatti/info`) — rendered as `<a>` hrefs, not
  innerHTML
- Terminal output — handled by xterm.js which sanitizes escape sequences

No user-controlled content is rendered as raw HTML. No `localStorage`,
no cookies, no secrets in the page.

### Threat: path traversal

`/_bhatti/shell/../../../etc/passwd` — the `PublicProxyHandler` matches
the literal prefix `/_bhatti/` after `path.Clean()`. The three valid
paths are exact-matched. Everything else is 404. The cleaned path never
reaches the proxied app.

### Threat: WebSocket hijack

An attacker connects to `/_bhatti/ws` without a token. The server
checks `token` query param → empty → returns 401 before upgrading.
No WebSocket connection is established.

---

## Store Changes

### Migration

```sql
ALTER TABLE sandboxes ADD COLUMN shell_token_hash TEXT;
```

Add to `pkg/store/store.go` in the migration list. SQLite supports
`ALTER TABLE ADD COLUMN` without a full table rebuild.

### New methods

```go
// SetShellToken stores the shell token hash for a sandbox.
func (s *Store) SetShellToken(sandboxID, hash string) error

// ClearShellToken removes the shell token hash (revoke).
func (s *Store) ClearShellToken(sandboxID string) error

// GetShellTokenHash returns the shell token hash, or "" if not set.
// (Existing GetSandbox already returns the full sandbox struct —
// just add ShellTokenHash to the struct.)
```

Actually, simpler: just add `ShellTokenHash string` to the `Sandbox`
struct and use the existing `GetSandbox` / update methods. The only
new method needed is `SetShellToken` (a focused UPDATE).

---

## What Changes, What Doesn't

### Changes

| Component | Change |
|-----------|--------|
| `pkg/store/store.go` | Add `shell_token_hash` column migration |
| `pkg/store/sandbox.go` | Add `ShellTokenHash` field to `Sandbox` struct, `SetShellToken`, `ClearShellToken` |
| `pkg/server/public_proxy.go` | Intercept `/_bhatti/*`, serve HTML, handle WS, handle info |
| `pkg/server/exec_handlers.go` | Factor out `wsRelay` from `handleSandboxWS` |
| `pkg/server/publish_handlers.go` | `--shell` flag in publish, shell token generation |
| `pkg/server/admin_handlers.go` (or new file) | Shell token rotate/revoke endpoints |
| `cmd/bhatti/sandbox_cmd.go` | `--shell` flag on publish, `shell-token` subcommand |
| `pkg/server/shell.html` (new, embedded) | The xterm.js terminal page |
| `web/index.html` | Delete |

### Does NOT change

| Component | Why |
|-----------|-----|
| `ServeHTTP` auth middleware | Shell auth bypasses it entirely (goes through PublicProxyHandler) |
| Bearer token auth | CLI clients unaffected |
| `handleSandboxWS` | Still works for authenticated API WebSocket shells. Just refactored to use shared `wsRelay`. |
| Thermal manager | `EnsureHot` is called the same way |
| Engine / lohar / agent | Shell/Terminal interfaces unchanged |
| Publish rules table | No schema change — shell token is on the sandbox, not the rule |
| Existing `bhatti publish` without `--shell` | Unchanged behavior |

---

## Implementation Order

```
Phase 1 — Store + token generation
  1.1  Add shell_token_hash column to sandboxes     (store)
  1.2  SetShellToken / ClearShellToken methods       (store)
  1.3  Shell token API endpoints                     (server)
  1.4  CLI: bhatti shell-token rotate/revoke         (cli)

Phase 2 — Public proxy intercept
  2.1  Factor wsRelay out of handleSandboxWS         (exec_handlers)
  2.2  /_bhatti/* intercept in proxyToAlias           (public_proxy)
  2.3  serveShellHTML (embedded static page)          (public_proxy)
  2.4  handleShellWS (token validation + relay)       (public_proxy)
  2.5  handleShellInfo (metadata endpoint)            (public_proxy)

Phase 3 — CLI integration
  3.1  --shell flag on bhatti publish                 (cli + publish_handlers)
  3.2  Shell URL in publish JSON output               (cli)

Phase 4 — Cleanup
  4.1  Delete web/index.html and related files
  4.2  Test: deploy.sh with --shell, verify flow
```

Phase 1 and 2 are independent. Phase 3 depends on both. Phase 4 is
cleanup after the feature is confirmed working.

### Testing

- Unit: token generation, hash validation, store round-trip
- Unit: `/_bhatti/` path intercept in `PublicProxyHandler`
- Integration: publish with `--shell`, connect WebSocket with token,
  verify shell works
- Integration: invalid token → 401
- Integration: no shell token set → 403 on `/_bhatti/ws`
- Integration: sandbox destroyed → `/_bhatti/ws` returns 404
- Integration: cold sandbox → shell wakes it, connects

---

## Future Considerations (not in scope)

- **Read-only shell mode:** A token that allows viewing terminal output
  but not typing. Useful for sharing debug sessions.
- **Multiple tokens per sandbox:** Different tokens for different people,
  with audit logging. Overkill for preview envs.
- **Token expiry:** Independent of sandbox lifetime. Could add later if
  there's a use case for long-lived sandboxes where shell access should
  expire.
- **Shell session reattach:** Public shells currently create new sessions.
  Could store session IDs and reattach on reconnect. Worth doing later
  if developers complain about losing scrollback on page refresh.
