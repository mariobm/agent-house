package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/sahil-shubham/bhatti/pkg"
	"github.com/sahil-shubham/bhatti/pkg/store"
)

// cliTest holds the test server + helpers for CLI integration tests.
// The CLI binary talks to a real bhatti HTTP server — either local or remote
// via Cloudflare tunnel. Config is loaded from ~/.bhatti/config.yaml.
type cliTest struct {
	t       *testing.T
	bhatti  string // path to bhatti binary
	baseURL string // daemon URL
	token   string // auth token
}

// run executes a bhatti CLI command and returns stdout, stderr, exit code.
func (c *cliTest) run(args ...string) (stdout, stderr string, exitCode int) {
	c.t.Helper()
	cmd := exec.Command(c.bhatti, args...)
	cmd.Env = append(os.Environ(),
		"BHATTI_URL="+c.baseURL,
		"BHATTI_TOKEN="+c.token,
	)
	var outBuf, errBuf strings.Builder
	cmd.Stdout = &outBuf
	cmd.Stderr = &errBuf
	err := cmd.Run()
	exitCode = 0
	if err != nil {
		if exitErr, ok := err.(*exec.ExitError); ok {
			exitCode = exitErr.ExitCode()
		} else {
			c.t.Fatalf("run %v: %v", args, err)
		}
	}
	return outBuf.String(), errBuf.String(), exitCode
}

// runJSON executes a bhatti CLI command with --json and unmarshals the output.
func (c *cliTest) runJSON(v any, args ...string) (exitCode int) {
	c.t.Helper()
	fullArgs := append([]string{"--json"}, args...)
	stdout, stderr, code := c.run(fullArgs...)
	if code != 0 {
		c.t.Fatalf("run %v: exit %d\nstdout: %s\nstderr: %s", args, code, stdout, stderr)
	}
	if err := json.Unmarshal([]byte(stdout), v); err != nil {
		c.t.Fatalf("json unmarshal %v: %v\nraw: %s", args, err, stdout)
	}
	return code
}

// setupCLITest builds the bhatti binary in a temp dir and loads connection
// config from ~/.bhatti/config.yaml (api_url + auth_token). This lets CLI
// tests run from any machine — the dev laptop, CI, or on the server itself.
//
// Skip conditions:
//   - No config file with api_url + auth_token → skip (no daemon to test against)
//   - Daemon unreachable → skip
func setupCLITest(t *testing.T) *cliTest {
	t.Helper()

	// Load config — same file the real CLI uses
	cfg, err := pkg.LoadConfig()
	if err != nil || cfg == nil {
		t.Skip("no bhatti config found")
	}

	baseURL := cfg.APIURL
	token := cfg.AuthToken

	// Allow env var overrides for CI
	if v := os.Getenv("BHATTI_URL"); v != "" {
		baseURL = v
	}
	if v := os.Getenv("BHATTI_TOKEN"); v != "" {
		token = v
	}

	if baseURL == "" || token == "" {
		t.Skip("bhatti config missing api_url or auth_token")
	}

	// Verify daemon is reachable
	client := &http.Client{Timeout: 10 * time.Second}
	req, _ := http.NewRequest("GET", baseURL+"/health", nil)
	resp, err := client.Do(req)
	if err != nil {
		t.Skipf("bhatti daemon not reachable at %s: %v", baseURL, err)
	}
	resp.Body.Close()
	if resp.StatusCode != 200 {
		t.Skipf("bhatti daemon unhealthy: %d", resp.StatusCode)
	}

	// Build the binary from source (tests the actual compiled code)
	binPath := filepath.Join(t.TempDir(), "bhatti")
	build := exec.Command("go", "build", "-o", binPath, "./cmd/bhatti/")
	build.Dir = projectRoot(t)
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("build bhatti: %s\n%v", out, err)
	}

	return &cliTest{
		t:       t,
		bhatti:  binPath,
		baseURL: baseURL,
		token:   token,
	}
}

// projectRoot returns the repo root by walking up from the test file.
func projectRoot(t *testing.T) string {
	t.Helper()
	// We're in cmd/bhatti/, go up two levels
	dir, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	for {
		if _, err := os.Stat(filepath.Join(dir, "go.mod")); err == nil {
			return dir
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatal("could not find project root (go.mod)")
		}
		dir = parent
	}
}

// --- Tests ---

func TestCLICreate(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, code := c.run("create", "--name", "cli-test-create")
	if code != 0 {
		t.Fatalf("exit %d", code)
	}
	// Output: ID\tNAME\tIP
	parts := strings.Fields(stdout)
	if len(parts) < 2 {
		t.Fatalf("unexpected output: %q", stdout)
	}
	sbID := parts[0]
	if sbID == "" {
		t.Fatal("no sandbox ID in output")
	}
	t.Logf("created: %s", strings.TrimSpace(stdout))

	// Cleanup
	t.Cleanup(func() { c.run("destroy", sbID) })
}

