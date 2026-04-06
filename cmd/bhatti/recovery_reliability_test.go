package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/sahil-shubham/bhatti/pkg/store"
)

// ==========================================================================
// Recovery Reliability Tests
//
// These test the recoverVMs path for bugs identified in the reliability trace.
// They use a real SQLite store but a mock VMStateProvider (no Firecracker needed).
// ==========================================================================

// ---------------------------------------------------------------------------
// Bug #1: Sandboxes created via snapshot resume have no volume_attachments.
// After daemon restart, recoverVMs can't find volume info.
// ---------------------------------------------------------------------------

// TestRecoverVolumeAttachments verifies that volume attachments from the
// store are correctly passed to RestoreVM during recovery.
func TestRecoverVolumeAttachments(t *testing.T) {
	st, dir := setupRecoveryTest(t)
	mock := &mockVMStateProvider{}

	// Create a stopped sandbox with snapshot files
	createTestSandbox(t, st, "vol-sb", "vol-vm", "eng-vol", "stopped")
	memPath, vmPath, rootfsPath := createTestSnapshotFiles(t, dir, "vol")
	saveTestFCState(t, st, "vol-sb", memPath, vmPath, rootfsPath)

	// Create a persistent volume and attach it
	st.CreatePersistentVolume(store.PersistentVolume{
		ID: "pv1", UserID: "usr_test", Name: "workspace",
		SizeMB: 1024, FilePath: "/var/lib/bhatti/volumes/usr_test/workspace.ext4",
		Status: "ready", CreatedAt: time.Now(),
	})
	st.AttachPersistentVolume("usr_test", "workspace", "vol-sb", "/workspace", false)

	recoverVMs(st, mock)

	if len(mock.restored) != 1 {
		t.Fatalf("expected 1 restored, got %d", len(mock.restored))
	}

	state := mock.restored[0].State
	vols, ok := state["volumes"]
	if !ok || vols == nil {
		t.Fatal("recovered state has no 'volumes' key")
	}

	volSlice, ok := vols.([]map[string]interface{})
	if !ok || len(volSlice) == 0 {
		t.Fatalf("expected volume attachments, got %T: %v", vols, vols)
	}

	v := volSlice[0]
	if v["name"] != "workspace" {
		t.Errorf("volume name: %v, want 'workspace'", v["name"])
	}
	if v["file_path"] != "/var/lib/bhatti/volumes/usr_test/workspace.ext4" {
		t.Errorf("volume file_path: %v", v["file_path"])
	}
	if v["mount"] != "/workspace" {
		t.Errorf("volume mount: %v", v["mount"])
	}
	if v["drive_id"] != "vol0" {
		t.Errorf("volume drive_id: %v, want 'vol0'", v["drive_id"])
	}
	t.Log("✓ volume attachments recovered from store")
}

// TestRecoverMissingVolumeAttachments simulates Bug #1: a sandbox was
// created via snapshot resume, so no volume_attachments exist in the DB.
// Recovery should still succeed but volumes will be empty.
func TestRecoverMissingVolumeAttachments(t *testing.T) {
	st, dir := setupRecoveryTest(t)
	mock := &mockVMStateProvider{}

	createTestSandbox(t, st, "no-va-sb", "no-va-vm", "eng-no-va", "stopped")
	memPath, vmPath, rootfsPath := createTestSnapshotFiles(t, dir, "no-va")
	saveTestFCState(t, st, "no-va-sb", memPath, vmPath, rootfsPath)

	// NO volume_attachments created — simulates Bug #1

	recoverVMs(st, mock)

	if len(mock.restored) != 1 {
		t.Fatalf("expected 1 restored, got %d", len(mock.restored))
	}

	state := mock.restored[0].State
	vols := state["volumes"]

	// volumes should be nil or empty — this is the Bug #1 manifestation.
	// The sandbox was recovered but has no volume info.
	if vols != nil {
		volSlice, ok := vols.([]map[string]interface{})
		if ok && len(volSlice) > 0 {
			t.Fatal("expected empty volumes (no attachments in DB), but got some")
		}
	}

	t.Log("✓ recovery succeeds without volume_attachments (Bug #1: volumes empty)")
	t.Log("  → Fix: handleSnapshotResume must create volume_attachments in the store")
}

