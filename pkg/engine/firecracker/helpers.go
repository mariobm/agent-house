//go:build linux

package firecracker

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"time"

	"github.com/sahil-shubham/bhatti/pkg/agent"
	"github.com/sahil-shubham/bhatti/pkg/engine"
)

// --- SaveImage (save rootfs as image) ---

// SaveImage pauses the VM, flushes the page cache, copies the rootfs to
// destPath, and resumes. The copy is a complete flat ext4 file capturing
// the filesystem at save time (no memory state).
func (e *Engine) SaveImage(ctx context.Context, sandboxID, destPath string) error {
	vm, err := e.getVM(sandboxID)
	if err != nil {
		return err
	}

	vm.stateMu.Lock()
	defer vm.stateMu.Unlock()

	// Flush guest page cache before pausing — pausing vCPUs does NOT
	// flush dirty pages from guest RAM to the virtio-blk device.
	if vm.Thermal == "hot" && vm.Agent != nil {
		syncCtx, syncCancel := context.WithTimeout(context.Background(), 10*time.Second)
		vm.Agent.Exec(syncCtx, []string{"sync"}, nil, "")
		syncCancel()
	}

	wasPaused := vm.Thermal == "warm"
	if vm.Thermal == "hot" {
		client := fcAPIClient(vm.SocketPath)
		pauseCtx, pauseCancel := context.WithTimeout(ctx, 5*time.Second)
		defer pauseCancel()
		if err := fcPatch(pauseCtx, client, "/vm", `{"state":"Paused"}`); err != nil {
			return fmt.Errorf("pause for save: %w", err)
		}
		vm.Thermal = "warm"
	}

	// Copy rootfs while VM is paused — no concurrent mutations possible
	if err := copyRootfs(vm.RootfsPath, destPath); err != nil {
		if !wasPaused {
			client := fcAPIClient(vm.SocketPath)
			resumeCtx, resumeCancel := context.WithTimeout(context.Background(), 5*time.Second)
			fcPatch(resumeCtx, client, "/vm", `{"state":"Resumed"}`)
			resumeCancel()
			vm.Thermal = "hot"
		}
		return fmt.Errorf("copy rootfs: %w", err)
	}

	if !wasPaused {
		client := fcAPIClient(vm.SocketPath)
		resumeCtx, resumeCancel := context.WithTimeout(ctx, 5*time.Second)
		defer resumeCancel()
		if err := fcPatch(resumeCtx, client, "/vm", `{"state":"Resumed"}`); err != nil {
			return fmt.Errorf("resume after save: %w", err)
		}
		vm.Thermal = "hot"
	}

	return nil
}


// --- Status, List ---

func (e *Engine) Status(ctx context.Context, id string) (engine.SandboxInfo, error) {
	vm, err := e.getVM(id)
	if err != nil {
		return engine.SandboxInfo{}, err
	}
	vm.stateMu.Lock()
	defer vm.stateMu.Unlock()
	return engine.SandboxInfo{
		ID: vm.ID, Name: vm.Name, Status: vm.Status,
		IP: vm.GuestIP, EngineID: vm.ID,
	}, nil
}

// VMState returns the internal state of a VM for persistence.
// Returns nil if the VM doesn't exist.
func (e *Engine) VMState(id string) map[string]interface{} {
	vm, err := e.getVM(id)
	if err != nil {
		return nil
	}
	vm.stateMu.Lock()
	defer vm.stateMu.Unlock()
	return map[string]interface{}{
		"rootfs_path":       vm.RootfsPath,
		"snap_mem_path":     vm.SnapMemPath,
		"snap_vm_path":      vm.SnapVMPath,
		"vsock_cid":         vm.CID,
		"tap_device":        vm.TapDevice,
		"guest_ip":          vm.GuestIP,
		"guest_mac":         vm.GuestMAC,
		"vcpu_count":        vm.VcpuCount,
		"mem_size_mib":      vm.MemSizeMib,
		"socket_path":       vm.SocketPath,
		"vsock_path":        vm.VsockPath,
		"has_base_snapshot": vm.hasBaseSnapshot,
		"agent_token":       vm.Token,
		"volumes":           vm.Volumes,
		"fc_path_origin":    vm.FCPathOrigin,
	}
}

// RestoreVM adds a VM to the engine's in-memory map from persisted state.
// Used during startup recovery.
//
// The state map comes from either JSON unmarshal (all numbers are float64) or
// SQLite (numbers may be int, int64, or float64 depending on the driver).
// All extraction uses type-safe helpers to avoid panics on type mismatch.
func (e *Engine) RestoreVM(id, name, status string, state map[string]interface{}) {
	userID := stateStr(state, "user_id")
	subnetIndex := int(stateInt64(state, "subnet_index"))

	token := stateStr(state, "agent_token")

	thermal := ""
	if status == "stopped" {
		thermal = "cold" // has snapshot on disk, can be resumed
	}

	vm := &VM{
		ID:          id,
		Name:        name,
		UserID:      userID,
		Status:      status,
		Thermal:     thermal,
		Token:       token,
		RootfsPath:  stateStr(state, "rootfs_path"),
		SocketPath:  stateStr(state, "socket_path"),
		VsockPath:   stateStr(state, "vsock_path"),
		CID:         stateUint32(state, "vsock_cid"),
		TapDevice:   stateStr(state, "tap_device"),
		GuestIP:     stateStr(state, "guest_ip"),
		GuestMAC:    stateStr(state, "guest_mac"),
		VcpuCount:   stateInt64(state, "vcpu_count"),
		MemSizeMib:  stateInt64(state, "mem_size_mib"),
		SnapMemPath:     stateStr(state, "snap_mem_path"),
		SnapVMPath:      stateStr(state, "snap_vm_path"),
		FCPathOrigin:    stateStr(state, "fc_path_origin"),
		hasBaseSnapshot: false, // Always reset on recovery — first post-recovery
		// snapshot will be Full, establishing a clean base. The persisted
		// has_base_snapshot may refer to a base that was overwritten.
	}

	// Restore volume attachments (JSON round-trip through interface{})
	if raw, ok := state["volumes"]; ok && raw != nil {
		b, _ := json.Marshal(raw)
		json.Unmarshal(b, &vm.Volumes)
	}

	if status == "running" {
		if token != "" {
			vm.Agent = agent.NewTCPClientWithAuth(vm.GuestIP, token)
		} else {
			vm.Agent = agent.NewTCPClient(vm.GuestIP)
		}
	}

	// Reserve the IP in the user's pool
	if userID != "" && subnetIndex > 0 {
		userNet := e.getOrCreateUserNetwork(userID, subnetIndex)
		userNet.Pool.Mark(vm.GuestIP)
	}

	e.mu.Lock()
	e.vms[id] = vm
	if vm.CID >= e.nextCID {
		e.nextCID = vm.CID + 1
	}
	e.mu.Unlock()
}

