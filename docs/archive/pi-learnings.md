# Pi (coding agent) Architecture Learnings

Research from the pi-mono monorepo — specifically `packages/agent` (the
generic agent loop) and `packages/coding-agent` (the coding-specific tools
and CLI). Pi is an agentic coding framework with a TUI, extension system,
and pluggable tool execution.

Researched: 2026-03-19

---

## Core design: tools with pluggable operations

Every tool in pi exposes a **pluggable operations interface** that separates
the tool's logic (argument parsing, truncation, output formatting) from the
I/O substrate (local filesystem, SSH, sandbox API). This is the integration
point for bhatti.

```typescript
// Each tool has a *Operations interface:
interface BashOperations {
  exec(command, cwd, { onData, signal, timeout, env }) → { exitCode }
}
interface ReadOperations {
  readFile(absolutePath) → Buffer
  access(absolutePath) → void
  detectImageMimeType?(absolutePath) → string | null
}
interface WriteOperations {
  writeFile(absolutePath, content) → void
  mkdir(dir) → void
}
interface EditOperations {
  readFile(absolutePath) → Buffer
  writeFile(absolutePath, content) → void
  access(absolutePath) → void
}
interface LsOperations {
  exists(absolutePath) → boolean
  stat(absolutePath) → { isDirectory() }
  readdir(absolutePath) → string[]
}
interface GrepOperations {
  isDirectory(absolutePath) → boolean
  readFile(absolutePath) → string
}
interface FindOperations {
  exists(absolutePath) → boolean
  glob(pattern, cwd, { ignore, limit }) → string[]
}
```

The sandbox extension already demonstrates this pattern: it swaps in a
`BashOperations` implementation that wraps commands in OS-level sandboxing.
A bhatti-backed pi would implement these against the bhatti REST API.

**Mapping to bhatti API:**

| Pi operation | Bhatti API |
|---|---|
| `BashOperations.exec` | `POST /sandboxes/:id/exec` |
| `ReadOperations.readFile` | `GET /sandboxes/:id/files?path=...` |
| `WriteOperations.writeFile` | `PUT /sandboxes/:id/files?path=...` |
| `WriteOperations.mkdir` | `POST /sandboxes/:id/exec` with `mkdir -p` |
| `EditOperations.readFile` + `writeFile` | `GET` then `PUT` files |
| `LsOperations.readdir` | `GET /sandboxes/:id/files?path=...&ls=true` |
| `LsOperations.stat` | `HEAD /sandboxes/:id/files?path=...` |
| `GrepOperations.*` | `POST /sandboxes/:id/exec` with `rg` |
| `FindOperations.glob` | `POST /sandboxes/:id/exec` with `fd` |

---

## Tool execution is parallel by default