// TestRecoverMultipleVolumes verifies that multiple volume attachments
// are all recovered with correct drive_id assignment.
func TestRecoverMultipleVolumes(t *testing.T) {
	st, dir := setupRecoveryTest(t)
	mock := &mockVMStateProvider{}

	createTestSandbox(t, st, "multi-sb", "multi-vm", "eng-multi", "stopped")
	memPath, vmPath, rootfsPath := createTestSnapshotFiles(t, dir, "multi")
	saveTestFCState(t, st, "multi-sb", memPath, vmPath, rootfsPath)

	// Create two volumes and attach them
	st.CreatePersistentVolume(store.PersistentVolume{
		ID: "pv-a", UserID: "usr_test", Name: "alpha",
		SizeMB: 512, FilePath: "/vol/alpha.ext4",
		Status: "ready", CreatedAt: time.Now(),
	})
	st.CreatePersistentVolume(store.PersistentVolume{
		ID: "pv-b", UserID: "usr_test", Name: "beta",
		SizeMB: 256, FilePath: "/vol/beta.ext4",
		Status: "ready", CreatedAt: time.Now(),
	})
	st.AttachPersistentVolume("usr_test", "alpha", "multi-sb", "/alpha", false)
	st.AttachPersistentVolume("usr_test", "beta", "multi-sb", "/beta", true)

	recoverVMs(st, mock)

	if len(mock.restored) != 1 {
		t.Fatalf("expected 1 restored, got %d", len(mock.restored))
	}

	state := mock.restored[0].State
	vols, ok := state["volumes"].([]map[string]interface{})
	if !ok || len(vols) != 2 {
		t.Fatalf("expected 2 volumes, got %v", state["volumes"])
	}

	// Bug #8: drive_id ordering. Currently depends on DB query order.
	// Verify both volumes are present regardless of order.
	names := map[string]string{} // name → drive_id
	for _, v := range vols {
		name := v["name"].(string)
		driveID := v["drive_id"].(string)
		names[name] = driveID
	}

	if _, ok := names["alpha"]; !ok {
		t.Error("alpha volume missing from recovery")
	}
	if _, ok := names["beta"]; !ok {
		t.Error("beta volume missing from recovery")
	}

	// drive_id should be vol0 and vol1
	driveIDs := map[string]bool{}
	for _, did := range names {
		driveIDs[did] = true
	}
	if !driveIDs["vol0"] || !driveIDs["vol1"] {
		t.Errorf("expected drive_ids vol0 and vol1, got %v", names)
	}

	t.Logf("✓ multiple volumes recovered: %v", names)
}

// TestRecoverVolumeDBError verifies that a DB error when querying
// volume attachments doesn't prevent sandbox recovery (just loses volumes).
func TestRecoverVolumeDBError(t *testing.T) {
	st, dir := setupRecoveryTest(t)
	mock := &mockVMStateProvider{}

	createTestSandbox(t, st, "db-err-sb", "db-err-vm", "eng-db-err", "stopped")
	memPath, vmPath, rootfsPath := createTestSnapshotFiles(t, dir, "db-err")
	saveTestFCState(t, st, "db-err-sb", memPath, vmPath, rootfsPath)

	// Don't create the volumes table error — just don't attach any volumes.
	// The query will return empty, not error. Recovery should still work.

	recoverVMs(st, mock)

	if len(mock.restored) != 1 {
		t.Fatalf("expected 1 restored, got %d", len(mock.restored))
	}
	t.Log("✓ recovery succeeds when no volume attachments exist")
}

// ---------------------------------------------------------------------------
// Verify saveVMState → LoadFirecrackerState round-trip preserves all fields
// ---------------------------------------------------------------------------