func TestCLIList(t *testing.T) {
	c := setupCLITest(t)

	// Create a sandbox
	stdout, _, _ := c.run("create", "--name", "cli-test-list")
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	// List
	stdout, _, code := c.run("list")
	if code != 0 {
		t.Fatalf("exit %d", code)
	}
	if !strings.Contains(stdout, "cli-test-list") {
		t.Errorf("sandbox not in list: %s", stdout)
	}
	t.Logf("list output:\n%s", stdout)
}

func TestCLIExec(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, _ := c.run("create", "--name", "cli-test-exec")
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	stdout, stderr, code := c.run("exec", "cli-test-exec", "--", "echo", "hello from cli")
	if code != 0 {
		t.Fatalf("exit %d, stderr: %s", code, stderr)
	}
	if strings.TrimSpace(stdout) != "hello from cli" {
		t.Errorf("stdout: %q", stdout)
	}
}

func TestCLIExecFailure(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, _ := c.run("create", "--name", "cli-test-execfail")
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	_, _, code := c.run("exec", "cli-test-execfail", "--", "false")
	if code != 1 {
		t.Errorf("exit code: %d, want 1", code)
	}
}

func TestCLIExecByName(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, _ := c.run("create", "--name", "cli-test-byname")
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	// Resolve by name
	stdout, _, code := c.run("exec", "cli-test-byname", "--", "hostname")
	if code != 0 {
		t.Fatalf("exit %d", code)
	}
	if strings.TrimSpace(stdout) != "cli-test-byname" {
		t.Errorf("hostname: %q, want cli-test-byname", strings.TrimSpace(stdout))
	}
}

func TestCLIDestroy(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, _ := c.run("create", "--name", "cli-test-destroy")
	sbID := strings.Fields(stdout)[0]

	stdout, _, code := c.run("destroy", sbID)
	if code != 0 {
		t.Fatalf("exit %d", code)
	}
	if !strings.Contains(stdout, "destroyed") {
		t.Errorf("output: %q", stdout)
	}

	// Verify gone from list
	stdout, _, _ = c.run("list")
	if strings.Contains(stdout, "cli-test-destroy") {
		t.Errorf("sandbox still in list after destroy")
	}
}

func TestCLIDestroyByName(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, _ := c.run("create", "--name", "cli-test-rmname")
	_ = strings.Fields(stdout)[0]

	stdout, _, code := c.run("rm", "cli-test-rmname")
	if code != 0 {
		t.Fatalf("exit %d", code)
	}
	if !strings.Contains(stdout, "destroyed") {
		t.Errorf("output: %q", stdout)
	}
}

func TestCLIFileWriteRead(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, _ := c.run("create", "--name", "cli-test-file")
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	// Write via stdin piped to the binary
	content := "hello from cli file write"
	cmd := exec.Command(c.bhatti, "file", "write", "cli-test-file", "/workspace/test.txt")
	cmd.Env = append(os.Environ(), "BHATTI_URL="+c.baseURL, "BHATTI_TOKEN="+c.token)
	cmd.Stdin = strings.NewReader(content)
	var outBuf strings.Builder
	cmd.Stdout = &outBuf
	if err := cmd.Run(); err != nil {
		t.Fatalf("file write: %v", err)
	}
	if !strings.Contains(outBuf.String(), "ok") {
		t.Errorf("write output: %q", outBuf.String())
	}

	// Read
	stdout, _, code := c.run("file", "read", "cli-test-file", "/workspace/test.txt")
	if code != 0 {
		t.Fatalf("file read exit %d", code)
	}
	if stdout != content {
		t.Errorf("read: %q, want %q", stdout, content)
	}
}

func TestCLIFileLS(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, _ := c.run("create", "--name", "cli-test-filels")
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	// Create some files via exec
	c.run("exec", "cli-test-filels", "--", "sh", "-c",
		"echo a > /workspace/a.txt && echo b > /workspace/b.txt && mkdir /workspace/sub")

	stdout, _, code := c.run("file", "ls", "cli-test-filels", "/workspace/")
	if code != 0 {
		t.Fatalf("file ls exit %d", code)
	}
	if !strings.Contains(stdout, "a.txt") || !strings.Contains(stdout, "b.txt") || !strings.Contains(stdout, "sub") {
		t.Errorf("ls output: %s", stdout)
	}
	t.Logf("ls output:\n%s", stdout)
}

