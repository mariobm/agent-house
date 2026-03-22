package main

import (
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/gorilla/websocket"
	"golang.org/x/term"

	"github.com/sahil-shubham/bhatti/pkg"
	"github.com/sahil-shubham/bhatti/pkg/store"
)

var (
	apiURL   = envOr("BHATTI_URL", "http://localhost:8080")
	apiToken = envOr("BHATTI_TOKEN", "")
)

func init() {
	if apiToken == "" {
		if cfg, err := pkg.LoadConfig(); err == nil && cfg.AuthToken != "" {
			apiToken = cfg.AuthToken
		}
	}
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

// --- HTTP helpers ---

func apiRequest(method, path string, body any) (*http.Response, error) {
	var r io.Reader
	if body != nil {
		data, _ := json.Marshal(body)
		r = bytes.NewReader(data)
	}
	req, err := http.NewRequest(method, apiURL+path, r)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	if apiToken != "" {
		req.Header.Set("Authorization", "Bearer "+apiToken)
	}
	return http.DefaultClient.Do(req)
}

func apiJSON(method, path string, body any, result any) error {
	resp, err := apiRequest(method, path, body)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		var errBody struct {
			Error string `json:"error"`
		}
		json.NewDecoder(resp.Body).Decode(&errBody)
		return fmt.Errorf("%s: %s", resp.Status, errBody.Error)
	}
	if result != nil {
		return json.NewDecoder(resp.Body).Decode(result)
	}
	return nil
}

func fatal(args ...any) {
	fmt.Fprintln(os.Stderr, args...)
	os.Exit(1)
}

// --- Command router ---

