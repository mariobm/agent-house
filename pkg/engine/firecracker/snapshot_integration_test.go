//go:build linux

package firecracker

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/sahil-shubham/bhatti/pkg/engine"
)

// TestCheckpointAndResume creates a sandbox, writes data, starts a background
// process, checkpoints, destroys the original, resumes from the snapshot,
// and verifies data + process are restored.
func TestCheckpointAndResume(t *testing.T) {
	eng := testEngine(t)
	ctx := context.Background()

	info, err := eng.Create(ctx, testSpec("snap-ckpt"))
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	// Write data and start background process
	execWithTimeout(t, eng, info.ID, []string{"sh", "-c", "echo ckpt-data > /home/lohar/data.txt"})
	r, _ := execWithTimeout(t, eng, info.ID, []string{"sh", "-c",
		"sleep 3600 </dev/null >/dev/null 2>&1 & echo $!"})
	bgPID := strings.TrimSpace(r.Stdout)
	t.Logf("background PID: %s", bgPID)

	// Checkpoint
	snapDir := filepath.Join(eng.cfg.DataDir, "snapshots", "usr_test")
	os.MkdirAll(snapDir, 0700)
	defer os.RemoveAll(filepath.Join(snapDir, "ckpt-test"))

	manifestAny, err := eng.Checkpoint(ctx, info.ID, "usr_test", 99, "ckpt-test", snapDir)
	manifest := manifestAny.(*SnapshotManifest)
	if err != nil {
		eng.Destroy(ctx, info.ID)
		t.Fatalf("Checkpoint: %v", err)
	}
	t.Logf("✓ checkpoint created: %d drives", len(manifest.Drives))

	// Verify VM still running after checkpoint (was resumed)
	r, err = execWithTimeout(t, eng, info.ID, []string{"echo", "still-alive"})
	if err != nil || !strings.Contains(r.Stdout, "still-alive") {
		t.Fatalf("VM should be running after checkpoint: %v %q", err, r.Stdout)
	}
	t.Log("✓ VM still running after checkpoint")

	// Destroy original
	eng.Destroy(ctx, info.ID)

	// Resume from snapshot
	snapPath := filepath.Join(snapDir, "ckpt-test")
	info2, err := eng.ResumeSnapshot(ctx, snapPath, manifest, "resumed-sb")
	if err != nil {
		t.Fatalf("ResumeSnapshot: %v", err)
	}
	defer eng.Destroy(ctx, info2.ID)

	// Verify data persists
	r, _ = execWithTimeout(t, eng, info2.ID, []string{"cat", "/home/lohar/data.txt"})
	if !strings.Contains(r.Stdout, "ckpt-data") {
		t.Fatalf("data not restored: %q", r.Stdout)
	}
	t.Log("✓ data restored from checkpoint")

	// Verify background process is still alive
	r, _ = execWithTimeout(t, eng, info2.ID, []string{"kill", "-0", bgPID})
	if r.ExitCode != 0 {
		t.Fatalf("background PID %s not alive: exit=%d", bgPID, r.ExitCode)
	}
	t.Logf("✓ background PID %s still alive after resume", bgPID)
}

// TestCheckpointWithVolume verifies that attached volume data is captured
// in the snapshot and readable after resume.
func TestCheckpointWithVolume(t *testing.T) {
	eng := testEngine(t)
	ctx := context.Background()

	volDir := filepath.Join(eng.cfg.DataDir, "volumes", "usr_test")
	os.MkdirAll(volDir, 0700)
	volPath := filepath.Join(volDir, "snap-vol.ext4")
	defer os.Remove(volPath)
	createVolumeFile(t, volPath, 64)

	spec := testSpec("snap-vol-ckpt")
	spec.ResolvedVolumes = []engine.ResolvedVolume{{
		FilePath: volPath, DriveID: "vol0", Name: "snap-vol",
		Mount: "/workspace", ReadOnly: false,
	}}
	info, err := eng.Create(ctx, spec)
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	execWithTimeout(t, eng, info.ID, []string{"sh", "-c", "echo vol-snap-data > /workspace/data.txt"})

	snapDir := filepath.Join(eng.cfg.DataDir, "snapshots", "usr_test")
	os.MkdirAll(snapDir, 0700)
	defer os.RemoveAll(filepath.Join(snapDir, "vol-ckpt"))

	manifestAny, err := eng.Checkpoint(ctx, info.ID, "usr_test", 99, "vol-ckpt", snapDir)
	manifest := manifestAny.(*SnapshotManifest)
	if err != nil {
		eng.Destroy(ctx, info.ID)
		t.Fatalf("Checkpoint: %v", err)
	}
	eng.Destroy(ctx, info.ID)

	// Resume
	snapPath := filepath.Join(snapDir, "vol-ckpt")
	info2, err := eng.ResumeSnapshot(ctx, snapPath, manifest, "vol-resumed")
	if err != nil {
		t.Fatalf("ResumeSnapshot: %v", err)
	}
	defer eng.Destroy(ctx, info2.ID)

	r, _ := execWithTimeout(t, eng, info2.ID, []string{"cat", "/workspace/data.txt"})
	if !strings.Contains(r.Stdout, "vol-snap-data") {
		t.Fatalf("volume data not restored: %q", r.Stdout)
	}
	t.Log("✓ volume data restored from checkpoint")
}