func TestCLIPS(t *testing.T) {
	c := setupCLITest(t)

	// Create sandbox with init script (creates a session)
	stdout, _, _ := c.run("create", "--name", "cli-test-ps", "--init", "sleep 3600")
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	time.Sleep(2 * time.Second)

	stdout, _, code := c.run("ps", "cli-test-ps")
	if code != 0 {
		t.Fatalf("ps exit %d", code)
	}
	if !strings.Contains(stdout, "init") {
		t.Errorf("init session not in ps output: %s", stdout)
	}
	t.Logf("ps output:\n%s", stdout)
}

// --- Local user command tests (no daemon required) ---

func testLocalStore(t *testing.T) *store.Store {
	t.Helper()
	dir := t.TempDir()
	st, err := store.New(filepath.Join(dir, "test.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { st.Close() })
	return st
}

func TestGenerateAPIKey(t *testing.T) {
	key := generateAPIKey()
	if !strings.HasPrefix(key, "bht_") {
		t.Errorf("key should have bht_ prefix, got %q", key)
	}
	if len(key) != 4+64 { // "bht_" + 32 bytes hex
		t.Errorf("key length: %d, want %d", len(key), 68)
	}

	// Two keys should be different
	key2 := generateAPIKey()
	if key == key2 {
		t.Error("two generated keys should not be equal")
	}
}

func TestSha256HexCLI(t *testing.T) {
	hash := sha256HexCLI("test")
	if len(hash) != 64 {
		t.Errorf("hash length: %d, want 64", len(hash))
	}
	// Known SHA-256 of "test"
	if hash != "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08" {
		t.Errorf("unexpected hash: %s", hash)
	}
}

func TestUserCreateAndLookup(t *testing.T) {
	st := testLocalStore(t)

	key := generateAPIKey()
	hash := sha256HexCLI(key)

	err := st.CreateUser(store.User{
		ID: "usr_clitest", Name: "cli-user", APIKeyHash: hash,
		MaxSandboxes: 10, MaxCPUsPerSandbox: 2, MaxMemoryMBPerSandbox: 2048,
		SubnetIndex: 1, CreatedAt: time.Now(),
	})
	if err != nil {
		t.Fatal(err)
	}

	// Lookup by hash (simulates auth middleware)
	u, err := st.GetUserByKeyHash(hash)
	if err != nil {
		t.Fatal(err)
	}
	if u.Name != "cli-user" || u.MaxSandboxes != 10 {
		t.Fatalf("unexpected user: %+v", u)
	}

	// List
	users, _ := st.ListUsers()
	if len(users) != 1 {
		t.Fatalf("expected 1 user, got %d", len(users))
	}
}

func TestUserRotateKeyFlow(t *testing.T) {
	st := testLocalStore(t)

	oldKey := generateAPIKey()
	oldHash := sha256HexCLI(oldKey)

	st.CreateUser(store.User{
		ID: "usr_rot", Name: "rotate-test", APIKeyHash: oldHash,
		MaxSandboxes: 5, SubnetIndex: 1, CreatedAt: time.Now(),
	})

	// Rotate
	newKey := generateAPIKey()
	newHash := sha256HexCLI(newKey)
	if err := st.RotateUserKey("usr_rot", newHash); err != nil {
		t.Fatal(err)
	}

	// Old key doesn't work
	_, err := st.GetUserByKeyHash(oldHash)
	if err == nil {
		t.Fatal("old key should not work after rotation")
	}

	// New key works
	u, err := st.GetUserByKeyHash(newHash)
	if err != nil {
		t.Fatal(err)
	}
	if u.Name != "rotate-test" {
		t.Fatalf("unexpected user: %+v", u)
	}
}

func TestUserDeleteFlowWithProtection(t *testing.T) {
	st := testLocalStore(t)

	st.CreateUser(store.User{
		ID: "usr_delf", Name: "del-flow", APIKeyHash: "h1",
		MaxSandboxes: 5, SubnetIndex: 1, CreatedAt: time.Now(),
	})

	// Create a sandbox
	st.CreateSandbox(store.Sandbox{
		ID: "sb_delf", Name: "del-sb", Status: "running",
		CreatedBy: "usr_delf", CreatedAt: time.Now(),
	})

	// Delete should fail
	err := st.DeleteUser("usr_delf")
	if err == nil || !strings.Contains(err.Error(), "active sandbox") {
		t.Fatalf("expected deletion refused, got: %v", err)
	}

	// Destroy sandbox
	st.DeleteSandboxByID("sb_delf")

	// Now delete succeeds
	if err := st.DeleteUser("usr_delf"); err != nil {
		t.Fatal(err)
	}

	users, _ := st.ListUsers()
	if len(users) != 0 {
		t.Fatal("expected 0 users after delete")
	}
}

func TestCLISecretCRUD(t *testing.T) {
	c := setupCLITest(t)

	// Set
	stdout, _, code := c.run("secret", "set", "cli-test-key", "cli-test-val")
	if code != 0 {
		t.Fatalf("secret set exit %d", code)
	}
	if !strings.Contains(stdout, "ok") {
		t.Errorf("set output: %q", stdout)
	}

	// List
	stdout, _, code = c.run("secret", "list")
	if code != 0 {
		t.Fatalf("secret list exit %d", code)
	}
	if !strings.Contains(stdout, "cli-test-key") {
		t.Errorf("secret not in list: %s", stdout)
	}

	// Delete
	stdout, _, code = c.run("secret", "delete", "cli-test-key")
	if code != 0 {
		t.Fatalf("secret delete exit %d", code)
	}

	// Verify gone
	stdout, _, _ = c.run("secret", "list")
	if strings.Contains(stdout, "cli-test-key") {
		t.Error("secret still in list after delete")
	}
}

// ==========================================================================
// v0.3 CLI Tests — Persistent Volumes
// ==========================================================================

func TestCLIVolumeCRUD(t *testing.T) {
	c := setupCLITest(t)

	name := fmt.Sprintf("cli-vol-%d", time.Now().UnixNano()%100000)

	// Create
	stdout, stderr, code := c.run("volume", "create", "--name", name, "--size", "64")
	if code != 0 {
		t.Fatalf("volume create exit %d: %s", code, stderr)
	}
	if !strings.Contains(stdout, name) {
		t.Fatalf("volume create output missing name: %s", stdout)
	}
	t.Logf("✓ volume create: %s", strings.TrimSpace(stdout))

	// List
	stdout, _, code = c.run("volume", "list")
	if code != 0 {
		t.Fatalf("volume list exit %d", code)
	}
	if !strings.Contains(stdout, name) {
		t.Fatalf("volume not in list: %s", stdout)
	}
	t.Log("✓ volume list shows created volume")

	// Delete
	stdout, _, code = c.run("volume", "delete", name)
	if code != 0 {
		t.Fatalf("volume delete exit %d", code)
	}
	t.Log("✓ volume deleted")

	// Verify gone
	stdout, _, _ = c.run("volume", "list")
	if strings.Contains(stdout, name) {
		t.Error("volume still in list after delete")
	}
	t.Log("✓ volume absent from list after delete")
}

func TestCLIVolumeResize(t *testing.T) {
	c := setupCLITest(t)

	name := fmt.Sprintf("cli-resize-%d", time.Now().UnixNano()%100000)

	c.run("volume", "create", "--name", name, "--size", "64")
	t.Cleanup(func() { c.run("volume", "delete", name) })

	// Resize up
	stdout, stderr, code := c.run("volume", "resize", name, "--size", "128")
	if code != 0 {
		t.Fatalf("volume resize exit %d: %s", code, stderr)
	}
	if !strings.Contains(stdout, "128") && !strings.Contains(stdout, "resized") {
		t.Fatalf("resize output: %s", stdout)
	}
	t.Log("✓ volume resized to 128MB")

	// Resize down should fail
	_, stderr, code = c.run("volume", "resize", name, "--size", "32")
	if code == 0 {
		t.Fatal("resize shrink should fail")
	}
	t.Logf("✓ volume resize shrink rejected: %s", strings.TrimSpace(stderr))
}

func TestCLIVolumeAttachToSandbox(t *testing.T) {
	c := setupCLITest(t)

	volName := fmt.Sprintf("cli-attach-%d", time.Now().UnixNano()%100000)
	sbName := fmt.Sprintf("cli-vol-sb-%d", time.Now().UnixNano()%100000)

	// Create volume
	c.run("volume", "create", "--name", volName, "--size", "64")
	t.Cleanup(func() { c.run("volume", "delete", volName) })

	// Create sandbox with volume attached
	stdout, stderr, code := c.run("create", "--name", sbName,
		"--volume", volName+":/workspace")
	if code != 0 {
		t.Fatalf("create with volume exit %d: %s", code, stderr)
	}
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })
	t.Log("✓ sandbox created with volume attached")

	// Write data to volume
	stdout, stderr, code = c.run("exec", sbName, "--", "sh", "-c",
		"echo cli-vol-data > /workspace/test.txt && cat /workspace/test.txt")
	if code != 0 {
		t.Fatalf("exec write exit %d: %s", code, stderr)
	}
	if !strings.Contains(stdout, "cli-vol-data") {
		t.Fatalf("write verification failed: %s", stdout)
	}
	t.Log("✓ wrote data to volume via exec")

	// Destroy sandbox
	c.run("destroy", sbID)
	t.Log("✓ sandbox destroyed, volume should survive")

	// Create new sandbox with same volume
	sbName2 := sbName + "-2"
	stdout, stderr, code = c.run("create", "--name", sbName2,
		"--volume", volName+":/workspace")
	if code != 0 {
		t.Fatalf("create sb2 exit %d: %s", code, stderr)
	}
	sbID2 := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID2) })

	// Read data back
	stdout, _, code = c.run("exec", sbName2, "--", "cat", "/workspace/test.txt")
	if code != 0 || !strings.Contains(stdout, "cli-vol-data") {
		t.Fatalf("data did not persist: exit=%d stdout=%q", code, stdout)
	}
	t.Log("✓ data persists across sandbox destroy/recreate via CLI")
}