func runCLI() {
	if len(os.Args) < 2 {
		printUsage()
		os.Exit(1)
	}
	switch os.Args[1] {
	case "create":
		cmdCreate(os.Args[2:])
	case "list", "ls":
		cmdList(os.Args[2:])
	case "destroy", "rm":
		cmdDestroy(os.Args[2:])
	case "exec":
		cmdExec(os.Args[2:])
	case "shell", "sh":
		cmdShell(os.Args[2:])
	case "ps":
		cmdPS(os.Args[2:])
	case "file":
		cmdFile(os.Args[2:])
	case "secret":
		cmdSecret(os.Args[2:])
	case "user":
		cmdUser(os.Args[2:])
	case "setup":
		cmdSetup(os.Args[2:])
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
  user create|list|delete|rotate-key  Manage users (local)
  setup                         Configure CLI (endpoint + API key)

Environment:
  BHATTI_URL     API endpoint (default: http://localhost:8080)
  BHATTI_TOKEN   Auth token (default: from ~/.bhatti/config.yaml)
`)
}

// --- create ---

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
	if len(envMap) > 0 {
		req["env"] = envMap
	}
	if *initCmd != "" {
		req["init"] = *initCmd
	}

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

func parseEnvFlag(s string) map[string]string {
	if s == "" {
		return nil
	}
	m := map[string]string{}
	for _, kv := range strings.Split(s, ",") {
		parts := strings.SplitN(kv, "=", 2)
		if len(parts) == 2 {
			m[parts[0]] = parts[1]
		}
	}
	return m
}

// --- list ---

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

// --- destroy ---

func cmdDestroy(args []string) {
	if len(args) == 0 {
		fatal("usage: bhatti destroy <id|name>")
	}
	id := resolveID(args[0])
	if err := apiJSON("DELETE", "/sandboxes/"+id, nil, nil); err != nil {
		fatal(err)
	}
	fmt.Println("destroyed")
}

// --- exec ---

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

	var result struct {
		ExitCode int    `json:"exit_code"`
		Stdout   string `json:"stdout"`
		Stderr   string `json:"stderr"`
	}
	if err := apiJSON("POST", "/sandboxes/"+id+"/exec", map[string]any{
		"cmd": cmd,
	}, &result); err != nil {
		fatal(err)
	}
	os.Stdout.WriteString(result.Stdout)
	os.Stderr.WriteString(result.Stderr)
	os.Exit(result.ExitCode)
}

// --- shell ---

func cmdShell(args []string) {
	if len(args) == 0 {
		fatal("usage: bhatti shell <id|name>")
	}
	id := resolveID(args[0])

	wsURL := strings.Replace(apiURL, "http://", "ws://", 1)
	wsURL = strings.Replace(wsURL, "https://", "wss://", 1)
	header := http.Header{}
	if apiToken != "" {
		header.Set("Authorization", "Bearer "+apiToken)
	}
	conn, _, err := websocket.DefaultDialer.Dial(
		wsURL+"/sandboxes/"+id+"/ws", header)
	if err != nil {
		fatal(err)
	}
	defer conn.Close()

	// Raw terminal mode
	oldState, err := term.MakeRaw(int(os.Stdin.Fd()))
	if err != nil {
		fatal(err)
	}
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
			if err != nil {
				return
			}
			os.Stdout.Write(msg)
		}
	}()

	// stdin → WebSocket (Ctrl+\ = detach)
	go func() {
		buf := make([]byte, 4096)
		for {
			n, err := os.Stdin.Read(buf)
			if err != nil {
				conn.Close()
				return
			}
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

// --- ps ---

func cmdPS(args []string) {
	if len(args) == 0 {
		fatal("usage: bhatti ps <id|name>")
	}
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

// --- file ---

func cmdFile(args []string) {
	if len(args) < 1 {
		fatal("usage: bhatti file read|write|ls <id> <path>")
	}
	switch args[0] {
	case "read":
		if len(args) < 3 {
			fatal("usage: bhatti file read <id> <path>")
		}
		id := resolveID(args[1])
		resp, err := apiRequest("GET",
			"/sandboxes/"+id+"/files?path="+url.QueryEscape(args[2]), nil)
		if err != nil {
			fatal(err)
		}
		defer resp.Body.Close()
		if resp.StatusCode >= 400 {
			body, _ := io.ReadAll(resp.Body)
			fatal(string(body))
		}
		io.Copy(os.Stdout, resp.Body)

	case "write":
		if len(args) < 3 {
			fatal("usage: bhatti file write <id> <path> < file")
		}
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
		if err != nil {
			fatal(err)
		}
		defer resp.Body.Close()
		if resp.StatusCode >= 400 {
			body, _ := io.ReadAll(resp.Body)
			fatal(string(body))
		}
		fmt.Println("ok")

	case "ls":
		if len(args) < 3 {
			fatal("usage: bhatti file ls <id> <path>")
		}
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
			if f.IsDir {
				dirFlag = "d"
			}
			fmt.Printf("%s%s %8d %s\n", dirFlag, f.Mode, f.Size, f.Name)
		}

	default:
		fatal("usage: bhatti file read|write|ls <id> <path>")
	}
}

// --- secret ---

func cmdSecret(args []string) {
	if len(args) == 0 {
		fatal("usage: bhatti secret set|list|delete")
	}
	switch args[0] {
	case "set":
		if len(args) < 3 {
			fatal("usage: bhatti secret set NAME VALUE")
		}
		if err := apiJSON("POST", "/secrets", map[string]any{
			"name": args[1], "value": args[2],
		}, nil); err != nil {
			fatal(err)
		}
		fmt.Println("ok")
	case "list":
		var secrets []struct {
			Name string `json:"name"`
		}
		if err := apiJSON("GET", "/secrets", nil, &secrets); err != nil {
			fatal(err)
		}
		for _, s := range secrets {
			fmt.Println(s.Name)
		}
	case "delete":
		if len(args) < 2 {
			fatal("usage: bhatti secret delete NAME")
		}
		if err := apiJSON("DELETE", "/secrets/"+args[1], nil, nil); err != nil {
			fatal(err)
		}
		fmt.Println("deleted")
	default:
		fatal("usage: bhatti secret set|list|delete")
	}
}

// --- user (local commands — operate on SQLite directly) ---

func cmdUser(args []string) {
	if len(args) == 0 {
		fatal("usage: bhatti user create|list|delete|rotate-key")
	}
	switch args[0] {
	case "create":
		cmdUserCreate(args[1:])
	case "list":
		cmdUserList()
	case "delete":
		cmdUserDelete(args[1:])
	case "rotate-key":
		cmdUserRotateKey(args[1:])
	default:
		fatal("usage: bhatti user create|list|delete|rotate-key")
	}
}

func cmdUserCreate(args []string) {
	fs := flag.NewFlagSet("user create", flag.ExitOnError)
	name := fs.String("name", "", "user name (required)")
	maxSandboxes := fs.Int("max-sandboxes", 5, "max sandboxes for this user")
	maxCPUs := fs.Int("max-cpus", 4, "max CPUs per sandbox")
	maxMemory := fs.Int("max-memory", 4096, "max memory MB per sandbox")
	fs.Parse(args)

	if *name == "" {
		fatal("usage: bhatti user create --name NAME [--max-sandboxes N]")
	}

	st := openLocalStore()
	defer st.Close()

	// Generate API key
	apiKey := generateAPIKey()
	keyHash := sha256HexCLI(apiKey)

	// Allocate subnet index
	subnetIdx, err := st.NextSubnetIndex()
	if err != nil {
		fatal("subnet index:", err)
	}

	// Generate user ID
	idBytes := make([]byte, 4)
	rand.Read(idBytes)
	userID := "usr_" + hex.EncodeToString(idBytes)

	u := store.User{
		ID:                    userID,
		Name:                  *name,
		APIKeyHash:            keyHash,
		MaxSandboxes:          *maxSandboxes,
		MaxCPUsPerSandbox:     *maxCPUs,
		MaxMemoryMBPerSandbox: *maxMemory,
		SubnetIndex:           subnetIdx,
		CreatedAt:             time.Now(),
	}

	if err := st.CreateUser(u); err != nil {
		if strings.Contains(err.Error(), "UNIQUE") {
			fatal(fmt.Sprintf("user %q already exists", *name))
		}
		fatal("create user:", err)
	}

	fmt.Printf("User created:\n")
	fmt.Printf("  ID:       %s\n", u.ID)
	fmt.Printf("  Name:     %s\n", u.Name)
	fmt.Printf("  Subnet:   %d\n", u.SubnetIndex)
	fmt.Printf("  API key:  %s\n", apiKey)
	fmt.Println()
	fmt.Println("This key will not be shown again. Save it now.")
	fmt.Printf("\nQuick start:\n")
	fmt.Printf("  export BHATTI_URL=%s\n", apiURL)
	fmt.Printf("  export BHATTI_TOKEN=%s\n", apiKey)
	fmt.Printf("  bhatti create --name my-sandbox\n")
}

func cmdUserList() {
	st := openLocalStore()
	defer st.Close()

	users, err := st.ListUsers()
	if err != nil {
		fatal("list users:", err)
	}

	fmt.Printf("%-12s %-20s %-8s %-6s %-6s %-8s\n",
		"ID", "NAME", "SANDBOXES", "CPUS", "MEM", "SUBNET")
	for _, u := range users {
		count, _ := st.CountUserSandboxes(u.ID)
		fmt.Printf("%-12s %-20s %d/%-6d %-6d %-6d %-8d\n",
			u.ID, u.Name, count, u.MaxSandboxes,
			u.MaxCPUsPerSandbox, u.MaxMemoryMBPerSandbox, u.SubnetIndex)
	}
}

func cmdUserDelete(args []string) {
	if len(args) == 0 {
		fatal("usage: bhatti user delete <name>")
	}

	st := openLocalStore()
	defer st.Close()

	// Find user by name
	users, _ := st.ListUsers()
	var userID string
	for _, u := range users {
		if u.Name == args[0] {
			userID = u.ID
			break
		}
	}
	if userID == "" {
		fatal(fmt.Sprintf("user %q not found", args[0]))
	}

	if err := st.DeleteUser(userID); err != nil {
		fatal(err)
	}
	fmt.Printf("deleted user %q\n", args[0])
}

func cmdUserRotateKey(args []string) {
	if len(args) == 0 {
		fatal("usage: bhatti user rotate-key <name>")
	}

	st := openLocalStore()
	defer st.Close()

	// Find user by name
	users, _ := st.ListUsers()
	var userID string
	for _, u := range users {
		if u.Name == args[0] {
			userID = u.ID
			break
		}
	}
	if userID == "" {
		fatal(fmt.Sprintf("user %q not found", args[0]))
	}

	newKey := generateAPIKey()
	newHash := sha256HexCLI(newKey)

	if err := st.RotateUserKey(userID, newHash); err != nil {
		fatal(err)
	}

	fmt.Printf("API key rotated for %q\n", args[0])
	fmt.Printf("  New key: %s\n", newKey)
	fmt.Println()
	fmt.Println("The old key is immediately invalidated.")
	fmt.Println("This key will not be shown again. Save it now.")
}

// openLocalStore opens the SQLite store from the local config.
// Used by user commands which operate directly on the DB.
func openLocalStore() *store.Store {
	cfg, err := pkg.LoadConfig()
	if err != nil {
		fatal("load config:", err)
	}
	st, err := store.New(filepath.Join(cfg.DataDir, "state.db"))
	if err != nil {
		fatal("open store:", err)
	}
	return st
}

func generateAPIKey() string {
	b := make([]byte, 32)
	rand.Read(b)
	return "bht_" + hex.EncodeToString(b)
}

func sha256HexCLI(s string) string {
	h := sha256.Sum256([]byte(s))
	return hex.EncodeToString(h[:])
}

// --- setup ---

func cmdSetup(args []string) {
	// Interactive configuration
	fmt.Printf("API endpoint [%s]: ", apiURL)
	var endpoint string
	fmt.Scanln(&endpoint)
	if endpoint == "" {
		endpoint = apiURL
	}

	fmt.Print("API key: ")
	keyBytes, err := term.ReadPassword(int(os.Stdin.Fd()))
	fmt.Println()
	if err != nil {
		fatal("read key:", err)
	}
	key := strings.TrimSpace(string(keyBytes))
	if key == "" {
		fatal("API key is required")
	}

	// Write config
	cfgDir := pkg.DefaultDataDir()
	os.MkdirAll(cfgDir, 0700)
	cfgPath := filepath.Join(cfgDir, "config.yaml")

	cfgContent := fmt.Sprintf("listen: %s\nauth_token: %s\n",
		strings.TrimPrefix(endpoint, "http://"),
		key)

	// For remote endpoints, store the URL as-is
	if strings.HasPrefix(endpoint, "https://") || strings.HasPrefix(endpoint, "http://") {
		cfgContent = fmt.Sprintf("auth_token: %s\n", key)
	}

	if err := os.WriteFile(cfgPath, []byte(cfgContent), 0600); err != nil {
		fatal("write config:", err)
	}
	fmt.Printf("Saved to %s\n", cfgPath)

	// Test connection
	fmt.Print("Testing connection... ")
	apiURL = endpoint
	apiToken = key
	var health map[string]any
	if err := apiJSON("GET", "/health", nil, &health); err != nil {
		fmt.Printf("✗ %v\n", err)
		return
	}
	fmt.Printf("✓ connected (sandboxes: %v, uptime: %v)\n",
		health["sandboxes"], health["uptime"])
}

// --- Name-to-ID resolution ---

func resolveID(nameOrID string) string {
	// Try direct ID lookup first
	resp, err := apiRequest("GET", "/sandboxes/"+nameOrID, nil)
	if err == nil && resp.StatusCode == 200 {
		resp.Body.Close()
		return nameOrID
	}
	if resp != nil {
		resp.Body.Close()
	}

	// Fall back to name search
	var sandboxes []struct {
		ID   string `json:"id"`
		Name string `json:"name"`
	}
	if err := apiJSON("GET", "/sandboxes", nil, &sandboxes); err != nil {
		fatal("cannot list sandboxes:", err)
	}
	for _, sb := range sandboxes {
		if sb.Name == nameOrID {
			return sb.ID
		}
	}
	fmt.Fprintf(os.Stderr, "sandbox %q not found\n", nameOrID)
	os.Exit(1)
	return ""
}
