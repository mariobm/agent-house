//go:build linux

package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// failedStateTestSetup sandboxes serviceDirs and pidDir into tempdirs so
// the .failed marker can be written and read without root or clobbering
// the host's /run/bhatti.
func failedStateTestSetup(t *testing.T) (svcDir string) {
	t.Helper()
	svcDir = t.TempDir()
	origDirs := serviceDirs
	origPid := pidDir
	origLog := logDir
	serviceDirs = []string{svcDir}
	pidDir = t.TempDir()
	logDir = t.TempDir()
	t.Cleanup(func() {
		serviceDirs = origDirs
		pidDir = origPid
		logDir = origLog
	})
	return svcDir
}

func TestShouldRestart(t *testing.T) {
	// Maps Restart= directive + exit code to a yes/no decision. Mirrors
	// systemd's policy table.
	cases := []struct {
		policy  string
		exit    int
		wantRun bool
	}{
		{"no", 0, false},
		{"no", 1, false},
		{"", 0, false}, // unset = explicit opt-in
		{"", 1, false},
		{"always", 0, true},
		{"always", 1, true},
		{"on-success", 0, true},
		{"on-success", 1, false},
		{"on-failure", 0, false},
		{"on-failure", 1, true},
		{"on-abnormal", 1, false},     // not signal-killed
		{"on-abnormal", 130, true},    // exit > 128 -> killed by signal
		{"on-abnormal", 137, true},    // SIGKILL = 128+9
	}
	for _, c := range cases {
		got := shouldRestart(c.policy, c.exit)
		if got != c.wantRun {
			t.Errorf("shouldRestart(%q, exit=%d) = %v, want %v", c.policy, c.exit, got, c.wantRun)
		}
	}
}

func TestParseRestartSec(t *testing.T) {
	cases := []struct {
		in   string
		want time.Duration
	}{
		{"", 100 * time.Millisecond},      // default matches systemd
		{"5", 5 * time.Second},            // bare integer = seconds
		{"500ms", 500 * time.Millisecond}, // Go duration form
		{"2s", 2 * time.Second},
		{"garbage", 100 * time.Millisecond}, // fallback on unparseable
	}
	for _, c := range cases {
		if got := parseRestartSec(c.in); got != c.want {
			t.Errorf("parseRestartSec(%q) = %v, want %v", c.in, got, c.want)
		}
	}
}

func TestFailedMarker(t *testing.T) {
	// MarkFailed writes the exit code; IsFailed reads it; ClearFailed
	// removes it. The whole machinery underlying systemctl is-failed.
	dir := failedStateTestSetup(t)
	os.WriteFile(filepath.Join(dir, "test.service"),
		[]byte("[Service]\nExecStart=/bin/true\n"), 0644)

	reg := NewRegistry()
	u, _ := reg.Resolve("test")

	if u.IsFailed() {
		t.Fatal("fresh unit should not be failed")
	}
	u.MarkFailed(42)
	if !u.IsFailed() {
		t.Error("after MarkFailed, IsFailed should be true")
	}
	if got := u.LastExitCode(); got != 42 {
		t.Errorf("LastExitCode = %d, want 42", got)
	}
	u.ClearFailed()
	if u.IsFailed() {
		t.Error("after ClearFailed, IsFailed should be false")
	}
}

func TestRestartOnFailure(t *testing.T) {
	// Real lifecycle test: a service with Restart=on-failure that exits
	// non-zero should be auto-restarted by the watcher. Use a unit whose
	// ExecStart fails fast; assert the failed marker eventually appears
	// and the watcher tried to restart (we observe restart attempts via
	// the burst-history map).
	//
	// We use Restart=on-failure with /bin/false, which exits 1 immediately.
	// StartLimitBurst=2 so the watcher gives up after 2 attempts and the
	// test doesn't loop forever.
	dir := failedStateTestSetup(t)
	os.WriteFile(filepath.Join(dir, "crasher.service"), []byte(`
[Service]
Type=simple
ExecStart=/bin/false
Restart=on-failure
RestartSec=10ms
StartLimitBurst=2
StartLimitIntervalSec=10
`), 0644)

	reg := NewRegistry()
	u, _ := reg.Resolve("crasher")

	// Start the service. The watcher will see /bin/false exit with code
	// 1, mark failed, and try to restart per Restart=on-failure.
	if err := svcStart(u); err != nil {
		t.Fatalf("svcStart: %v", err)
	}

	// Wait for the burst limit to be hit. With RestartSec=10ms and
	// burst=2, we need ~20ms of attempts plus the final mark-failed.
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		restartBurst.Lock()
		count := len(restartBurst.history[u.Canonical])
		restartBurst.Unlock()
		if count >= 2 {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}

	// Give the final attempt time to land.
	time.Sleep(50 * time.Millisecond)

	if !u.IsFailed() {
		t.Error("crasher should be marked failed after the restart loop gives up")
	}
	if u.LastExitCode() != 1 {
		t.Errorf("LastExitCode = %d, want 1 (/bin/false's exit code)", u.LastExitCode())
	}

	// Reset state for any subsequent tests in the same process.
	restartBurst.Lock()
	delete(restartBurst.history, u.Canonical)
	restartBurst.Unlock()
}