func TestCLIVolumeReadOnly(t *testing.T) {
	c := setupCLITest(t)

	volName := fmt.Sprintf("cli-ro-%d", time.Now().UnixNano()%100000)
	sbName := fmt.Sprintf("cli-ro-sb-%d", time.Now().UnixNano()%100000)

	// Create and seed volume with data
	c.run("volume", "create", "--name", volName, "--size", "64")
	t.Cleanup(func() { c.run("volume", "delete", volName) })

	// Seed with data (RW mount)
	stdout, stderr, code := c.run("create", "--name", sbName, "--volume", volName+":/data")
	if code != 0 {
		t.Fatalf("create RW exit %d: %s", code, stderr)
	}
	sbID := strings.Fields(stdout)[0]
	c.run("exec", sbName, "--", "sh", "-c", "echo ro-test-data > /data/file.txt")
	c.run("destroy", sbID)

	// Now mount read-only
	sbName2 := sbName + "-ro"
	stdout, stderr, code = c.run("create", "--name", sbName2, "--volume", volName+":/data:ro")
	if code != 0 {
		t.Fatalf("create RO exit %d: %s", code, stderr)
	}
	sbID2 := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID2) })

	// Data should be readable
	stdout, _, code = c.run("exec", sbName2, "--", "cat", "/data/file.txt")
	if !strings.Contains(stdout, "ro-test-data") {
		t.Fatalf("expected data readable in RO: %q", stdout)
	}
	t.Log("✓ data readable through RO volume")

	// Write should fail
	stdout, _, code = c.run("exec", sbName2, "--", "sh", "-c", "touch /data/nope 2>&1; echo exit=$?")
	if !strings.Contains(stdout, "Read-only") {
		t.Fatalf("expected Read-only error: %q", stdout)
	}
	t.Log("✓ write rejected on RO volume")
}

