# Bhatti v3 — CLI, Filesystem, Deployment

Parts 8–12 are done: VM lifecycle, sessions, thermal management, auth,
volumes, init scripts. 67 integration tests on real Firecracker VMs.

What remains: a filesystem API for host↔guest file transfer, a CLI to
interact with the system, and deployment tooling to run it permanently.

### Dependency graph

```
Part 13 (filesystem)   — no deps
     ↓
Part 17 (API fix)      — no deps, prerequisite for CLI
     ↓
Part 18 (CLI)          — needs Part 13 + 17
     ↓
Part 19 (deployment)   — needs Part 18 (bhatti serve)
     ↓
Part 20 (install)      — needs Part 19 (systemd service)
```

All parts support both aarch64 and x86_64.

---

## Part 13 — Filesystem API

Read, write, stat, and list files inside a sandbox. Used by the CLI
(`bhatti file read/write`) and by SDK consumers building on top of bhatti.

### 13.1 Frame Types

```go
// pkg/agent/proto/constants.go — additions

FILE_READ_REQ   byte = 0x50  // host → guest: JSON {"path": "..."}
FILE_READ_RESP  byte = 0x51  // guest → host: JSON {"size": N, "mode": "0644"}
                              //   followed by STDOUT frames with file content,
                              //   then EXIT with code 0
FILE_WRITE_REQ  byte = 0x52  // host → guest: JSON {"path": "...", "mode": "0644", "size": N}
                              //   followed by STDIN frames with content
FILE_WRITE_RESP byte = 0x53  // guest → host: JSON {"status": "ok"}
FILE_STAT_REQ   byte = 0x54  // host → guest: JSON {"path": "..."}
FILE_STAT_RESP  byte = 0x55  // guest → host: JSON FileInfo
FILE_LS_REQ     byte = 0x56  // host → guest: JSON {"path": "..."}
FILE_LS_RESP    byte = 0x57  // guest → host: JSON []FileInfo
```

### 13.2 Message Structs

```go
// pkg/agent/proto/messages.go — additions

type FileInfo struct {
    Name  string `json:"name"`
    Size  int64  `json:"size"`
    Mode  string `json:"mode"`  // e.g. "0644"
    IsDir bool   `json:"is_dir"`
    Mtime int64  `json:"mtime"` // unix timestamp
}
```

### 13.3 Lohar Handlers

```go
// cmd/lohar/files.go

func handleFileRead(conn net.Conn, payload []byte) {
    var req struct{ Path string `json:"path"` }
    json.Unmarshal(payload, &req)

    f, err := os.Open(req.Path)
    if err != nil {
        proto.WriteFrame(conn, proto.ERROR, []byte(err.Error()))
        return
    }
    defer f.Close()

    info, _ := f.Stat()
    proto.SendJSON(conn, proto.FILE_READ_RESP, map[string]any{
        "size": info.Size(),
        "mode": fmt.Sprintf("%04o", info.Mode().Perm()),
    })

    buf := make([]byte, 32768)
    for {
        n, err := f.Read(buf)
        if n > 0 {
            proto.WriteFrame(conn, proto.STDOUT, buf[:n])
        }
        if err != nil { break }
    }
    exit := proto.ExitPayload(0)
    proto.WriteFrame(conn, proto.EXIT, exit[:])
}

func handleFileWrite(conn net.Conn, payload []byte) {
    var req struct {
        Path string `json:"path"`
        Mode string `json:"mode"`
        Size int64  `json:"size"`
    }
    json.Unmarshal(payload, &req)

    mode, _ := strconv.ParseUint(req.Mode, 8, 32)
    if mode == 0 { mode = 0644 }

    os.MkdirAll(filepath.Dir(req.Path), 0755)
    f, err := os.OpenFile(req.Path, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, os.FileMode(mode))
    if err != nil {
        proto.WriteFrame(conn, proto.ERROR, []byte(err.Error()))
        return
    }
    defer f.Close()

    var written int64
    for written < req.Size {
        msgType, data, err := proto.ReadFrame(conn)
        if err != nil { break }
        if msgType == proto.STDIN {
            f.Write(data)
            written += int64(len(data))
        }
    }

    // chown to lohar user
    os.Chown(req.Path, 1000, 1000)

    proto.SendJSON(conn, proto.FILE_WRITE_RESP, map[string]string{"status": "ok"})
}

func handleFileStat(conn net.Conn, payload []byte) {
    var req struct{ Path string `json:"path"` }
    json.Unmarshal(payload, &req)

    info, err := os.Stat(req.Path)
    if err != nil {
        proto.WriteFrame(conn, proto.ERROR, []byte(err.Error()))
        return
    }
    proto.SendJSON(conn, proto.FILE_STAT_RESP, proto.FileInfo{
        Name:  info.Name(),
        Size:  info.Size(),
        Mode:  fmt.Sprintf("%04o", info.Mode().Perm()),
        IsDir: info.IsDir(),
        Mtime: info.ModTime().Unix(),
    })
}

func handleFileList(conn net.Conn, payload []byte) {
    var req struct{ Path string `json:"path"` }
    json.Unmarshal(payload, &req)

    entries, err := os.ReadDir(req.Path)
    if err != nil {
        proto.WriteFrame(conn, proto.ERROR, []byte(err.Error()))
        return
    }
    var files []proto.FileInfo
    for _, e := range entries {
        info, _ := e.Info()
        if info == nil { continue }
        files = append(files, proto.FileInfo{
            Name:  info.Name(),
            Size:  info.Size(),
            Mode:  fmt.Sprintf("%04o", info.Mode().Perm()),
            IsDir: info.IsDir(),
            Mtime: info.ModTime().Unix(),
        })
    }
    proto.SendJSON(conn, proto.FILE_LS_RESP, files)
}
```