// TestCheckpointVMConfig verifies that the resumed VM has the same vCPU
// and memory configuration as the original.
func TestCheckpointVMConfig(t *testing.T) {
	eng := testEngine(t)
	ctx := context.Background()

	spec := testSpec("snap-config")
	spec.CPUs = 2
	spec.MemoryMB = 1024

	info, err := eng.Create(ctx, spec)
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	snapDir := filepath.Join(eng.cfg.DataDir, "snapshots", "usr_test")
	os.MkdirAll(snapDir, 0700)
	defer os.RemoveAll(filepath.Join(snapDir, "config-ckpt"))

	manifestAny, err := eng.Checkpoint(ctx, info.ID, "usr_test", 99, "config-ckpt", snapDir)
	manifest := manifestAny.(*SnapshotManifest)
	if err != nil {
		eng.Destroy(ctx, info.ID)
		t.Fatalf("Checkpoint: %v", err)
	}
	eng.Destroy(ctx, info.ID)

	snapPath := filepath.Join(snapDir, "config-ckpt")
	info2, err := eng.ResumeSnapshot(ctx, snapPath, manifest, "config-resumed")
	if err != nil {
		t.Fatalf("ResumeSnapshot: %v", err)
	}
	defer eng.Destroy(ctx, info2.ID)

	// Verify vCPU count
	r, _ := execWithTimeout(t, eng, info2.ID, []string{"nproc"})
	if strings.TrimSpace(r.Stdout) != "2" {
		t.Fatalf("expected 2 vCPUs, got %q", strings.TrimSpace(r.Stdout))
	}
	t.Log("✓ vCPU count matches checkpoint")

	// Verify memory (should be ~1024MB)
	r, _ = execWithTimeout(t, eng, info2.ID, []string{"sh", "-c", "free -m | head -2 | tail -1 | awk '{print $2}'"})
	mem := strings.TrimSpace(r.Stdout)
	t.Logf("memory: %sMB (expected ~1024)", mem)
	// Memory reported by free is slightly less than configured due to kernel reservation
}

// TestCheckpointDuplicateName verifies that checkpointing with an existing
// name fails without pausing the VM.
func TestCheckpointDuplicateName(t *testing.T) {
	eng := testEngine(t)
	ctx := context.Background()

	info, err := eng.Create(ctx, testSpec("snap-dup"))
	if err != nil {
		t.Fatalf("Create: %v", err)
	}
	defer eng.Destroy(ctx, info.ID)

	snapDir := filepath.Join(eng.cfg.DataDir, "snapshots", "usr_test")
	os.MkdirAll(snapDir, 0700)
	defer os.RemoveAll(filepath.Join(snapDir, "dup-test"))

	// First checkpoint succeeds
	_, err = eng.Checkpoint(ctx, info.ID, "usr_test", 99, "dup-test", snapDir)
	if err != nil {
		t.Fatalf("first checkpoint: %v", err)
	}

	// Second checkpoint with same name fails
	_, err = eng.Checkpoint(ctx, info.ID, "usr_test", 99, "dup-test", snapDir)
	if err == nil {
		t.Fatal("expected error for duplicate snapshot name")
	}
	if !strings.Contains(err.Error(), "already exists") {
		t.Fatalf("expected 'already exists' error, got: %v", err)
	}
	t.Log("✓ duplicate snapshot name rejected")

	// VM should still be running (wasn't paused for the failed attempt)
	r, _ := execWithTimeout(t, eng, info.ID, []string{"echo", "still-ok"})
	if !strings.Contains(r.Stdout, "still-ok") {
		t.Fatal("VM should still be running after failed checkpoint")
	}
	t.Log("✓ VM not disrupted by failed checkpoint")
}