Pi's agent loop executes tool calls in **parallel**. When the LLM returns
multiple tool calls in one message (e.g., "read file A, read file B, run
grep for X"), they all execute concurrently:

```typescript
// agent-loop.ts — executeToolCallsParallel
const runningCalls = runnableCalls.map(prepared => ({
  prepared,
  execution: executePreparedToolCall(prepared, signal, emit),
}));

for (const running of runningCalls) {
  const executed = await running.execution;
  results.push(/* finalize */);
}
```

Tool calls are **prepared sequentially** (validation, beforeToolCall hooks)
then **executed concurrently**. Results are collected in source order.

**Implication:** bhatti's connection-per-operation model handles this
naturally — each parallel tool call opens its own TCP connection to lohar.
But the p99 latency under 5–10 concurrent connections matters, since the
user sees the slowest one. A common real pattern: 5 parallel file reads.

---

## Aggressive output truncation is a first-class concern

Every tool truncates output before returning it to the LLM:

| Tool | Limit | Strategy |
|---|---|---|
| Bash | 2000 lines or 50KB (tail) | Keep end of output (errors) |
| Read | 2000 lines or 50KB (head) | Keep start of file, with offset/limit pagination |
| Grep | 100 matches, 500 chars/line | Truncate long lines |
| Find | 1000 results | Cap results |
| Ls | 500 entries | Cap entries |

All limits are whichever-is-hit-first (lines vs bytes). Full output for
bash is saved to a temp file and its path included in the response.

**Key design:** Truncation parameters are passed alongside the operation:

```typescript
// Read tool supports offset + limit for pagination:
{ path: "/large-file.log", offset: 2001, limit: 2000 }
// → reads lines 2001–4000 from the file
```

**Implication for bhatti:** Bhatti's `FileRead` streams the **entire file**
from guest to host. For a 100MB log file, that's 100MB through the wire
protocol even though pi will truncate to 50KB. The truncation should happen
inside the guest. Add `offset` (1-indexed line number) and `limit` (max
lines) parameters to `FILE_READ_REQ` so lohar does server-side truncation.

---

## Bash streaming: `onData` callback, not buffered result

Pi's bash tool streams output in real-time via an `onData` callback:

```typescript
exec(command, cwd, {
  onData: (data: Buffer) => void,  // called for each chunk
  signal?: AbortSignal,
  timeout?: number,
  env?: NodeJS.ProcessEnv,
}) → { exitCode }
```

The tool also calls `onUpdate` with truncated partial results for live UI:

```typescript
execute: async (id, { command, timeout }, signal, onUpdate) => {
  // ...
  const handleData = (data: Buffer) => {
    totalBytes += data.length;
    chunks.push(data);

    if (onUpdate) {
      const truncation = truncateTail(Buffer.concat(chunks).toString());
      onUpdate({
        content: [{ type: "text", text: truncation.content }],
        details: { truncation, fullOutputPath: tempFilePath },
      });
    }
  };
  // ...
}
```

**Implication:** Bhatti's `Exec` API buffers the entire stdout/stderr and
returns it in one JSON response. For a `npm install` that takes 30 seconds,
the user sees nothing until completion. Pi expects streaming. This mismatch
means a bhatti-backed pi would either need to poll or use a different
endpoint.

---

## Edit is read-modify-write with fuzzy matching

Pi's edit tool is the most complex:

1. Read file from disk
2. Strip BOM (byte order mark)
3. Normalize line endings to LF
4. Fuzzy text matching (handles whitespace differences)
5. Check for ambiguous matches (reject if >1 occurrence)
6. Perform replacement
7. Restore original line endings + BOM
8. Write file back
9. Generate unified diff for display

All of this runs on the host side. In a bhatti-backed mode, steps 1 and 8
are remote operations — **two sequential round trips per edit**.

With bhatti's ~5ms file read + ~5ms file write, an edit takes ~10ms. Not
a problem today, but a guest-side edit primitive would halve it.

---

## Abort propagation is pervasive

Every tool checks `AbortSignal` before, during, and after operations:

```typescript
// Before:
if (signal?.aborted) { reject(new Error("Operation aborted")); return; }

// During (bash):
const onAbort = () => { killProcessTree(child.pid); };
signal?.addEventListener("abort", onAbort, { once: true });

// After:
if (aborted) return;
```

For bash, abort kills the **entire process tree** (`process.kill(-pid, SIGKILL)`).
This is important — a simple `SIGTERM` to the shell doesn't kill child
processes.

**Implication:** Bhatti's `KILL` frame sends `SIGTERM` to the session
process. For non-TTY exec, this may not kill grandchild processes. Consider
sending `SIGKILL` to the process group (negative PID) for more reliable
abort. Also, there's no abort mechanism for in-flight file operations —
a large FileRead can't be cancelled mid-stream.

---

## Working directory is session-scoped

Pi creates all tools with a fixed `cwd`:

```typescript
const tools = createCodingTools("/workspace");
// Every bash, read, write, edit, grep, find, ls resolves relative to this
```

Path resolution handles:
- `~` expansion to home directory
- `@` prefix stripping (UI artifact)
- Unicode space normalization
- macOS NFD normalization (decomposed filenames)
- Relative paths resolved against cwd

**Implication:** Bhatti file operations use absolute paths. Pi's SDK
integration would resolve paths on the host side before calling the API.
This works, but supporting relative-to-workspace resolution inside lohar
would simplify the integration.

---

## The agent needs `rg` and `fd` in the rootfs

Pi uses:
- **ripgrep** (`rg`) for the grep tool — JSON output mode, respects .gitignore
- **fd** for the find tool — glob matching, respects .gitignore

These are significantly faster than system `grep`/`find` on large codebases.
Pi auto-downloads them if missing (`ensureTool("rg", true)`), but having
them pre-installed in the rootfs avoids a 10-second download on first use.

---

## The coding tool set is small and focused

Pi's default coding tools: **read, bash, edit, write** (4 tools).

Read-only mode adds: **grep, find, ls** instead of edit/write.

The agent almost never needs more than these 7 primitives. Complex
operations (git, npm, docker, curl) go through bash.

---

## Agent loop: steering and follow-up queues

Pi's agent loop supports mid-run interruption:

```typescript
// While agent is executing tools:
agent.steer(message);     // interrupts after current tool, injects message
agent.followUp(message);  // waits until agent finishes, then continues

// The loop checks after each tool execution:
pendingMessages = await config.getSteeringMessages();
// If non-empty: skip remaining tool calls, inject steering messages
```

This means the agent can be redirected without waiting for all parallel
tool calls to complete. Steering messages skip the remaining queued tools.

**Implication:** Bhatti's exec API is fire-and-forget per request. There's
no way to signal "abort all pending operations for this sandbox" in one
call. The SDK integration would need to track in-flight requests and abort
them individually. A batch-abort endpoint (`DELETE /sandboxes/:id/exec`
or similar) could help.

---

## Summary: what bhatti should learn from pi

### Must-have for SDK integration

1. **Server-side file read truncation** — add `offset`/`limit` to
   `FILE_READ_REQ`. Agents always truncate; doing it guest-side avoids
   transferring megabytes of data that gets thrown away.

2. **Streaming exec** — the agent UI expects real-time output chunks,
   not a buffered response after completion.

3. **`rg` and `fd` in rootfs** — the two tools agents use most after
   bash and read/write.

### Should-have for good UX

4. **Guest-side edit primitive** — `FILE_EDIT_REQ` with find/replace
   saves a full round trip vs read-modify-write.

5. **Abort for file operations** — support `KILL` mid-FileRead.

6. **Process group kill on abort** — `SIGKILL` to `-pid`, not `SIGTERM`
   to `pid`, for reliable cleanup.

7. **Relative path support** — resolve paths relative to `/workspace`
   by default.

### Nice-to-have

8. **Batch abort** — cancel all in-flight operations for a sandbox.

9. **File watch** — `FILE_WATCH_REQ` for filesystem change notifications
   (pi doesn't have this yet either, but Sprites does).

10. **Document the operations mapping** — a "building a pi extension for
    bhatti" guide showing which API calls map to which pi operations.