### 13.4 Handler Dispatch Addition

```go
// cmd/lohar/handler.go — add to the switch in handleControlConnection:

    case proto.FILE_READ_REQ:
        updateActivity()
        handleFileRead(conn, payload)
    case proto.FILE_WRITE_REQ:
        updateActivity()
        handleFileWrite(conn, payload)
    case proto.FILE_STAT_REQ:
        handleFileStat(conn, payload)
    case proto.FILE_LS_REQ:
        handleFileList(conn, payload)
```

### 13.5 AgentClient Methods

```go
// pkg/agent/client.go — additions

// FileRead reads a file from the guest and writes its contents to w.
// Returns the file size and mode.
func (c *AgentClient) FileRead(ctx context.Context, path string, w io.Writer) (size int64, mode string, err error) {
    conn, err := c.dialControl()
    if err != nil { return 0, "", err }
    defer conn.Close()

    if deadline, ok := ctx.Deadline(); ok {
        conn.SetDeadline(deadline)
    }

    proto.SendJSON(conn, proto.FILE_READ_REQ, map[string]string{"path": path})

    // Read FILE_READ_RESP
    msgType, payload, err := proto.ReadFrame(conn)
    if msgType == proto.ERROR { return 0, "", fmt.Errorf("agent: %s", payload) }
    var resp struct {
        Size int64  `json:"size"`
        Mode string `json:"mode"`
    }
    json.Unmarshal(payload, &resp)

    // Read STDOUT frames until EXIT
    var written int64
    for {
        msgType, payload, err = proto.ReadFrame(conn)
        if err != nil { return written, resp.Mode, err }
        if msgType == proto.STDOUT {
            n, _ := w.Write(payload)
            written += int64(n)
        }
        if msgType == proto.EXIT { break }
        if msgType == proto.ERROR { return written, resp.Mode, fmt.Errorf("agent: %s", payload) }
    }
    return written, resp.Mode, nil
}

// FileWrite writes content from r to a file in the guest.
func (c *AgentClient) FileWrite(ctx context.Context, path, mode string, size int64, r io.Reader) error {
    conn, err := c.dialControl()
    if err != nil { return err }
    defer conn.Close()

    if deadline, ok := ctx.Deadline(); ok {
        conn.SetDeadline(deadline)
    }

    proto.SendJSON(conn, proto.FILE_WRITE_REQ, map[string]any{
        "path": path, "mode": mode, "size": size,
    })

    // Send file content as STDIN frames
    buf := make([]byte, 32768)
    for {
        n, err := r.Read(buf)
        if n > 0 {
            proto.WriteFrame(conn, proto.STDIN, buf[:n])
        }
        if err != nil { break }
    }

    // Read FILE_WRITE_RESP
    msgType, payload, err := proto.ReadFrame(conn)
    if err != nil { return err }
    if msgType == proto.ERROR { return fmt.Errorf("agent: %s", payload) }
    return nil
}

// FileStat returns file info for a path in the guest.
func (c *AgentClient) FileStat(ctx context.Context, path string) (*proto.FileInfo, error) {
    conn, err := c.dialControl()
    if err != nil { return nil, err }
    defer conn.Close()

    proto.SendJSON(conn, proto.FILE_STAT_REQ, map[string]string{"path": path})

    msgType, payload, err := proto.ReadFrame(conn)
    if err != nil { return nil, err }
    if msgType == proto.ERROR { return nil, fmt.Errorf("agent: %s", payload) }
    var info proto.FileInfo
    json.Unmarshal(payload, &info)
    return &info, nil
}

// FileList returns directory contents for a path in the guest.
func (c *AgentClient) FileList(ctx context.Context, path string) ([]proto.FileInfo, error) {
    conn, err := c.dialControl()
    if err != nil { return nil, err }
    defer conn.Close()

    proto.SendJSON(conn, proto.FILE_LS_REQ, map[string]string{"path": path})

    msgType, payload, err := proto.ReadFrame(conn)
    if err != nil { return nil, err }
    if msgType == proto.ERROR { return nil, fmt.Errorf("agent: %s", payload) }
    var files []proto.FileInfo
    json.Unmarshal(payload, &files)
    return files, nil
}
```