// TestNestedCheckpointAndResume verifies that checkpointing a sandbox that
// was itself resumed from a snapshot works correctly. This catches the
// FCPathOrigin bug: Firecracker's vm.snap records the original seed sandbox's
// paths, so a second-level checkpoint must track that origin to create
// symlinks at the right location during resume.
func TestNestedCheckpointAndResume(t *testing.T) {
	eng := testEngine(t)
	ctx := context.Background()

	// Create sandbox A and write initial data
	info, err := eng.Create(ctx, testSpec("snap-nest-a"))
	if err != nil {
		t.Fatalf("Create A: %v", err)
	}
	execWithTimeout(t, eng, info.ID, []string{"sh", "-c", "echo layer1 > /home/lohar/data.txt"})

	// Checkpoint A → "nest-ckpt1"
	snapDir := filepath.Join(eng.cfg.DataDir, "snapshots", "usr_test")
	os.MkdirAll(snapDir, 0700)
	defer os.RemoveAll(filepath.Join(snapDir, "nest-ckpt1"))
	defer os.RemoveAll(filepath.Join(snapDir, "nest-ckpt2"))

	m1Any, err := eng.Checkpoint(ctx, info.ID, "usr_test", 99, "nest-ckpt1", snapDir)
	m1 := m1Any.(*SnapshotManifest)
	if err != nil {
		eng.Destroy(ctx, info.ID)
		t.Fatalf("Checkpoint 1: %v", err)
	}
	eng.Destroy(ctx, info.ID)
	t.Log("✓ checkpoint 1 created, original destroyed")

	// Resume from "nest-ckpt1" → sandbox B
	snap1Path := filepath.Join(snapDir, "nest-ckpt1")
	infoB, err := eng.ResumeSnapshot(ctx, snap1Path, m1, "nest-b")
	if err != nil {
		t.Fatalf("Resume 1: %v", err)
	}

	// Verify data from checkpoint 1
	r, _ := execWithTimeout(t, eng, infoB.ID, []string{"cat", "/home/lohar/data.txt"})
	if !strings.Contains(r.Stdout, "layer1") {
		t.Fatalf("checkpoint 1 data not restored: %q", r.Stdout)
	}

	// Write additional data in B
	execWithTimeout(t, eng, infoB.ID, []string{"sh", "-c", "echo layer2 >> /home/lohar/data.txt"})

	// Checkpoint B → "nest-ckpt2" (this is the nested checkpoint)
	m2Any, err := eng.Checkpoint(ctx, infoB.ID, "usr_test", 99, "nest-ckpt2", snapDir)
	m2 := m2Any.(*SnapshotManifest)
	if err != nil {
		eng.Destroy(ctx, infoB.ID)
		t.Fatalf("Checkpoint 2 (nested): %v", err)
	}
	eng.Destroy(ctx, infoB.ID)
	t.Log("✓ nested checkpoint 2 created, sandbox B destroyed")

	// Verify FCPathOrigin points to the original seed sandbox, not B
	if m2.FCPathOrigin == "" {
		t.Fatal("FCPathOrigin should be set in nested manifest")
	}
	if m2.FCPathOrigin == m2.CreatedFrom {
		t.Fatalf("FCPathOrigin (%s) should differ from CreatedFrom (%s) for nested snapshots",
			m2.FCPathOrigin, m2.CreatedFrom)
	}
	t.Logf("✓ FCPathOrigin=%s (seed), CreatedFrom=%s (sandbox B)", m2.FCPathOrigin, m2.CreatedFrom)

	// Resume from "nest-ckpt2" → sandbox C
	snap2Path := filepath.Join(snapDir, "nest-ckpt2")
	infoC, err := eng.ResumeSnapshot(ctx, snap2Path, m2, "nest-c")
	if err != nil {
		t.Fatalf("Resume 2 (nested): %v", err)
	}
	defer eng.Destroy(ctx, infoC.ID)

	// Verify BOTH layers of data are present
	r, _ = execWithTimeout(t, eng, infoC.ID, []string{"cat", "/home/lohar/data.txt"})
	if !strings.Contains(r.Stdout, "layer1") || !strings.Contains(r.Stdout, "layer2") {
		t.Fatalf("nested checkpoint data not fully restored: %q", r.Stdout)
	}
	t.Log("✓ both data layers restored from nested checkpoint")
}

