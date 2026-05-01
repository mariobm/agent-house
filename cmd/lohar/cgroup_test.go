//go:build linux

package main

import (
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestParseMemoryValue(t *testing.T) {
	cases := []struct {
		in, want string
	}{
		{"", "max"},
		{"infinity", "max"},
		{"max", "max"},
		{"512", "512"},                  // bare bytes
		{"1K", "1024"},
		{"512M", strconv.FormatUint(512*1024*1024, 10)},
		{"1G", "1073741824"},
		{"1MB", strconv.FormatUint(1024*1024, 10)}, // trailing B accepted
		{"garbage", "max"},                          // unparseable -> permissive default
	}
	for _, c := range cases {
		if got := parseMemoryValue(c.in); got != c.want {
			t.Errorf("parseMemoryValue(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestParseInfinityOrInt(t *testing.T) {
	cases := []struct {
		in, want string
	}{
		{"", "max"},
		{"infinity", "max"},
		{"512", "512"},
		{"garbage", "max"},
	}
	for _, c := range cases {
		if got := parseInfinityOrInt(c.in); got != c.want {
			t.Errorf("parseInfinityOrInt(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestParseCPUQuota(t *testing.T) {
	cases := []struct {
		in, want string
	}{
		{"50%", "50000 100000"},
		{"100%", "100000 100000"},
		{"200%", "200000 100000"},
		{"25%", "25000 100000"},
		{"", "max 100000"},          // empty/zero -> no limit
		{"garbage", "max 100000"},
	}
	for _, c := range cases {
		if got := parseCPUQuota(c.in); got != c.want {
			t.Errorf("parseCPUQuota(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestKillModeFor(t *testing.T) {
	mk := func(km string) *Unit {
		return &Unit{
			Sections: serviceFile{sections: map[string][]kvPair{
				"Service": {{key: "KillMode", value: km}},
			}},
		}
	}
	for _, km := range []string{"control-group", "process", "mixed", "none"} {
		if got := killModeFor(mk(km)); got != km {
			t.Errorf("killModeFor(%q) = %q", km, got)
		}
	}
	// Default + unrecognised both yield control-group (matches systemd's
	// permissive parser).
	if got := killModeFor(mk("")); got != "control-group" {
		t.Errorf("killModeFor(unset) = %q, want control-group", got)
	}
	if got := killModeFor(mk("garbage")); got != "control-group" {
		t.Errorf("killModeFor(garbage) = %q, want control-group", got)
	}
}

// cgroupTestSetup returns a Unit pointing into a fake cgroup root under
// t.TempDir(). Most cgroup operations work on regular dirs (mkdir,
// writes), so the tests exercise the real code paths without needing
// root or a real cgroup mount. KillCgroup is the exception (writes "1"
// to a file, which won't actually kill anything in a fake tree) \u2014 those
// tests are gated separately.
func cgroupTestSetup(t *testing.T) *Unit {
	t.Helper()
	dir := t.TempDir()
	root := t.TempDir()
	origDirs := serviceDirs
	origRoot := cgroupRoot
	origPid := pidDir
	origLog := logDir
	serviceDirs = []string{dir}
	cgroupRoot = root
	pidDir = t.TempDir()
	logDir = t.TempDir()
	t.Cleanup(func() {
		serviceDirs = origDirs
		cgroupRoot = origRoot
		pidDir = origPid
		logDir = origLog
	})

	os.WriteFile(filepath.Join(dir, "test.service"), []byte(`
[Service]
ExecStart=/bin/true
MemoryMax=512M
TasksMax=128
CPUQuota=50%
`), 0644)
	reg := NewRegistry()
	u, err := reg.Resolve("test")
	if err != nil {
		t.Fatalf("Resolve: %v", err)
	}
	return u
}

func TestCgroupPath(t *testing.T) {
	u := cgroupTestSetup(t)
	want := filepath.Join(cgroupRoot, "system.slice", "test.service")
	if got := u.CgroupPath(); got != want {
		t.Errorf("CgroupPath = %q, want %q", got, want)
	}
}

func TestCreateCgroupAppliesLimits(t *testing.T) {
	u := cgroupTestSetup(t)
	if err := u.CreateCgroup(); err != nil {
		t.Fatalf("CreateCgroup: %v", err)
	}
	cg := u.CgroupPath()
	if _, err := os.Stat(cg); err != nil {
		t.Fatalf("cgroup dir not created: %v", err)
	}
	// Resource limits should have been written to control files.
	cases := []struct {
		file, want string
	}{
		{"memory.max", strconv.FormatUint(512*1024*1024, 10)},
		{"pids.max", "128"},
		{"cpu.max", "50000 100000"},
	}
	for _, c := range cases {
		data, err := os.ReadFile(filepath.Join(cg, c.file))
		if err != nil {
			t.Errorf("read %s: %v", c.file, err)
			continue
		}
		got := strings.TrimSpace(string(data))
		if got != c.want {
			t.Errorf("%s = %q, want %q", c.file, got, c.want)
		}
	}
}

func TestPlaceInCgroup(t *testing.T) {
	u := cgroupTestSetup(t)
	if err := u.CreateCgroup(); err != nil {
		t.Fatalf("CreateCgroup: %v", err)
	}
	// Pretend-write a PID. In real use the kernel rejects PIDs that
	// aren't actually running, but in a fake cgroup tree any write
	// succeeds \u2014 we're testing that PlaceInCgroup writes the right value
	// to the right file, not the kernel's accept logic.
	if err := u.PlaceInCgroup(1234); err != nil {
		t.Fatalf("PlaceInCgroup: %v", err)
	}
	data, err := os.ReadFile(filepath.Join(u.CgroupPath(), "cgroup.procs"))
	if err != nil {
		t.Fatalf("read cgroup.procs: %v", err)
	}
	if strings.TrimSpace(string(data)) != "1234" {
		t.Errorf("cgroup.procs = %q, want 1234", string(data))
	}
	if !u.CgroupHasProcs() {
		t.Error("CgroupHasProcs should be true after PlaceInCgroup")
	}
}

func TestRemoveCgroup(t *testing.T) {
	// In production cgroup v2, the directory contains kernel-virtual
	// files (memory.max etc.) that auto-clean when the dir is rmdir'd.
	// In our tempdir fake those are real files — so we drop them first
	// to mirror the production end-state, then verify RemoveCgroup
	// (which is just rmdir) succeeds.
	u := cgroupTestSetup(t)
	u.CreateCgroup()
	entries, _ := os.ReadDir(u.CgroupPath())
	for _, e := range entries {
		os.Remove(filepath.Join(u.CgroupPath(), e.Name()))
	}
	if err := u.RemoveCgroup(); err != nil {
		t.Errorf("RemoveCgroup: %v", err)
	}
	if _, err := os.Stat(u.CgroupPath()); !os.IsNotExist(err) {
		t.Errorf("cgroup not removed: %v", err)
	}
}

func TestCgroupKillIntegration(t *testing.T) {
	// Real kernel cgroup test: requires root + a real cgroup v2 mount +
	// kernel >= 5.14. Skip in unprivileged environments.
	if os.Getuid() != 0 {
		t.Skip("requires root for real cgroup operations")
	}
	if _, err := os.Stat("/sys/fs/cgroup/cgroup.controllers"); err != nil {
		t.Skip("requires cgroup v2 mounted at /sys/fs/cgroup")
	}
	if _, err := os.Stat("/sys/fs/cgroup/system.slice"); err != nil {
		// Try to create it for the test.
		if err := os.MkdirAll("/sys/fs/cgroup/system.slice", 0755); err != nil {
			t.Skipf("can't create system.slice: %v", err)
		}
	}

	dir := t.TempDir()
	origDirs := serviceDirs
	origPid := pidDir
	origLog := logDir
	serviceDirs = []string{dir}
	pidDir = t.TempDir()
	logDir = t.TempDir()
	defer func() {
		serviceDirs = origDirs
		pidDir = origPid
		logDir = origLog
	}()

	// Create a unit whose ExecStart spawns a child that intentionally
	// daemonises (setsid + fork) to escape the parent's PGID. Without
	// cgroup-per-unit, PGID-kill would miss it. With cgroup.kill, the
	// kernel SIGKILLs everything in the cgroup atomically.
	os.WriteFile(filepath.Join(dir, "fork-bomb.service"), []byte(`
[Service]
ExecStart=/bin/sh -c "( /bin/sleep 60 & ) ; /bin/sleep 60"
`), 0644)
	reg := NewRegistry()
	u, _ := reg.Resolve("fork-bomb")

	if err := svcStart(u); err != nil {
		t.Fatalf("svcStart: %v", err)
	}
	defer u.RemoveCgroup()

	// Give the children time to start.
	time.Sleep(200 * time.Millisecond)

	// At least 2 procs in the cgroup: the shell + each background sleep.
	// The kernel may have reaped the shell already; we just need >= 1.
	if !u.CgroupHasProcs() {
		t.Fatal("cgroup is empty after svcStart")
	}

	if err := u.KillCgroup(); err != nil {
		t.Fatalf("KillCgroup: %v", err)
	}
	if !u.WaitCgroupDrain(2 * time.Second) {
		t.Errorf("cgroup did not drain after KillCgroup")
	}
}

// TestSvcStopUsesCgroupKillIfAvailable exercises the svcStop path that
// writes to cgroup.kill. We can't fake the kernel side, so we use a
// non-root path: a tempdir cgroup tree where cgroup.kill is just a
// regular file \u2014 the WRITE succeeds (no actual kill happens) and svcStop
// proceeds through its drain+remove cleanup.
func TestSvcStopUsesCgroupKillPathWhenFileExists(t *testing.T) {
	u := cgroupTestSetup(t)

	// Pretend a daemon is running: write a pidfile pointing at a real
	// process we own (so processAlive returns true), create the cgroup,
	// and put cgroup.kill in place so svcStop's KillCgroup write succeeds.
	cmd := exec.Command("/bin/sleep", "30")
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err := cmd.Start(); err != nil {
		t.Fatalf("spawn: %v", err)
	}
	defer cmd.Process.Kill()
	go cmd.Wait() // reap zombie

	u.CreateCgroup()
	// Touch cgroup.kill so the write path succeeds. Don't actually put
	// the sleep into our fake cgroup \u2014 we want svcStop to "succeed via
	// cgroup.kill" without actually killing the test's sleep.
	os.WriteFile(filepath.Join(u.CgroupPath(), "cgroup.kill"), []byte{}, 0644)

	u.WritePID(cmd.Process.Pid)

	// svcStop should write 1 to cgroup.kill and proceed. Our fake
	// cgroup.kill is a regular file, so the write succeeds without any
	// actual kill effect. WaitCgroupDrain returns immediately because
	// cgroup.procs is empty (we didn't add the sleep). Cleanup proceeds.
	//
	// Note: the test sleep is NOT killed by this svcStop, because our
	// cgroup.kill is a regular file, not a kernel-backed control file.
	// We separately kill it in the deferred cmd.Process.Kill().
	if err := svcStop(u); err != nil {
		t.Errorf("svcStop: %v", err)
	}

	// Pidfile should be gone.
	if _, err := os.Stat(u.PidPath()); !os.IsNotExist(err) {
		t.Errorf("pidfile not removed")
	}
	// Verify svcStop took the control-group path by checking cgroup.kill
	// got the "1" write. In production the kernel SIGKILLs the cgroup
	// and rmdir succeeds on the now-empty virtual dir; in our tempdir
	// fake, leftover memory.max/pids.max/cpu.max real files block rmdir,
	// so the cgroup dir survives. That's a tempdir artefact, not a bug
	// in the production code. The crucial assertion is that svcStop
	// wrote the right control file:
	data, err := os.ReadFile(filepath.Join(u.CgroupPath(), "cgroup.kill"))
	if err != nil {
		t.Fatalf("read cgroup.kill: %v", err)
	}
	if strings.TrimSpace(string(data)) != "1" {
		t.Errorf("cgroup.kill contents = %q, want 1 (svcStop didn't take the control-group path)", string(data))
	}
}

func TestHumanBytes(t *testing.T) {
	cases := []struct {
		n    uint64
		want string
	}{
		{0, "0B"},
		{512, "512B"},
		{1024, "1.0K"},
		{1536, "1.5K"},
		{1024 * 1024, "1.0M"},
		{124*1024*1024 + 512*1024, "124.5M"},
		{1024 * 1024 * 1024, "1.0G"},
	}
	for _, c := range cases {
		if got := humanBytes(c.n); got != c.want {
			t.Errorf("humanBytes(%d) = %q, want %q", c.n, got, c.want)
		}
	}
}