### 13.6 Engine Methods

```go
// pkg/engine/firecracker/engine.go — additions

func (e *Engine) FileRead(ctx context.Context, id, path string, w io.Writer) (int64, string, error) {
    vm, err := e.getVM(id)
    if err != nil { return 0, "", err }
    if vm.Thermal != "hot" { return 0, "", fmt.Errorf("sandbox not hot") }
    return vm.Agent.FileRead(ctx, path, w)
}

func (e *Engine) FileWrite(ctx context.Context, id, path, mode string, size int64, r io.Reader) error {
    vm, err := e.getVM(id)
    if err != nil { return err }
    if vm.Thermal != "hot" { return fmt.Errorf("sandbox not hot") }
    return vm.Agent.FileWrite(ctx, path, mode, size, r)
}

func (e *Engine) FileStat(ctx context.Context, id, path string) (*proto.FileInfo, error) {
    vm, err := e.getVM(id)
    if err != nil { return nil, err }
    if vm.Thermal != "hot" { return nil, fmt.Errorf("sandbox not hot") }
    return vm.Agent.FileStat(ctx, path)
}

func (e *Engine) FileList(ctx context.Context, id, path string) ([]proto.FileInfo, error) {
    vm, err := e.getVM(id)
    if err != nil { return nil, err }
    if vm.Thermal != "hot" { return nil, fmt.Errorf("sandbox not hot") }
    return vm.Agent.FileList(ctx, path)
}
```

### 13.7 HTTP API Routes

```
GET    /sandboxes/:id/files?path=/workspace/f.txt      → file content
PUT    /sandboxes/:id/files?path=/workspace/f.txt       → body = content
HEAD   /sandboxes/:id/files?path=/workspace/f.txt       → file stat
GET    /sandboxes/:id/files?path=/workspace/&ls=true    → JSON directory listing
```

```go
// pkg/server/routes.go — add route:
//   case "files":
//       s.handleSandboxFiles(w, r, id)

func (s *Server) handleSandboxFiles(w http.ResponseWriter, r *http.Request, id string) {
    sb, err := s.store.GetSandbox(id)
    if err != nil { errResp(w, 404, "not found"); return }

    if err := s.ensureHot(r.Context(), sb.EngineID); err != nil {
        errResp(w, 500, "wake sandbox: "+err.Error()); return
    }

    path := r.URL.Query().Get("path")
    if path == "" { errResp(w, 400, "path required"); return }

    eng := s.engine.(*Engine) // or type-assert to interface with file methods

    switch r.Method {
    case http.MethodGet:
        if r.URL.Query().Get("ls") == "true" {
            files, err := eng.FileList(r.Context(), sb.EngineID, path)
            if err != nil { errResp(w, 500, err.Error()); return }
            writeJSON(w, 200, files)
        } else {
            w.Header().Set("Content-Type", "application/octet-stream")
            _, _, err := eng.FileRead(r.Context(), sb.EngineID, path, w)
            if err != nil { errResp(w, 500, err.Error()); return }
        }
    case http.MethodPut:
        size := r.ContentLength
        mode := r.URL.Query().Get("mode")
        if mode == "" { mode = "0644" }
        if err := eng.FileWrite(r.Context(), sb.EngineID, path, mode, size, r.Body); err != nil {
            errResp(w, 500, err.Error()); return
        }
        writeJSON(w, 200, map[string]string{"status": "ok"})
    case http.MethodHead:
        info, err := eng.FileStat(r.Context(), sb.EngineID, path)
        if err != nil { errResp(w, 404, err.Error()); return }
        w.Header().Set("X-File-Size", fmt.Sprint(info.Size))
        w.Header().Set("X-File-Mode", info.Mode)
        w.Header().Set("X-File-IsDir", fmt.Sprint(info.IsDir))
        w.WriteHeader(200)
    default:
        errResp(w, 405, "method not allowed")
    }
}
```

### 13.8 Tests

All tests run on real Firecracker VMs. No mocks.

- `TestFileWriteRead` — write "hello world" to `/workspace/test.txt`,
  read back, verify content matches.
- `TestFileReadNotFound` — read `/nonexistent`, verify ERROR.
- `TestFileStat` — write a file, stat it, verify size and mode.
- `TestFileList` — write several files, list `/workspace/`, verify names.
- `TestFileLargeRoundTrip` — write 10MB of random data, read back, verify
  SHA256 checksum matches.
- `TestFileWritePermissions` — write with mode "0600", verify file mode
  inside the VM.
- `TestFileWriteCreatesDirs` — write to `/workspace/deep/nested/file.txt`,
  verify intermediate directories are created.
- `TestFileListEmpty` — list empty directory, verify empty JSON array.

---

## Part 17 — Template-Free Sandbox Creation

Current `POST /sandboxes` requires `template_id`. For CLI usage, support
direct creation without a template.

### 17.1 API Change