func TestRestartNoSuppressesAutoRestart(t *testing.T) {
	// Restart=no (the default) means a crashing service stays dead.
	// The watcher writes the failed marker but doesn't loop.
	dir := failedStateTestSetup(t)
	os.WriteFile(filepath.Join(dir, "diehard.service"),
		[]byte("[Service]\nType=simple\nExecStart=/bin/false\nRestart=no\n"), 0644)

	reg := NewRegistry()
	u, _ := reg.Resolve("diehard")

	if err := svcStart(u); err != nil {
		t.Fatalf("svcStart: %v", err)
	}

	// Give the watcher time to observe the exit and mark failed.
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if u.IsFailed() {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}

	if !u.IsFailed() {
		t.Fatal("diehard.service should be marked failed after exit")
	}

	// And the burst history should NOT have entries (no restart was attempted).
	restartBurst.Lock()
	count := len(restartBurst.history[u.Canonical])
	restartBurst.Unlock()
	if count != 0 {
		t.Errorf("burst history = %d, want 0 (Restart=no should not attempt restart)", count)
	}
}

func TestStopSuppressesRestart(t *testing.T) {
	// Even with Restart=always, an explicit svcStop must NOT trigger a
	// restart \u2014 the admin asked for the service to stop. The watcher
	// reads the stopRequested flag and bails out.
	dir := failedStateTestSetup(t)
	os.WriteFile(filepath.Join(dir, "loopy.service"),
		[]byte("[Service]\nType=simple\nExecStart=/bin/sleep 30\nRestart=always\nRestartSec=10ms\n"), 0644)

	reg := NewRegistry()
	u, _ := reg.Resolve("loopy")

	if err := svcStart(u); err != nil {
		t.Fatalf("svcStart: %v", err)
	}
	// Process is sleeping; svcStop should kill it AND prevent the restart.
	if err := svcStop(u); err != nil {
		t.Fatalf("svcStop: %v", err)
	}

	// Give the watcher time to react (it reads stopRequested after Wait).
	time.Sleep(200 * time.Millisecond)

	// No new pidfile (no restart).
	if _, err := os.Stat(u.PidPath()); !os.IsNotExist(err) {
		t.Errorf("pidfile reappeared after stop; restart was not suppressed")
	}
	// Not failed (clean stop, not a crash).
	if u.IsFailed() {
		t.Error("svcStop after a Restart=always service should not leave the failed marker")
	}
}

func TestResetFailed(t *testing.T) {
	// `systemctl reset-failed` clears the .failed marker. Tested via the
	// dispatch path the IPC server uses.
	dir := failedStateTestSetup(t)
	os.WriteFile(filepath.Join(dir, "errored.service"),
		[]byte("[Service]\nExecStart=/bin/true\n"), 0644)

	reg := NewRegistry()
	u, _ := reg.Resolve("errored")
	u.MarkFailed(7)
	if !u.IsFailed() {
		t.Fatal("setup: should be failed before reset-failed")
	}

	// Re-resolve through a fresh registry like the IPC server does, then
	// clear the marker.
	reg2 := NewRegistry()
	u2, _ := reg2.Resolve("errored")
	u2.ClearFailed()

	// First Unit should also see the cleared state \u2014 the marker is on disk.
	if u.IsFailed() {
		t.Error("after ClearFailed, IsFailed should return false (marker is on disk, not in memory)")
	}
}

func TestStatusShowsFailed(t *testing.T) {
	// status should print "Active: failed" with the exit code when the
	// failed marker is present. Captures the stdout for assertion.
	dir := failedStateTestSetup(t)
	os.WriteFile(filepath.Join(dir, "boom.service"),
		[]byte("[Unit]\nDescription=Boom service\n[Service]\nExecStart=/bin/true\n"), 0644)

	reg := NewRegistry()
	u, _ := reg.Resolve("boom")
	u.MarkFailed(99)

	old := os.Stdout
	r, w, _ := os.Pipe()
	os.Stdout = w
	svcStatus(u, "boom")
	w.Close()
	os.Stdout = old
	buf := make([]byte, 1024)
	n, _ := r.Read(buf)
	got := string(buf[:n])

	if !strings.Contains(got, "Active: failed") {
		t.Errorf("status missing 'Active: failed', got: %q", got)
	}
	if !strings.Contains(got, "code=99") {
		t.Errorf("status missing 'code=99', got: %q", got)
	}
}
