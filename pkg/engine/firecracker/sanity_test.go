//go:build linux

package firecracker

import (
	"os"
	"path/filepath"
	"testing"
)

func TestVerifySnapshotArtifacts_Valid(t *testing.T) {
	dir := t.TempDir()

	// Valid vm.snap (non-empty)
	vmPath := filepath.Join(dir, "vm.snap")
	os.WriteFile(vmPath, []byte("binary snapshot data"), 0644)

	// Valid Full mem.snap (exact size)
	memPath := filepath.Join(dir, "mem.snap")
	mem := make([]byte, 512*1024*1024) // 512 MiB
	os.WriteFile(memPath, mem, 0644)

	if err := verifySnapshotArtifacts(vmPath, memPath, 512, "Full"); err != nil {
		t.Errorf("expected valid, got: %v", err)
	}
}

func TestVerifySnapshotArtifacts_EmptyVMSnap(t *testing.T) {
	dir := t.TempDir()

	vmPath := filepath.Join(dir, "vm.snap")
	os.WriteFile(vmPath, []byte{}, 0644) // empty

	memPath := filepath.Join(dir, "mem.snap")
	os.WriteFile(memPath, make([]byte, 512*1024*1024), 0644)

	err := verifySnapshotArtifacts(vmPath, memPath, 512, "Full")
	if err == nil {
		t.Fatal("expected error for empty vm.snap")
	}
	if !contains(err.Error(), "empty") {
		t.Errorf("expected 'empty' in error, got: %v", err)
	}
}

func TestVerifySnapshotArtifacts_MissingVMSnap(t *testing.T) {
	dir := t.TempDir()
	memPath := filepath.Join(dir, "mem.snap")
	os.WriteFile(memPath, make([]byte, 1024), 0644)

	err := verifySnapshotArtifacts(filepath.Join(dir, "nonexistent"), memPath, 512, "Full")
	if err == nil {
		t.Fatal("expected error for missing vm.snap")
	}
}

func TestVerifySnapshotArtifacts_WrongFullMemSize(t *testing.T) {
	dir := t.TempDir()

	vmPath := filepath.Join(dir, "vm.snap")
	os.WriteFile(vmPath, []byte("data"), 0644)

	// Full snapshot but mem.snap is wrong size
	memPath := filepath.Join(dir, "mem.snap")
	os.WriteFile(memPath, make([]byte, 100*1024*1024), 0644) // 100 MiB, expected 512

	err := verifySnapshotArtifacts(vmPath, memPath, 512, "Full")
	if err == nil {
		t.Fatal("expected error for wrong Full mem.snap size")
	}
	if !contains(err.Error(), "size") {
		t.Errorf("expected 'size' in error, got: %v", err)
	}
}

func TestVerifySnapshotArtifacts_EmptyMemSnap(t *testing.T) {
	dir := t.TempDir()

	vmPath := filepath.Join(dir, "vm.snap")
	os.WriteFile(vmPath, []byte("data"), 0644)

	memPath := filepath.Join(dir, "mem.snap")
	os.WriteFile(memPath, []byte{}, 0644) // empty

	err := verifySnapshotArtifacts(vmPath, memPath, 512, "Full")
	if err == nil {
		t.Fatal("expected error for empty mem.snap")
	}
}

func TestValidateSocketPath_OK(t *testing.T) {
	if err := validateSocketPath("/var/lib/bhatti/sandboxes/abc123/fc.sock"); err != nil {
		t.Errorf("expected OK, got: %v", err)
	}
}

func TestValidateSocketPath_TooLong(t *testing.T) {
	long := "/" + string(make([]byte, 110))
	for i := range long {
		if long[i] == 0 {
			long = long[:i] + "a" + long[i+1:]
		}
	}
	err := validateSocketPath(long)
	if err == nil {
		t.Fatal("expected error for path >= 108 bytes")
	}
	if !contains(err.Error(), "too long") {
		t.Errorf("expected 'too long' in error, got: %v", err)
	}
}

func contains(s, sub string) bool {
	return len(s) >= len(sub) && containsStr(s, sub)
}

func containsStr(s, sub string) bool {
	for i := 0; i <= len(s)-len(sub); i++ {
		if s[i:i+len(sub)] == sub {
			return true
		}
	}
	return false
}