```json
POST /sandboxes
{
  "name": "my-sandbox",
  "cpus": 1,
  "memory_mb": 512,
  "env": {"API_KEY": "sk-..."},
  "init": "npm install",
  "new_volumes": [{"name": "work", "size_mb": 256, "mount": "/workspace"}]
}
```

If `template_id` is set, use template (existing behavior). If absent, use
the fields directly. Defaults: 1 CPU, 512MB RAM, no volumes, no init.

### 17.2 Request Struct Change

```go
// pkg/server/routes.go

type createSandboxReq struct {
    Name       string               `json:"name"`
    TemplateID string               `json:"template_id,omitempty"`
    CPUs       float64              `json:"cpus,omitempty"`
    MemoryMB   int                  `json:"memory_mb,omitempty"`
    Env        map[string]string    `json:"env,omitempty"`
    Init       string               `json:"init,omitempty"`
    NewVolumes []engine.VolumeSpec  `json:"new_volumes,omitempty"`
    Volumes    []engine.VolumeMount `json:"volumes,omitempty"`
}
```

### 17.3 Handler Change

```go
case http.MethodPost:
    var req createSandboxReq
    readJSON(r, &req)

    var spec engine.SandboxSpec

    if req.TemplateID != "" {
        // Existing template-based path (unchanged)
        tmpl, _ := s.store.GetTemplate(req.TemplateID)
        spec = engine.SandboxSpec{
            Name: req.Name, Image: tmpl.Image,
            CPUs: tmpl.CPUs, MemoryMB: tmpl.MemoryMB,
            // ... resolve secrets, volumes from template ...
        }
    } else {
        // Direct creation — no template needed
        spec = engine.SandboxSpec{
            Name:       req.Name,
            CPUs:       req.CPUs,
            MemoryMB:   req.MemoryMB,
            Env:        req.Env,
            Init:       req.Init,
            NewVolumes: req.NewVolumes,
        }
    }

    if spec.Name == "" {
        spec.Name = "sandbox-" + genID()[:6]
    }

    info, err := s.engine.Create(r.Context(), spec)
    // ... store sandbox, persist state (same as before) ...
```

### 17.4 Tests

- `TestCreateWithoutTemplate` — POST /sandboxes without template_id, verify
  sandbox is created with specified CPUs/memory.
- `TestCreateWithoutTemplateDefaults` — POST with only `{"name":"foo"}`,
  verify defaults (1 CPU, 512MB).
- `TestCreateWithTemplateStillWorks` — existing template-based flow unchanged.

---

## Part 18 — CLI

Same binary as the daemon. Single file. No CLI framework — `os.Args` + `flag`.

### 18.1 Mode Detection

```go
// cmd/bhatti/main.go

func main() {
    if len(os.Args) > 1 && os.Args[1] != "serve" {
        runCLI()
        return
    }
    runDaemon() // current main() logic, renamed
}
```

### 18.2 CLI Config

```go
// cmd/bhatti/cli.go

var (
    apiURL   = envOr("BHATTI_URL", "http://localhost:8080")
    apiToken = envOr("BHATTI_TOKEN", "")
)

func init() {
    if apiToken == "" {
        if cfg, err := pkg.LoadConfig(); err == nil {
            apiToken = cfg.AuthToken
        }
    }
}
```

### 18.3 HTTP Helpers

```go
func apiRequest(method, path string, body any) (*http.Response, error) {
    var r io.Reader
    if body != nil {
        data, _ := json.Marshal(body)
        r = bytes.NewReader(data)
    }
    req, _ := http.NewRequest(method, apiURL+path, r)
    req.Header.Set("Content-Type", "application/json")
    if apiToken != "" {
        req.Header.Set("Authorization", "Bearer "+apiToken)
    }
    return http.DefaultClient.Do(req)
}

func apiJSON(method, path string, body any, result any) error {
    resp, err := apiRequest(method, path, body)
    if err != nil { return err }
    defer resp.Body.Close()
    if resp.StatusCode >= 400 {
        var errBody struct{ Error string `json:"error"` }
        json.NewDecoder(resp.Body).Decode(&errBody)
        return fmt.Errorf("%s: %s", resp.Status, errBody.Error)
    }
    if result != nil {
        return json.NewDecoder(resp.Body).Decode(result)
    }
    return nil
}
```

### 18.4 Command Router

```go
func runCLI() {
    if len(os.Args) < 2 {
        printUsage()
        os.Exit(1)
    }
    switch os.Args[1] {
    case "create":   cmdCreate(os.Args[2:])
    case "list","ls": cmdList(os.Args[2:])
    case "destroy","rm": cmdDestroy(os.Args[2:])
    case "exec":     cmdExec(os.Args[2:])
    case "shell","sh": cmdShell(os.Args[2:])
    case "ps":       cmdPS(os.Args[2:])
    case "file":     cmdFile(os.Args[2:])
    case "secret":   cmdSecret(os.Args[2:])
    default:
        fmt.Fprintf(os.Stderr, "unknown command: %s\n", os.Args[1])
        printUsage()
        os.Exit(1)
    }
}

func printUsage() {
    fmt.Fprintf(os.Stderr, `Usage: bhatti <command> [args]