func TestCLIVolumeDeleteWhileAttached(t *testing.T) {
	c := setupCLITest(t)

	volName := fmt.Sprintf("cli-del-att-%d", time.Now().UnixNano()%100000)
	sbName := fmt.Sprintf("cli-del-sb-%d", time.Now().UnixNano()%100000)

	c.run("volume", "create", "--name", volName, "--size", "64")
	stdout, _, _ := c.run("create", "--name", sbName, "--volume", volName+":/workspace")
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() {
		c.run("destroy", sbID)
		c.run("volume", "delete", volName)
	})

	// Delete should fail while attached
	_, stderr, code := c.run("volume", "delete", volName)
	if code == 0 {
		t.Fatal("volume delete should fail while attached")
	}
	if !strings.Contains(stderr, "attachment") {
		t.Fatalf("expected attachment error: %s", stderr)
	}
	t.Log("✓ volume delete blocked while attached")
}

// ==========================================================================
// v0.3 CLI Tests — Images
// ==========================================================================

func TestCLIImagePullAndBoot(t *testing.T) {
	c := setupCLITest(t)

	imgName := fmt.Sprintf("cli-alpine-%d", time.Now().UnixNano()%100000)
	sbName := fmt.Sprintf("cli-img-sb-%d", time.Now().UnixNano()%100000)

	// Pull alpine (async — returns task ID)
	stdout, stderr, code := c.run("image", "pull", "alpine:latest", "--name", imgName)
	if code != 0 {
		t.Fatalf("image pull exit %d: %s\nstdout: %s", code, stderr, stdout)
	}
	t.Logf("pull output: %s", strings.TrimSpace(stdout))

	// The CLI should poll until completion. Verify success message.
	if !strings.Contains(stdout, imgName) && !strings.Contains(stdout, "pulled") &&
		!strings.Contains(stdout, "completed") {
		// The pull may still be async — check image list
		t.Log("pull may be async, waiting for image to appear...")
		for i := 0; i < 60; i++ {
			time.Sleep(2 * time.Second)
			listOut, _, _ := c.run("image", "list")
			if strings.Contains(listOut, imgName) {
				t.Log("✓ image appeared in list")
				goto pullDone
			}
		}
		t.Fatal("image did not appear in list after 120s")
	}
pullDone:
	t.Cleanup(func() { c.run("image", "delete", imgName) })

	// List should show the image
	stdout, _, code = c.run("image", "list")
	if code != 0 {
		t.Fatalf("image list exit %d", code)
	}
	if !strings.Contains(stdout, imgName) {
		t.Fatalf("image not in list: %s", stdout)
	}
	t.Log("✓ image in list")

	// Boot from the pulled image
	stdout, stderr, code = c.run("create", "--name", sbName, "--image", imgName)
	if code != 0 {
		t.Fatalf("create with image exit %d: %s", code, stderr)
	}
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	// Verify it's Alpine
	stdout, _, code = c.run("exec", sbName, "--", "cat", "/etc/os-release")
	if code != 0 || !strings.Contains(stdout, "Alpine") {
		t.Fatalf("expected Alpine: %s", stdout)
	}
	t.Log("✓ booted from pulled OCI image via CLI")
}