func (e *Engine) List(ctx context.Context) ([]engine.SandboxInfo, error) {
	e.mu.RLock()
	defer e.mu.RUnlock()
	var out []engine.SandboxInfo
	for _, vm := range e.vms {
		out = append(out, engine.SandboxInfo{
			ID: vm.ID, Name: vm.Name, Status: vm.Status,
			IP: vm.GuestIP, EngineID: vm.ID,
		})
	}
	return out, nil
}

// --- State extraction helpers (type-safe for JSON float64 / SQLite int) ---

func stateStr(m map[string]interface{}, key string) string {
	if v, ok := m[key].(string); ok {
		return v
	}
	return ""
}

func stateInt64(m map[string]interface{}, key string) int64 {
	switch v := m[key].(type) {
	case int:
		return int64(v)
	case int64:
		return v
	case float64:
		return int64(v)
	case uint32:
		return int64(v)
	}
	return 0
}

func stateUint32(m map[string]interface{}, key string) uint32 {
	switch v := m[key].(type) {
	case int:
		return uint32(v)
	case int64:
		return uint32(v)
	case float64:
		return uint32(v)
	case uint32:
		return v
	}
	return 0
}

func stateBool(m map[string]interface{}, key string) bool {
	switch v := m[key].(type) {
	case bool:
		return v
	case int:
		return v != 0
	case float64:
		return v != 0
	}
	return false
}

// --- Helpers ---

func generateID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return fmt.Sprintf("%x", b)
}

func generateMAC() string {
	b := make([]byte, 6)
	rand.Read(b)
	b[0] = (b[0] & 0xfe) | 0x02 // locally administered, unicast
	return fmt.Sprintf("%02x:%02x:%02x:%02x:%02x:%02x", b[0], b[1], b[2], b[3], b[4], b[5])
}

// injectLoharIntoRootfs mounts the rootfs and overwrites /usr/local/bin/lohar
// with the current binary from DataDir. This ensures every sandbox uses the
// latest guest agent, preventing protocol drift after daemon upgrades.
// Adds ~50ms to sandbox creation (mount + cp + umount).
func injectLoharIntoRootfs(rootfsPath, dataDir string) error {
	loharSrc := filepath.Join(dataDir, "lohar")
	if _, err := os.Stat(loharSrc); err != nil {
		return nil // no lohar binary to inject (dev mode)
	}
	mnt, err := os.MkdirTemp("", "bhatti-inject-*")
	if err != nil {
		return err
	}
	defer os.RemoveAll(mnt)
	if err := exec.Command("mount", "-o", "loop", rootfsPath, mnt).Run(); err != nil {
		return fmt.Errorf("mount rootfs for lohar injection: %w", err)
	}
	defer exec.Command("umount", mnt).Run()
	dst := filepath.Join(mnt, "usr/local/bin/lohar")
	if err := exec.Command("cp", loharSrc, dst).Run(); err != nil {
		return fmt.Errorf("copy lohar: %w", err)
	}
	return os.Chmod(dst, 0755)
}

// verifySnapshotArtifacts performs lightweight sanity checks on snapshot files.
// Catches truncated files and zero-byte files without spawning a throwaway
// Firecracker process.
func verifySnapshotArtifacts(vmSnapPath, memSnapPath string, memSizeMib int64, snapshotType string) error {
	// vm.snap must exist and be non-empty.
	// Note: FC ≥1.14 uses a binary format for vm.snap, not JSON.
	vmInfo, err := os.Stat(vmSnapPath)
	if err != nil {
		return fmt.Errorf("stat vm.snap: %w", err)
	}
	if vmInfo.Size() == 0 {
		return fmt.Errorf("vm.snap is empty (0 bytes)")
	}

	// mem.snap size sanity
	memInfo, err := os.Stat(memSnapPath)
	if err != nil {
		return fmt.Errorf("stat mem.snap: %w", err)
	}
	expectedFull := memSizeMib * 1024 * 1024
	if snapshotType == "Full" && memInfo.Size() != expectedFull {
		return fmt.Errorf("Full snapshot mem.snap size %d != expected %d (VM memory)",
			memInfo.Size(), expectedFull)
	}
	if snapshotType == "Diff" && (memInfo.Size() == 0 || memInfo.Size() > expectedFull) {
		return fmt.Errorf("Diff snapshot mem.snap size %d out of range (0, %d]",
			memInfo.Size(), expectedFull)
	}

	return nil
}