Commands:
  serve                         Start the bhatti daemon
  create [flags]                Create a new sandbox
  list                          List sandboxes
  destroy <id|name>             Destroy a sandbox
  exec <id|name> -- CMD...      Execute a command
  shell <id|name>               Open an interactive shell
  ps <id|name>                  List sessions in a sandbox
  file read|write|ls <id> PATH  File operations
  secret set|list|delete        Manage secrets

Environment:
  BHATTI_URL     API endpoint (default: http://localhost:8080)
  BHATTI_TOKEN   Auth token (default: from ~/.bhatti/config.yaml)
`)
}
```

### 18.5 create

```go
func cmdCreate(args []string) {
    fs := flag.NewFlagSet("create", flag.ExitOnError)
    name := fs.String("name", "", "sandbox name")
    cpus := fs.Float64("cpus", 1, "vCPUs")
    memory := fs.Int("memory", 512, "memory in MB")
    env := fs.String("env", "", "env vars (K=V,K=V)")
    initCmd := fs.String("init", "", "init script")
    fs.Parse(args)

    envMap := parseEnvFlag(*env)

    req := map[string]any{
        "name": *name, "cpus": *cpus, "memory_mb": *memory,
    }
    if len(envMap) > 0  { req["env"] = envMap }
    if *initCmd != ""   { req["init"] = *initCmd }

    var sb struct {
        ID   string `json:"id"`
        Name string `json:"name"`
        IP   string `json:"ip"`
    }
    if err := apiJSON("POST", "/sandboxes", req, &sb); err != nil {
        fatal(err)
    }
    fmt.Printf("%s\t%s\t%s\n", sb.ID, sb.Name, sb.IP)
}
```

### 18.6 list

```go
func cmdList(args []string) {
    var sandboxes []struct {
        ID     string `json:"id"`
        Name   string `json:"name"`
        Status string `json:"status"`
        IP     string `json:"ip"`
    }
    if err := apiJSON("GET", "/sandboxes", nil, &sandboxes); err != nil {
        fatal(err)
    }
    fmt.Printf("%-20s %-20s %-10s %-16s\n", "ID", "NAME", "STATUS", "IP")
    for _, sb := range sandboxes {
        fmt.Printf("%-20s %-20s %-10s %-16s\n", sb.ID, sb.Name, sb.Status, sb.IP)
    }
}
```

### 18.7 destroy

```go
func cmdDestroy(args []string) {
    if len(args) == 0 { fatal("usage: bhatti destroy <id|name>") }
    id := resolveID(args[0])
    if err := apiJSON("DELETE", "/sandboxes/"+id, nil, nil); err != nil {
        fatal(err)
    }
    fmt.Println("destroyed")
}
```

### 18.8 exec

```go
func cmdExec(args []string) {
    var target string
    var cmd []string
    for i, a := range args {
        if a == "--" {
            target = args[0]
            cmd = args[i+1:]
            break
        }
    }
    if target == "" || len(cmd) == 0 {
        fatal("usage: bhatti exec <id|name> -- CMD...")
    }
    id := resolveID(target)

    var result engine.ExecResult
    if err := apiJSON("POST", "/sandboxes/"+id+"/exec", map[string]any{
        "cmd": cmd,
    }, &result); err != nil {
        fatal(err)
    }
    os.Stdout.WriteString(result.Stdout)
    os.Stderr.WriteString(result.Stderr)
    os.Exit(result.ExitCode)
}
```

### 18.9 shell

WebSocket + raw terminal. Uses `golang.org/x/term` (already an indirect
dependency via `golang.org/x/crypto`) and `gorilla/websocket` (already
a dependency).

```go
func cmdShell(args []string) {
    if len(args) == 0 { fatal("usage: bhatti shell <id|name>") }
    id := resolveID(args[0])

    wsURL := strings.Replace(apiURL, "http://", "ws://", 1)
    wsURL = strings.Replace(wsURL, "https://", "wss://", 1)
    header := http.Header{}
    if apiToken != "" {
        header.Set("Authorization", "Bearer "+apiToken)
    }
    conn, _, err := websocket.DefaultDialer.Dial(
        wsURL+"/sandboxes/"+id+"/ws", header)
    if err != nil { fatal(err) }
    defer conn.Close()

    // Raw terminal mode
    oldState, err := term.MakeRaw(int(os.Stdin.Fd()))
    if err != nil { fatal(err) }
    defer term.Restore(int(os.Stdin.Fd()), oldState)

    // Initial size
    w, h, _ := term.GetSize(int(os.Stdin.Fd()))
    conn.WriteJSON(map[string]any{"type": "resize", "rows": h, "cols": w})

    // SIGWINCH → resize
    sigwinch := make(chan os.Signal, 1)
    signal.Notify(sigwinch, syscall.SIGWINCH)
    go func() {
        for range sigwinch {
            w, h, _ := term.GetSize(int(os.Stdin.Fd()))
            conn.WriteJSON(map[string]any{
                "type": "resize", "rows": h, "cols": w,
            })
        }
    }()

    // WebSocket → stdout
    done := make(chan struct{})
    go func() {
        defer close(done)
        for {
            _, msg, err := conn.ReadMessage()
            if err != nil { return }
            os.Stdout.Write(msg)
        }
    }()

    // stdin → WebSocket (Ctrl+\ = detach)
    go func() {
        buf := make([]byte, 4096)
        for {
            n, err := os.Stdin.Read(buf)
            if err != nil { conn.Close(); return }
            for i := 0; i < n; i++ {
                if buf[i] == 0x1c { // Ctrl+backslash
                    term.Restore(int(os.Stdin.Fd()), oldState)
                    fmt.Fprintf(os.Stderr, "\r\ndetached\r\n")
                    conn.Close()
                    return
                }
            }
            conn.WriteMessage(websocket.BinaryMessage, buf[:n])
        }
    }()

    <-done
}
```

### 18.10 ps (session list)

Requires a new server route: `GET /sandboxes/:id/sessions`.

```go
// Server route addition:
//   case "sessions":
//       s.handleSandboxSessions(w, r, id)

func (s *Server) handleSandboxSessions(w http.ResponseWriter, r *http.Request, id string) {
    sb, err := s.store.GetSandbox(id)
    if err != nil { errResp(w, 404, "not found"); return }
    if err := s.ensureHot(r.Context(), sb.EngineID); err != nil {
        errResp(w, 500, "wake sandbox: "+err.Error()); return
    }
    vm, _ := eng.getVM(sb.EngineID)
    sessions, err := vm.Agent.SessionList(r.Context())
    if err != nil { errResp(w, 500, err.Error()); return }
    writeJSON(w, 200, sessions)
}
```

```go
// CLI:
func cmdPS(args []string) {
    if len(args) == 0 { fatal("usage: bhatti ps <id|name>") }
    id := resolveID(args[0])

    var sessions []struct {
        SessionID string `json:"session_id"`
        Argv      string `json:"argv"`
        Running   bool   `json:"running"`
        Attached  bool   `json:"attached"`
    }
    if err := apiJSON("GET", "/sandboxes/"+id+"/sessions", nil, &sessions); err != nil {
        fatal(err)
    }
    fmt.Printf("%-10s %-40s %-8s %-8s\n", "ID", "COMMAND", "RUNNING", "ATTACHED")
    for _, s := range sessions {
        fmt.Printf("%-10s %-40s %-8v %-8v\n",
            s.SessionID, s.Argv, s.Running, s.Attached)
    }
}
```

### 18.11 file

```go
func cmdFile(args []string) {
    if len(args) < 1 { fatal("usage: bhatti file read|write|ls <id> <path>") }
    switch args[0] {
    case "read":
        if len(args) < 3 { fatal("usage: bhatti file read <id> <path>") }
        id := resolveID(args[1])
        resp, err := apiRequest("GET",
            "/sandboxes/"+id+"/files?path="+url.QueryEscape(args[2]), nil)
        if err != nil { fatal(err) }
        defer resp.Body.Close()
        if resp.StatusCode >= 400 {
            body, _ := io.ReadAll(resp.Body)
            fatal(string(body))
        }
        io.Copy(os.Stdout, resp.Body)

    case "write":
        if len(args) < 3 { fatal("usage: bhatti file write <id> <path> < file") }
        id := resolveID(args[1])
        // Read all stdin to get Content-Length
        data, _ := io.ReadAll(os.Stdin)
        req, _ := http.NewRequest("PUT",
            apiURL+"/sandboxes/"+id+"/files?path="+url.QueryEscape(args[2]),
            bytes.NewReader(data))
        req.Header.Set("Content-Type", "application/octet-stream")
        req.ContentLength = int64(len(data))
        if apiToken != "" {
            req.Header.Set("Authorization", "Bearer "+apiToken)
        }
        resp, err := http.DefaultClient.Do(req)
        if err != nil { fatal(err) }
        resp.Body.Close()
        if resp.StatusCode >= 400 { fatal("write failed") }
        fmt.Println("ok")

    case "ls":
        if len(args) < 3 { fatal("usage: bhatti file ls <id> <path>") }
        id := resolveID(args[1])
        var files []struct {
            Name  string `json:"name"`
            Size  int64  `json:"size"`
            IsDir bool   `json:"is_dir"`
            Mode  string `json:"mode"`
        }
        if err := apiJSON("GET",
            "/sandboxes/"+id+"/files?path="+url.QueryEscape(args[2])+"&ls=true",
            nil, &files); err != nil {
            fatal(err)
        }
        for _, f := range files {
            dirFlag := "-"
            if f.IsDir { dirFlag = "d" }
            fmt.Printf("%s%s %8d %s\n", dirFlag, f.Mode, f.Size, f.Name)
        }
    }
}
```

### 18.12 secret

```go
func cmdSecret(args []string) {
    if len(args) == 0 { fatal("usage: bhatti secret set|list|delete") }
    switch args[0] {
    case "set":
        if len(args) < 3 { fatal("usage: bhatti secret set NAME VALUE") }
        if err := apiJSON("POST", "/secrets", map[string]any{
            "name": args[1], "value": args[2],
        }, nil); err != nil { fatal(err) }
        fmt.Println("ok")
    case "list":
        var secrets []struct{ Name string `json:"name"` }
        apiJSON("GET", "/secrets", nil, &secrets)
        for _, s := range secrets { fmt.Println(s.Name) }
    case "delete":
        if len(args) < 2 { fatal("usage: bhatti secret delete NAME") }
        apiJSON("DELETE", "/secrets/"+args[1], nil, nil)
        fmt.Println("deleted")
    }
}
```

### 18.13 Name-to-ID Resolution

```go
func resolveID(nameOrID string) string {
    resp, err := apiRequest("GET", "/sandboxes/"+nameOrID, nil)
    if err == nil && resp.StatusCode == 200 {
        resp.Body.Close()
        return nameOrID
    }
    if resp != nil { resp.Body.Close() }

    var sandboxes []struct {
        ID   string `json:"id"`
        Name string `json:"name"`
    }
    apiJSON("GET", "/sandboxes", nil, &sandboxes)
    for _, sb := range sandboxes {
        if sb.Name == nameOrID { return sb.ID }
    }
    fmt.Fprintf(os.Stderr, "sandbox %q not found\n", nameOrID)
    os.Exit(1)
    return ""
}
```

### 18.14 Tests

- `TestCLICreate` — `bhatti create --name test-cli`, verify output has ID.
- `TestCLIList` — create, list, verify sandbox in table.
- `TestCLIExec` — `bhatti exec <name> -- echo hello`, verify stdout + exit 0.
- `TestCLIExecFailure` — `bhatti exec <name> -- false`, verify exit code 1.
- `TestCLIDestroy` — create, destroy, verify list is empty.
- `TestCLIFileWriteRead` — write file via CLI, read back, verify content.
- `TestCLIFileLS` — write files, `bhatti file ls`, verify names.
- `TestCLIPS` — create sandbox with TTY session, `bhatti ps`, verify listed.

---

## Part 19 — Deployment

### 19.1 bhatti serve

Rename current `main()` daemon logic to `runDaemon()`. When
`os.Args[1] == "serve"` or no args, call `runDaemon()`.

### 19.2 Systemd Service

```ini
# deploy/bhatti.service

[Unit]
Description=Bhatti Sandbox Infrastructure
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/bhatti serve
WorkingDirectory=/var/lib/bhatti
Environment=HOME=/root
Restart=always
RestartSec=5
ExecStopPost=/bin/sh -c 'ip -o link show type tun | grep tap | cut -d: -f2 | xargs -r -n1 ip link del'
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

### 19.3 Config

Install script generates `/var/lib/bhatti/config.yaml`. Architecture-aware
paths:

```yaml
# aarch64
engine: firecracker
listen: :8080
auth_token: <random-32-hex>
data_dir: /var/lib/bhatti
firecracker_bin: /usr/local/bin/firecracker
firecracker_kernel: /var/lib/bhatti/images/vmlinux-arm64
firecracker_rootfs: /var/lib/bhatti/images/rootfs-base-arm64.ext4
```

```yaml
# x86_64
engine: firecracker
listen: :8080
auth_token: <random-32-hex>
data_dir: /var/lib/bhatti
firecracker_bin: /usr/local/bin/firecracker
firecracker_kernel: /var/lib/bhatti/images/vmlinux-amd64
firecracker_rootfs: /var/lib/bhatti/images/rootfs-base-amd64.ext4
```

---

## Part 20 — Install Script

One command on a fresh Linux host with KVM:

```bash
curl -fsSL https://bhatti.sh/install.sh | sudo bash
```

Supports both aarch64 and x86_64.

### 20.1 Script

```bash
#!/bin/bash
# scripts/install.sh
set -euo pipefail

BHATTI_VERSION="${BHATTI_VERSION:-latest}"
FC_VERSION="1.6.0"
DATA_DIR="/var/lib/bhatti"

# --- Preflight ---

if [[ $EUID -ne 0 ]]; then
    echo "error: must run as root" >&2; exit 1
fi

HOST_ARCH=$(uname -m)
case "$HOST_ARCH" in
    aarch64) FC_ARCH="aarch64"; GO_ARCH="arm64"; DEB_ARCH="arm64" ;;
    x86_64)  FC_ARCH="x86_64";  GO_ARCH="amd64"; DEB_ARCH="amd64" ;;
    *) echo "error: unsupported architecture $HOST_ARCH" >&2; exit 1 ;;