func TestCLIImageSaveAndBoot(t *testing.T) {
	c := setupCLITest(t)

	srcName := fmt.Sprintf("cli-save-src-%d", time.Now().UnixNano()%100000)
	imgName := fmt.Sprintf("cli-saved-%d", time.Now().UnixNano()%100000)
	dstName := fmt.Sprintf("cli-save-dst-%d", time.Now().UnixNano()%100000)

	// Create source sandbox and write a marker
	stdout, stderr, code := c.run("create", "--name", srcName)
	if code != 0 {
		t.Fatalf("create src exit %d: %s", code, stderr)
	}
	srcID := strings.Fields(stdout)[0]
	c.run("exec", srcName, "--", "sh", "-c", "echo cli-saved-marker > /home/lohar/marker.txt")

	// Save image
	stdout, stderr, code = c.run("image", "save", srcName, "--name", imgName)
	if code != 0 {
		c.run("destroy", srcID)
		t.Fatalf("image save exit %d: %s", code, stderr)
	}
	c.run("destroy", srcID)
	t.Cleanup(func() { c.run("image", "delete", imgName) })
	t.Log("✓ image saved from running sandbox")

	// Boot from saved image
	stdout, stderr, code = c.run("create", "--name", dstName, "--image", imgName)
	if code != 0 {
		t.Fatalf("create from saved exit %d: %s", code, stderr)
	}
	dstID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", dstID) })

	// Verify marker
	stdout, _, code = c.run("exec", dstName, "--", "cat", "/home/lohar/marker.txt")
	if code != 0 || !strings.Contains(stdout, "cli-saved-marker") {
		t.Fatalf("marker not found: %s", stdout)
	}
	t.Log("✓ booted from saved image, marker present")
}

func TestCLIImageDeleteNonexistent(t *testing.T) {
	c := setupCLITest(t)

	_, _, code := c.run("image", "delete", "nonexistent-image-xyz")
	if code == 0 {
		t.Fatal("delete nonexistent image should fail")
	}
	t.Log("✓ delete nonexistent image rejected")
}

// ==========================================================================
// v0.3 CLI Tests — Named Snapshots (Checkpoint/Resume)
// ==========================================================================

func TestCLISnapshotCheckpointAndResume(t *testing.T) {
	c := setupCLITest(t)

	sbName := fmt.Sprintf("cli-snap-src-%d", time.Now().UnixNano()%100000)
	snapName := fmt.Sprintf("cli-ckpt-%d", time.Now().UnixNano()%100000)
	resumeName := fmt.Sprintf("cli-resumed-%d", time.Now().UnixNano()%100000)

	// Create sandbox and write data
	stdout, stderr, code := c.run("create", "--name", sbName)
	if code != 0 {
		t.Fatalf("create exit %d: %s", code, stderr)
	}
	sbID := strings.Fields(stdout)[0]
	c.run("exec", sbName, "--", "sh", "-c", "echo snap-marker > /home/lohar/data.txt")

	// Checkpoint
	stdout, stderr, code = c.run("snapshot", "create", sbName, "--name", snapName)
	if code != 0 {
		c.run("destroy", sbID)
		t.Fatalf("snapshot create exit %d: %s\nstdout: %s", code, stderr, stdout)
	}
	t.Cleanup(func() { c.run("snapshot", "delete", snapName) })
	t.Log("✓ snapshot created")

	// Verify source VM still runs after checkpoint
	stdout, _, code = c.run("exec", sbName, "--", "echo", "still-alive")
	if code != 0 || !strings.Contains(stdout, "still-alive") {
		t.Fatalf("VM should be running after checkpoint: exit=%d out=%q", code, stdout)
	}
	t.Log("✓ source VM still running after checkpoint")

	// Destroy source
	c.run("destroy", sbID)

	// List snapshots
	stdout, _, code = c.run("snapshot", "list")
	if code != 0 {
		t.Fatalf("snapshot list exit %d", code)
	}
	if !strings.Contains(stdout, snapName) {
		t.Fatalf("snapshot not in list: %s", stdout)
	}
	t.Log("✓ snapshot in list")

	// Resume
	stdout, stderr, code = c.run("snapshot", "resume", snapName, "--name", resumeName)
	if code != 0 {
		t.Fatalf("snapshot resume exit %d: %s\nstdout: %s", code, stderr, stdout)
	}
	resumeID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", resumeID) })
	t.Log("✓ snapshot resumed")

	// Verify data restored
	stdout, _, code = c.run("exec", resumeName, "--", "cat", "/home/lohar/data.txt")
	if code != 0 || !strings.Contains(stdout, "snap-marker") {
		t.Fatalf("data not restored: exit=%d out=%q", code, stdout)
	}
	t.Log("✓ data restored from snapshot via CLI")
}