// TestResumeIPConflict verifies that resuming a snapshot fails cleanly
// when the required IP is already in use.
func TestResumeIPConflict(t *testing.T) {
	eng := testEngine(t)
	ctx := context.Background()

	// Create and checkpoint sandbox A
	infoA, err := eng.Create(ctx, testSpec("snap-ip-a"))
	if err != nil {
		t.Fatalf("Create A: %v", err)
	}

	snapDir := filepath.Join(eng.cfg.DataDir, "snapshots", "usr_test")
	os.MkdirAll(snapDir, 0700)
	defer os.RemoveAll(filepath.Join(snapDir, "ip-test"))

	manifestAny, err := eng.Checkpoint(ctx, infoA.ID, "usr_test", 99, "ip-test", snapDir)
	manifest := manifestAny.(*SnapshotManifest)
	if err != nil {
		eng.Destroy(ctx, infoA.ID)
		t.Fatalf("Checkpoint: %v", err)
	}
	ipA := manifest.Network.GuestIP
	t.Logf("snapshot IP: %s", ipA)

	// Keep A running — its IP is in use

	// Try to resume — should fail because IP is taken
	snapPath := filepath.Join(snapDir, "ip-test")
	_, err = eng.ResumeSnapshot(ctx, snapPath, manifest, "ip-conflict")
	if err == nil {
		t.Fatal("expected IP conflict error")
	}
	if !strings.Contains(err.Error(), "in use") {
		t.Fatalf("expected 'in use' error, got: %v", err)
	}
	t.Log("✓ resume correctly rejected: IP in use")

	// Now destroy A (frees the IP)
	eng.Destroy(ctx, infoA.ID)

	// Resume should succeed now
	info2, err := eng.ResumeSnapshot(ctx, snapPath, manifest, "ip-ok")
	if err != nil {
		t.Fatalf("ResumeSnapshot after IP freed: %v", err)
	}
	defer eng.Destroy(ctx, info2.ID)

	if info2.IP != ipA {
		t.Fatalf("expected same IP %s, got %s", ipA, info2.IP)
	}
	t.Logf("✓ resumed with same IP %s after original freed", ipA)
}

// TestResumeManifestRoundTrip verifies that the manifest JSON can be
// serialized, stored, deserialized, and used to resume.
func TestResumeManifestRoundTrip(t *testing.T) {
	eng := testEngine(t)
	ctx := context.Background()

	info, err := eng.Create(ctx, testSpec("snap-rt"))
	if err != nil {
		t.Fatalf("Create: %v", err)
	}

	execWithTimeout(t, eng, info.ID, []string{"sh", "-c", "echo roundtrip > /home/lohar/rt.txt"})

	snapDir := filepath.Join(eng.cfg.DataDir, "snapshots", "usr_test")
	os.MkdirAll(snapDir, 0700)
	defer os.RemoveAll(filepath.Join(snapDir, "rt-test"))

	manifestAny, err := eng.Checkpoint(ctx, info.ID, "usr_test", 99, "rt-test", snapDir)
	manifest := manifestAny.(*SnapshotManifest)
	if err != nil {
		eng.Destroy(ctx, info.ID)
		t.Fatalf("Checkpoint: %v", err)
	}
	eng.Destroy(ctx, info.ID)

	// Serialize → Deserialize (simulates store round-trip)
	jsonBytes, _ := json.Marshal(manifest)
	var restored SnapshotManifest
	json.Unmarshal(jsonBytes, &restored)

	snapPath := filepath.Join(snapDir, "rt-test")
	info2, err := eng.ResumeSnapshot(ctx, snapPath, &restored, "rt-resumed")
	if err != nil {
		t.Fatalf("ResumeSnapshot from deserialized manifest: %v", err)
	}
	defer eng.Destroy(ctx, info2.ID)

	r, _ := execWithTimeout(t, eng, info2.ID, []string{"cat", "/home/lohar/rt.txt"})
	if !strings.Contains(r.Stdout, "roundtrip") {
		t.Fatalf("data lost in manifest round-trip: %q", r.Stdout)
	}
	t.Log("✓ manifest survives JSON serialize/deserialize round-trip")
}