esac

if [[ ! -e /dev/kvm ]]; then
    modprobe kvm 2>/dev/null || true
    if [[ ! -e /dev/kvm ]]; then
        echo "error: /dev/kvm not available — KVM required" >&2; exit 1
    fi
fi

echo "==> Installing bhatti on $(hostname) ($HOST_ARCH)"

# --- Directories ---

mkdir -p "$DATA_DIR"/{images,sandboxes}

# --- Firecracker ---

if [[ ! -f /usr/local/bin/firecracker ]]; then
    echo "==> Installing Firecracker ${FC_VERSION}..."
    curl -fsSL \
        "https://github.com/firecracker-microvm/firecracker/releases/download/v${FC_VERSION}/firecracker-v${FC_VERSION}-${FC_ARCH}.tgz" \
        | tar xz
    mv "release-v${FC_VERSION}-${FC_ARCH}/firecracker-v${FC_VERSION}-${FC_ARCH}" \
        /usr/local/bin/firecracker
    chmod +x /usr/local/bin/firecracker
    rm -rf "release-v${FC_VERSION}-${FC_ARCH}"
fi

# --- Bhatti + Lohar binaries ---

echo "==> Downloading bhatti and lohar..."
if [[ "$BHATTI_VERSION" == "latest" ]]; then
    BHATTI_VERSION=$(curl -fsSL \
        https://api.github.com/repos/sahil-shubham/bhatti/releases/latest \
        | grep tag_name | cut -d'"' -f4)