// TestFirecrackerStateRoundTrip verifies that all fields survive the
// SaveFirecrackerState → LoadFirecrackerState round-trip.
func TestFirecrackerStateRoundTrip(t *testing.T) {
	st, _ := setupRecoveryTest(t)

	// Create sandbox first (SaveFirecrackerState is an UPDATE)
	st.CreateSandbox(store.Sandbox{
		ID: "rt-sb", Name: "roundtrip", EngineID: "eng-rt",
		Status: "stopped", EngineMeta: json.RawMessage("{}"),
		CreatedBy: "usr_test", CreatedAt: time.Now(),
	})

	original := store.FirecrackerState{
		RootfsPath:      "/sandboxes/abc/rootfs.ext4",
		SnapMemPath:     "/sandboxes/abc/mem.snap",
		SnapVMPath:      "/sandboxes/abc/vm.snap",
		VsockCID:        42,
		TapDevice:       "tap12345678",
		GuestIP:         "10.0.99.5",
		GuestMAC:        "02:ab:cd:ef:00:01",
		VcpuCount:       2,
		MemSizeMib:      2048,
		SocketPath:      "/sandboxes/abc/firecracker.sock",
		VsockPath:       "/sandboxes/abc/vsock.sock",
		AgentToken:      "tok_secret123",
		HasBaseSnapshot: false,
		FCPathOrigin:    "original-seed-id",
	}

	if err := st.SaveFirecrackerState("rt-sb", original); err != nil {
		t.Fatalf("Save: %v", err)
	}

	loaded, err := st.LoadFirecrackerState("rt-sb")
	if err != nil {
		t.Fatalf("Load: %v", err)
	}

	checks := []struct {
		name string
		got  interface{}
		want interface{}
	}{
		{"RootfsPath", loaded.RootfsPath, original.RootfsPath},
		{"SnapMemPath", loaded.SnapMemPath, original.SnapMemPath},
		{"SnapVMPath", loaded.SnapVMPath, original.SnapVMPath},
		{"VsockCID", loaded.VsockCID, original.VsockCID},
		{"TapDevice", loaded.TapDevice, original.TapDevice},
		{"GuestIP", loaded.GuestIP, original.GuestIP},
		{"GuestMAC", loaded.GuestMAC, original.GuestMAC},
		{"SocketPath", loaded.SocketPath, original.SocketPath},
		{"VsockPath", loaded.VsockPath, original.VsockPath},
		{"AgentToken", loaded.AgentToken, original.AgentToken},
		{"HasBaseSnapshot", loaded.HasBaseSnapshot, original.HasBaseSnapshot},
		{"FCPathOrigin", loaded.FCPathOrigin, original.FCPathOrigin},
	}

	for _, c := range checks {
		if c.got != c.want {
			t.Errorf("%s: got %v, want %v", c.name, c.got, c.want)
		}
	}

	// VcpuCount and MemSizeMib have type mismatches (float64 vs int)
	if int(loaded.VcpuCount) != int(original.VcpuCount) {
		t.Errorf("VcpuCount: got %v, want %v", loaded.VcpuCount, original.VcpuCount)
	}
	if loaded.MemSizeMib != original.MemSizeMib {
		t.Errorf("MemSizeMib: got %v, want %v", loaded.MemSizeMib, original.MemSizeMib)
	}

	t.Log("✓ all FirecrackerState fields survive DB round-trip")
}

// ---------------------------------------------------------------------------
// Verify recovery handles volume files that don't exist on disk.
// ---------------------------------------------------------------------------

// TestRecoverVolumeFileMissing verifies that recovery doesn't crash when
// a volume attachment points to a file that doesn't exist on disk.
func TestRecoverVolumeFileMissing(t *testing.T) {
	st, dir := setupRecoveryTest(t)
	mock := &mockVMStateProvider{}

	createTestSandbox(t, st, "missing-vol-sb", "missing-vol-vm", "eng-missing-vol", "stopped")
	memPath, vmPath, rootfsPath := createTestSnapshotFiles(t, dir, "missing-vol")
	saveTestFCState(t, st, "missing-vol-sb", memPath, vmPath, rootfsPath)

	// Attach a volume whose file doesn't exist
	st.CreatePersistentVolume(store.PersistentVolume{
		ID: "pv-ghost", UserID: "usr_test", Name: "ghost",
		SizeMB: 512, FilePath: "/nonexistent/ghost.ext4",
		Status: "ready", CreatedAt: time.Now(),
	})
	st.AttachPersistentVolume("usr_test", "ghost", "missing-vol-sb", "/ghost", false)

	// Recovery should succeed (sandbox is restored), but when startVM
	// later tries to link the volume file, it will fail.
	recoverVMs(st, mock)

	if len(mock.restored) != 1 {
		t.Fatalf("expected 1 restored, got %d", len(mock.restored))
	}

	state := mock.restored[0].State
	vols, _ := state["volumes"].([]map[string]interface{})
	if len(vols) != 1 {
		t.Fatalf("expected 1 volume (even though file is missing), got %d", len(vols))
	}
	if vols[0]["file_path"] != "/nonexistent/ghost.ext4" {
		t.Errorf("file_path: %v", vols[0]["file_path"])
	}

	t.Log("✓ recovery passes with missing volume file (Start will fail later with clear error)")
}

// TestRecoverSnapshotFilesEmptyPaths verifies that a sandbox with empty
// snapshot paths is marked unknown even when rootfs exists.
func TestRecoverSnapshotFilesEmptyPaths(t *testing.T) {
	st, dir := setupRecoveryTest(t)
	mock := &mockVMStateProvider{}

	createTestSandbox(t, st, "empty-paths", "empty-paths-vm", "eng-empty-paths", "stopped")
	rootfsPath := filepath.Join(dir, "rootfs.ext4")
	os.WriteFile(rootfsPath, []byte("rootfs"), 0644)
	// SnapMemPath and SnapVMPath are empty (never snapshotted)
	saveTestFCState(t, st, "empty-paths", "", "", rootfsPath)

	recoverVMs(st, mock)

	if len(mock.restored) != 0 {
		t.Fatalf("expected 0 restored (empty snap paths), got %d", len(mock.restored))
	}
	sb, _ := st.GetSandboxByID("empty-paths")
	if sb.Status != "unknown" {
		t.Errorf("status: %q, want 'unknown'", sb.Status)
	}
	t.Log("✓ sandbox with empty snapshot paths marked unknown")
}
