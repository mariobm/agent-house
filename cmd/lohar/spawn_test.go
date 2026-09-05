//go:build linux

package main

import (
	"bytes"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

// --- Pure tests of parseSpawnArgs ---

func TestParseSpawnArgs(t *testing.T) {
	cases := []struct {
		name     string
		args     []string
		wantCG   string
		wantArgv []string
		wantErr  string // substring; "" means no error
	}{
		{
			name:     "happy path: separate flag",
			args:     []string{"--cgroup", "/sys/fs/cgroup/system.slice/x.service", "--", "/bin/echo", "hi"},
			wantCG:   "/sys/fs/cgroup/system.slice/x.service",
			wantArgv: []string{"/bin/echo", "hi"},
		},
		{
			name:     "happy path: --cgroup=<value>",
			args:     []string{"--cgroup=/cg", "--", "/bin/sh", "-c", "exec /usr/bin/Xkasmvnc"},
			wantCG:   "/cg",
			wantArgv: []string{"/bin/sh", "-c", "exec /usr/bin/Xkasmvnc"},
		},
		{
			name:     "daemon argv starts with a flag — disambiguated by --",
			args:     []string{"--cgroup", "/cg", "--", "/usr/bin/Xkasmvnc", "-ac", "-dontvtswitch"},
			wantCG:   "/cg",
			wantArgv: []string{"/usr/bin/Xkasmvnc", "-ac", "-dontvtswitch"},
		},
		{
			name:    "missing -- separator",
			args:    []string{"--cgroup", "/cg", "/bin/echo", "hi"},
			wantErr: "unexpected argument",
		},
		{
			name:    "no --cgroup flag",
			args:    []string{"--", "/bin/echo", "hi"},
			wantErr: "--cgroup is required",
		},
		{
			name:    "empty argv after --",
			args:    []string{"--cgroup", "/cg", "--"},
			wantErr: "empty argv after --",
		},
		{
			name:    "--cgroup with no value at end",
			args:    []string{"--cgroup"},
			wantErr: "--cgroup requires a value",
		},
		{
			name:    "--cgroup= with empty value",
			args:    []string{"--cgroup=", "--", "/bin/echo"},
			wantErr: "--cgroup requires a value",
		},
		{
			name:    "no -- at all",
			args:    []string{"--cgroup", "/cg"},
			wantErr: "missing '--'",
		},
		{
			name:    "completely empty",
			args:    []string{},
			wantErr: "missing '--'",
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			cg, argv, err := parseSpawnArgs(c.args)
			if c.wantErr != "" {
				if err == nil {
					t.Fatalf("expected error containing %q, got nil (cg=%q argv=%v)", c.wantErr, cg, argv)
				}
				if !strings.Contains(err.Error(), c.wantErr) {
					t.Errorf("error %q does not contain %q", err, c.wantErr)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if cg != c.wantCG {
				t.Errorf("cgroup path: got %q, want %q", cg, c.wantCG)
			}
			if !slicesEqual(argv, c.wantArgv) {
				t.Errorf("argv: got %v, want %v", argv, c.wantArgv)
			}
		})
	}
}

func slicesEqual(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// --- Runtime tests via test-binary re-exec ---
//
// runSpawn is hard to test in-process because it calls os.Exit on
// failure and syscall.Exec on success. Either branch tears down the
// test process. The dispatch lives in TestMain (cmd/lohar/main_test.go)
// which routes the subprocess to runSpawn whenever LOHAR_SPAWN_HELPER=1
// is set in the environment. We just need to invoke ourselves with that
// env var and the helper-args sentinel.

// helperSpawn re-execs the test binary so that runSpawn runs in a
// fresh process. Returns stdout, stderr, and the exit code observed by
// the parent. The TestMain dispatch (see main_test.go) handles routing
// args after "--helper-args" to runSpawn.
func helperSpawn(t *testing.T, args ...string) (stdout, stderr string, exitCode int) {
	t.Helper()
	allArgs := []string{"--helper-args"}
	allArgs = append(allArgs, args...)
	cmd := exec.Command(os.Args[0], allArgs...)
	cmd.Env = append(os.Environ(), "LOHAR_SPAWN_HELPER=1")
	var outBuf, errBuf bytes.Buffer
	cmd.Stdout = &outBuf
	cmd.Stderr = &errBuf
	err := cmd.Run()
	stdout, stderr = outBuf.String(), errBuf.String()
	if err == nil {
		return stdout, stderr, 0
	}
	if ee, ok := err.(*exec.ExitError); ok {
		return stdout, stderr, ee.ExitCode()
	}
	t.Fatalf("run helper: %v", err)
	return
}

func TestSpawn_HappyPath(t *testing.T) {
	// A tempdir stands in for the unit's cgroup. spawn writes
	// <dir>/cgroup.procs as a regular file; the kernel never sees it.
	// We're verifying argv parse + write + exec end-to-end, not the
	// kernel side (that's TestStartDaemonPlacesProcessInUnitCgroup in
	// cgroup_test.go).
	dir := t.TempDir()

	stdout, stderr, code := helperSpawn(t, "--cgroup", dir, "--", "/bin/echo", "hello-from-spawn")

	if code != 0 {
		t.Errorf("exit code: %d, want 0; stderr: %s", code, stderr)
	}
	if strings.TrimSpace(stdout) != "hello-from-spawn" {
		t.Errorf("stdout: %q, want %q", stdout, "hello-from-spawn")
	}

	// cgroup.procs should contain the helper subprocess's PID (a
	// positive integer; we don't know the exact value but it must
	// have been written before the execve into /bin/echo).
	procs := filepath.Join(dir, "cgroup.procs")
	data, err := os.ReadFile(procs)
	if err != nil {
		t.Fatalf("read cgroup.procs: %v", err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(data)))
	if err != nil {
		t.Fatalf("cgroup.procs not a number: %q", data)
	}
	if pid < 1 {
		t.Errorf("cgroup.procs = %d, want positive PID", pid)
	}
}

func TestSpawn_HappyPath_ShWrapper(t *testing.T) {
	// Mirrors how the supervisor actually invokes spawn:
	//   /proc/self/exe spawn --cgroup <path> -- /bin/sh -c "exec <execStart>"
	// Verifies the two-execve chain (spawn → sh → daemon) works.
	dir := t.TempDir()

	stdout, _, code := helperSpawn(t, "--cgroup", dir, "--", "/bin/sh", "-c", "exec /bin/echo via-sh")

	if code != 0 {
		t.Fatalf("exit code: %d", code)
	}
	if strings.TrimSpace(stdout) != "via-sh" {
		t.Errorf("stdout: %q, want via-sh", stdout)
	}
}

func TestSpawn_CgroupWriteFailure(t *testing.T) {
	// /nonexistent/cgroup directory: os.WriteFile returns ENOENT
	// because the parent dir doesn't exist.
	_, stderr, code := helperSpawn(t, "--cgroup", "/nonexistent/cgroup/dir", "--", "/bin/echo", "should-not-run")

	if code != 1 {
		t.Errorf("exit code: %d, want 1", code)
	}
	if !strings.Contains(stderr, "cgroup.procs") {
		t.Errorf("stderr should mention cgroup.procs, got: %q", stderr)
	}
}

func TestSpawn_MissingDashDash(t *testing.T) {
	dir := t.TempDir()
	_, stderr, code := helperSpawn(t, "--cgroup", dir, "/bin/echo", "hi")

	if code != 2 {
		t.Errorf("exit code: %d, want 2", code)
	}
	if !strings.Contains(stderr, "usage:") {
		t.Errorf("stderr should contain usage line, got: %q", stderr)
	}
}

func TestSpawn_EmptyArgvAfterDashDash(t *testing.T) {
	dir := t.TempDir()
	_, stderr, code := helperSpawn(t, "--cgroup", dir, "--")

	if code != 2 {
		t.Errorf("exit code: %d, want 2", code)
	}
	if !strings.Contains(stderr, "empty argv") {
		t.Errorf("stderr should mention empty argv, got: %q", stderr)
	}
}

func TestSpawn_NoCgroupFlag(t *testing.T) {
	_, stderr, code := helperSpawn(t, "--", "/bin/echo", "hi")

	if code != 2 {
		t.Errorf("exit code: %d, want 2", code)
	}
	if !strings.Contains(stderr, "--cgroup is required") {
		t.Errorf("stderr should mention --cgroup is required, got: %q", stderr)
	}
}

func TestSpawn_ExecFailureBadBinary(t *testing.T) {
	// Cgroup write succeeds (tempdir), but the daemon binary doesn't
	// exist. syscall.Exec returns ENOENT; spawn prints and exits 1.
	dir := t.TempDir()
	_, stderr, code := helperSpawn(t, "--cgroup", dir, "--", "/nonexistent/binary")

	if code != 1 {
		t.Errorf("exit code: %d, want 1", code)
	}
	if !strings.Contains(stderr, "exec") {
		t.Errorf("stderr should mention exec failure, got: %q", stderr)
	}
	if !strings.Contains(stderr, "nonexistent") {
		t.Errorf("stderr should name the bad binary, got: %q", stderr)
	}
}