fi
RELEASE_URL="https://github.com/sahil-shubham/bhatti/releases/download/${BHATTI_VERSION}"
curl -fsSL "${RELEASE_URL}/bhatti-linux-${GO_ARCH}" -o /usr/local/bin/bhatti
curl -fsSL "${RELEASE_URL}/lohar-linux-${GO_ARCH}" -o "$DATA_DIR/lohar"
chmod +x /usr/local/bin/bhatti "$DATA_DIR/lohar"

# --- Kernel ---

KERNEL_PATH="$DATA_DIR/images/vmlinux-${GO_ARCH}"
if [[ ! -f "$KERNEL_PATH" ]]; then
    echo "==> Downloading kernel..."
    curl -fsSL \
        "https://s3.amazonaws.com/spec.ccfc.min/img/quickstart_guide/${FC_ARCH}/kernels/vmlinux.bin" \
        -o "$KERNEL_PATH"
fi

# --- Rootfs ---

ROOTFS_PATH="$DATA_DIR/images/rootfs-base-${DEB_ARCH}.ext4"
if [[ ! -f "$ROOTFS_PATH" ]]; then
    echo "==> Building rootfs (this takes ~10 minutes)..."
    apt-get update -qq && apt-get install -y -qq debootstrap
    curl -fsSL "${RELEASE_URL}/build-rootfs.sh" -o /tmp/build-rootfs.sh
    chmod +x /tmp/build-rootfs.sh
    /tmp/build-rootfs.sh "$DATA_DIR/lohar"
    rm -f /tmp/build-rootfs.sh