func TestCLISnapshotDeleteNonexistent(t *testing.T) {
	c := setupCLITest(t)

	_, _, code := c.run("snapshot", "delete", "nonexistent-snap-xyz")
	if code == 0 {
		t.Fatal("delete nonexistent snapshot should fail")
	}
	t.Log("✓ delete nonexistent snapshot rejected")
}

// ==========================================================================
// v0.3 CLI Tests — Disk Resize
// ==========================================================================

func TestCLIDiskResize(t *testing.T) {
	c := setupCLITest(t)

	sbName := fmt.Sprintf("cli-disk-%d", time.Now().UnixNano()%100000)

	stdout, stderr, code := c.run("create", "--name", sbName, "--disk-size", "4096")
	if code != 0 {
		t.Fatalf("create with disk-size exit %d: %s", code, stderr)
	}
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	stdout, _, code = c.run("exec", sbName, "--", "df", "-m", "/")
	if code != 0 {
		t.Fatalf("df exit %d", code)
	}
	lines := strings.Split(stdout, "\n")
	if len(lines) < 2 {
		t.Fatalf("unexpected df output: %q", stdout)
	}
	fields := strings.Fields(lines[1])
	if len(fields) < 2 {
		t.Fatalf("unexpected df fields: %q", lines[1])
	}
	var sizeMB int
	fmt.Sscanf(fields[1], "%d", &sizeMB)
	if sizeMB < 3500 {
		t.Fatalf("expected ~4096MB rootfs, got %dMB", sizeMB)
	}
	t.Logf("✓ rootfs resized to %dMB via --disk-size flag", sizeMB)
}

// ==========================================================================
// v0.3 CLI Tests — Init Script
// ==========================================================================

func TestCLIInitScript(t *testing.T) {
	c := setupCLITest(t)

	sbName := fmt.Sprintf("cli-init-%d", time.Now().UnixNano()%100000)

	stdout, stderr, code := c.run("create", "--name", sbName,
		"--init", "echo init-ran > /tmp/init-marker")
	if code != 0 {
		t.Fatalf("create with init exit %d: %s", code, stderr)
	}
	sbID := strings.Fields(stdout)[0]
	t.Cleanup(func() { c.run("destroy", sbID) })

	// Wait for init to run
	time.Sleep(3 * time.Second)

	stdout, _, code = c.run("exec", sbName, "--", "cat", "/tmp/init-marker")
	if code != 0 || !strings.Contains(stdout, "init-ran") {
		t.Fatalf("init marker not found: exit=%d out=%q", code, stdout)
	}
	t.Log("✓ init script ran successfully")
}

// ==========================================================================
// v0.3 CLI Tests — JSON Output
// ==========================================================================

func TestCLIJSONOutput(t *testing.T) {
	c := setupCLITest(t)

	sbName := fmt.Sprintf("cli-json-%d", time.Now().UnixNano()%100000)

	// Create with --json
	stdout, stderr, code := c.run("--json", "create", "--name", sbName)
	if code != 0 {
		t.Fatalf("create --json exit %d: %s", code, stderr)
	}

	var sb map[string]interface{}
	if err := json.Unmarshal([]byte(stdout), &sb); err != nil {
		t.Fatalf("json parse create: %v\nraw: %s", err, stdout)
	}
	sbID, ok := sb["id"].(string)
	if !ok || sbID == "" {
		t.Fatalf("missing id in json: %v", sb)
	}
	t.Cleanup(func() { c.run("destroy", sbID) })
	t.Log("✓ create --json returns valid JSON with id")

	// List with --json
	stdout, _, code = c.run("--json", "list")
	if code != 0 {
		t.Fatalf("list --json exit %d", code)
	}
	var list []map[string]interface{}
	if err := json.Unmarshal([]byte(stdout), &list); err != nil {
		t.Fatalf("json parse list: %v\nraw: %s", err, stdout)
	}
	found := false
	for _, s := range list {
		if s["name"] == sbName {
			found = true
			break
		}
	}
	if !found {
		t.Fatalf("sandbox not in json list")
	}
	t.Log("✓ list --json returns valid JSON array with sandbox")

	// Volume list with --json
	stdout, _, code = c.run("--json", "volume", "list")
	if code != 0 {
		t.Fatalf("volume list --json exit %d", code)
	}
	var vols []interface{}
	if err := json.Unmarshal([]byte(stdout), &vols); err != nil {
		t.Fatalf("json parse volume list: %v\nraw: %s", err, stdout)
	}
	t.Log("✓ volume list --json returns valid JSON")
}

