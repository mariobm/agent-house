//go:build krucible

package krucible

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/mariobm/agent-house/pkg/engine"
)

// recoveryEngine builds a block-root engine bound to a FIXED data dir, so a
// second engine can be constructed over the same state to simulate a daemon
// restart. Skips if libkrun/vmm/mke2fs are unavailable.
func recoveryEngine(t *testing.T, dataDir, baseRootfs string) *Engine {
	t.Helper()
	repo := repoRoot(t)
	if !hasLibkrun() {
		t.Skip("libkrun not installed; skipping")
	}
	if !hasHypervisor() {
		t.Skip("no hypervisor (/dev/kvm or HVF); skipping")
	}
	if _, err := exec.LookPath("mke2fs"); err != nil {
		t.Skip("mke2fs not found; skipping")
	}
	vmm := filepath.Join(repo, "ahvm-vmm")
	if _, err := os.Stat(vmm); err != nil {
		t.Skip("ahvm-vmm not built — run `make vmm`; skipping")
	}
	eng, err := New(Config{
		DataDir: dataDir, BaseRootfs: baseRootfs, VMMBinary: vmm,
		LibDir: libDir(), BlockRoot: true,
	})
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	return eng
}

func helperPID(t *testing.T, e *Engine, id string) int {
	t.Helper()
	e.mu.RLock()
	defer e.mu.RUnlock()
	vm := e.vms[id]
	if vm == nil {
		t.Fatalf("vm %s not registered", id)
	}
	return vm.HelperPID
}

// TestKrucibleRecoveryAdoptLive proves restart-safety: a sandbox's helper is
// detached and survives the engine that spawned it, and a fresh engine built
// over the same data dir (a simulated daemon restart) re-adopts the LIVE VM and
// can exec on it — no reboot, no lost state.
func TestKrucibleRecoveryAdoptLive(t *testing.T) {
	dataDir := t.TempDir()
	base := buildBaseRootfs(t, repoRoot(t))
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	eng1 := recoveryEngine(t, dataDir, base)
	info, err := eng1.Create(ctx, engine.SandboxSpec{Name: "rec", CPUs: 1, MemoryMB: 512})
	if err != nil {
		t.Fatalf("Create: %v", err)
	}
	id := info.ID
	pid := helperPID(t, eng1, id)
	// Guaranteed cleanup even if adoption fails: the helper is detached, so it
	// outlives eng1 — kill it by pid.
	t.Cleanup(func() { syscall.Kill(pid, syscall.SIGKILL) })

	if r, err := eng1.Exec(ctx, id, []string{"echo", "before-restart"}); err != nil || !strings.Contains(r.Stdout, "before-restart") {
		t.Fatalf("pre-restart exec: err=%v out=%q", err, r.Stdout)
	}

	// Simulate a daemon restart: abandon eng1 WITHOUT destroying (the detached
	// helper keeps running), then build a new engine over the same data dir.
	eng2 := recoveryEngine(t, dataDir, base)
	t.Cleanup(func() { eng2.Destroy(context.Background(), id) })

	s, err := eng2.Status(ctx, id)
	if err != nil || s.Status != "running" {
		t.Fatalf("recovered status = %q (err %v), want running", s.Status, err)
	}
	if got := helperPID(t, eng2, id); got != pid {
		t.Fatalf("recovered helper pid = %d, want adopted %d", got, pid)
	}
	if r, err := eng2.Exec(ctx, id, []string{"echo", "recovered-live"}); err != nil || !strings.Contains(r.Stdout, "recovered-live") {
		t.Fatalf("exec on recovered (adopted) VM: err=%v out=%q", err, r.Stdout)
	}
}

// TestKrucibleRecoveryDeadHelper proves the crash path: if the helper died, a
// fresh engine marks the sandbox stopped, and Start cold-boots it fresh (rootfs
// image persists) so it's usable again.
func TestKrucibleRecoveryDeadHelper(t *testing.T) {
	dataDir := t.TempDir()
	base := buildBaseRootfs(t, repoRoot(t))
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()

	eng1 := recoveryEngine(t, dataDir, base)
	// The boot-time config handshake over vsock is flaky under KVM: the
	// guest occasionally boots without its auth token (config fetch EOF),
	// after which every agent attempt fails auth until WaitReady times out.
	// Retry the boot itself a bounded number of times; boot reliability is
	// covered by the other suites, while everything below asserts the
	// crash-recovery semantics this test owns and stays strict.
	var info engine.SandboxInfo
	var err error
	for attempt := 1; ; attempt++ {
		info, err = eng1.Create(ctx, engine.SandboxSpec{Name: "crash", CPUs: 1, MemoryMB: 512})
		if err == nil || attempt == 3 {
			break
		}
		// Back off before retrying: a hard-killed helper can leave stale
		// host-side vsock state behind (libkrun reuses guest CIDs from 3 in
		// every fresh helper), and an immediate relaunch can land on it.
		t.Logf("Create attempt %d failed, backing off and retrying: %v", attempt, err)
		select {
		case <-ctx.Done():
			t.Fatalf("Create: %v", ctx.Err())
		case <-time.After(15 * time.Second):
		}
	}
	if err != nil {
		t.Fatalf("Create: %v", err)
	}
	id := info.ID
	pid := helperPID(t, eng1, id)

	// Crash the helper (SIGKILL), then wait for it to actually exit.
	syscall.Kill(pid, syscall.SIGKILL)
	for i := 0; i < 50 && pidAlive(pid); i++ {
		time.Sleep(100 * time.Millisecond)
	}

	// Fresh engine: recovery should see the dead pid and mark it stopped.
	eng2 := recoveryEngine(t, dataDir, base)
	t.Cleanup(func() { eng2.Destroy(context.Background(), id) })
	if s, err := eng2.Status(ctx, id); err != nil || s.Status != "stopped" {
		t.Fatalf("recovered (dead) status = %q (err %v), want stopped", s.Status, err)
	}

	// Start cold-boots it fresh (no bundle), and it works again.
	if err := eng2.Start(ctx, id); err != nil {
		t.Fatalf("Start (fresh relaunch): %v", err)
	}
	if r, err := eng2.Exec(ctx, id, []string{"echo", "restarted"}); err != nil || !strings.Contains(r.Stdout, "restarted") {
		t.Fatalf("exec after fresh restart: err=%v out=%q", err, r.Stdout)
	}
}