fi

# --- Config ---

if [[ ! -f "$DATA_DIR/config.yaml" ]]; then
    echo "==> Generating config..."
    TOKEN=$(head -c 16 /dev/urandom | xxd -p)
    cat > "$DATA_DIR/config.yaml" << EOF
engine: firecracker
listen: :8080
auth_token: ${TOKEN}
data_dir: ${DATA_DIR}
firecracker_bin: /usr/local/bin/firecracker
firecracker_kernel: ${KERNEL_PATH}
firecracker_rootfs: ${ROOTFS_PATH}
EOF
fi

# --- Age key ---

if [[ ! -f "$DATA_DIR/age.key" ]]; then
    touch "$DATA_DIR/age.key"
    chmod 600 "$DATA_DIR/age.key"
fi

# --- Systemd ---

echo "==> Installing systemd service..."
cat > /etc/systemd/system/bhatti.service << 'EOF'
[Unit]
Description=Bhatti Sandbox Infrastructure
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/bhatti serve
WorkingDirectory=/var/lib/bhatti
Environment=HOME=/root
Restart=always
RestartSec=5
ExecStopPost=/bin/sh -c 'ip -o link show type tun | grep tap | cut -d: -f2 | xargs -r -n1 ip link del'
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable bhatti
systemctl start bhatti

# --- Summary ---

TOKEN=$(grep auth_token "$DATA_DIR/config.yaml" | awk '{print $2}')
echo ""
echo "============================================"
echo "  bhatti is running on :8080"
echo "  auth token: ${TOKEN}"
echo ""
echo "  Quick start:"
echo "    export BHATTI_TOKEN=${TOKEN}"
echo "    bhatti create --name hello"
echo "    bhatti exec hello -- echo 'it works'"
echo "    bhatti shell hello"
echo "    bhatti destroy hello"
echo "============================================"
```

---

## What we're NOT building for launch

- OCI image pull/push (build-rootfs.sh works)
- Multi-user / RBAC (single bearer token)
- SDKs beyond Go (CLI is the interface)
- Web UI (CLI + API is sufficient)
- Diff snapshots (optimization for later)
- Custom rootfs flag (bring-your-own-ext4 — future)