// ==========================================================================
// v0.3 CLI Tests — Error Cases
// ==========================================================================

func TestCLICreateNonexistentImage(t *testing.T) {
	c := setupCLITest(t)

	_, stderr, code := c.run("create", "--name", "cli-bad-img", "--image", "nonexistent-image-xyz")
	if code == 0 {
		t.Cleanup(func() { c.run("destroy", "cli-bad-img") })
		t.Fatal("create with nonexistent image should fail")
	}
	if !strings.Contains(stderr, "not found") {
		t.Fatalf("expected 'not found' error: %s", stderr)
	}
	t.Log("✓ create with nonexistent image rejected")
}

func TestCLICreateNonexistentVolume(t *testing.T) {
	c := setupCLITest(t)

	_, stderr, code := c.run("create", "--name", "cli-bad-vol", "--volume", "nonexistent-vol-xyz:/data")
	if code == 0 {
		t.Cleanup(func() { c.run("destroy", "cli-bad-vol") })
		t.Fatal("create with nonexistent volume should fail")
	}
	if !strings.Contains(stderr, "not found") {
		t.Fatalf("expected 'not found' error: %s", stderr)
	}
	t.Log("✓ create with nonexistent volume rejected")
}

func TestCLISandboxNameValidation(t *testing.T) {
	c := setupCLITest(t)

	badNames := []string{"../etc/passwd", "a/b", "", "a b"}
	for _, name := range badNames {
		if name == "" {
			continue // empty name gets auto-generated
		}
		_, _, code := c.run("create", "--name", name)
		if code == 0 {
			c.run("destroy", name)
			t.Fatalf("name %q should be rejected", name)
		}
	}
	t.Log("✓ invalid sandbox names rejected")
}

func TestCLIVolumeNameValidation(t *testing.T) {
	c := setupCLITest(t)

	_, _, code := c.run("volume", "create", "--name", "../escape", "--size", "64")
	if code == 0 {
		c.run("volume", "delete", "../escape")
		t.Fatal("volume name with path traversal should be rejected")
	}
	t.Log("✓ invalid volume name rejected")
}

func TestCLIExecNonexistentSandbox(t *testing.T) {
	c := setupCLITest(t)

	_, _, code := c.run("exec", "nonexistent-sandbox-xyz", "--", "echo", "hi")
	if code == 0 {
		t.Fatal("exec on nonexistent sandbox should fail")
	}
	t.Log("✓ exec on nonexistent sandbox rejected")
}

func TestCLIDestroyNonexistentSandbox(t *testing.T) {
	c := setupCLITest(t)

	_, _, code := c.run("destroy", "nonexistent-sandbox-xyz")
	if code == 0 {
		t.Fatal("destroy nonexistent sandbox should fail")
	}
	t.Log("✓ destroy nonexistent sandbox rejected")
}

// ==========================================================================
// v0.3 CLI Tests — Version
// ==========================================================================

func TestCLIVersion(t *testing.T) {
	c := setupCLITest(t)

	stdout, _, code := c.run("version")
	if code != 0 {
		t.Fatalf("version exit %d", code)
	}
	if !strings.Contains(stdout, "bhatti") {
		t.Fatalf("version output: %s", stdout)
	}
	if !strings.Contains(stdout, "api:") {
		t.Fatalf("version should show api endpoint: %s", stdout)
	}
	t.Logf("✓ version: %s", strings.TrimSpace(stdout))
}

// TestCompareVersions tests the semver comparison used for CLI update checks.
// Does not require a running server.
func TestCompareVersions(t *testing.T) {
	tests := []struct {
		a, b string
		want int
	}{
		{"0.4.0", "0.4.0", 0},
		{"0.3.9", "0.4.0", -1},
		{"0.4.1", "0.4.0", 1},
		{"v0.4.0", "0.4.0", 0},
		{"0.4.0", "v0.4.0", 0},
		{"1.0.0", "0.99.99", 1},
		{"0.3.0", "0.4.0", -1},
		{"0.4.0", "0.4.1", -1},
		{"v1.2.3", "v1.2.4", -1},
		{"0.4", "0.4.0", 0},   // missing patch = 0
		{"0.4", "0.4.1", -1},
	}
	for _, tt := range tests {
		got := compareVersions(tt.a, tt.b)
		if got != tt.want {
			t.Errorf("compareVersions(%q, %q) = %d, want %d", tt.a, tt.b, got, tt.want)
		}
	}
}


