//go:build linux

package firecracker

import (
	"os"
	"path/filepath"
	"testing"
)

func TestStateStr(t *testing.T) {
	m := map[string]interface{}{
		"present": "hello",
		"number":  42,
	}
	if got := stateStr(m, "present"); got != "hello" {
		t.Errorf("present: %q, want 'hello'", got)
	}
	if got := stateStr(m, "missing"); got != "" {
		t.Errorf("missing: %q, want ''", got)
	}
	if got := stateStr(m, "number"); got != "" {
		t.Errorf("wrong type: %q, want ''", got)
	}
}

func TestStateInt64(t *testing.T) {
	cases := []struct {
		name string
		val  interface{}
		want int64
	}{
		{"int", int(42), 42},
		{"int64", int64(99), 99},
		{"float64", float64(3.14), 3},  // truncates
		{"uint32", uint32(7), 7},
		{"nil", nil, 0},
		{"string", "nope", 0},
	}
	for _, tc := range cases {
		m := map[string]interface{}{"k": tc.val}
		if got := stateInt64(m, "k"); got != tc.want {
			t.Errorf("%s: %d, want %d", tc.name, got, tc.want)
		}
	}
	// Missing key
	if got := stateInt64(map[string]interface{}{}, "k"); got != 0 {
		t.Errorf("missing: %d, want 0", got)
	}
}

func TestStateUint32(t *testing.T) {
	cases := []struct {
		name string
		val  interface{}
		want uint32
	}{
		{"int", int(42), 42},
		{"int64", int64(99), 99},
		{"float64", float64(256.9), 256},
		{"uint32", uint32(7), 7},
		{"nil", nil, 0},
	}
	for _, tc := range cases {
		m := map[string]interface{}{"k": tc.val}
		if got := stateUint32(m, "k"); got != tc.want {
			t.Errorf("%s: %d, want %d", tc.name, got, tc.want)
		}
	}
}

func TestStateBool(t *testing.T) {
	cases := []struct {
		name string
		val  interface{}
		want bool
	}{
		{"true", true, true},
		{"false", false, false},
		{"int 1", int(1), true},
		{"int 0", int(0), false},
		{"float64 1", float64(1), true},
		{"float64 0", float64(0), false},
		{"nil", nil, false},
		{"string", "true", false}, // strings not supported
	}
	for _, tc := range cases {
		m := map[string]interface{}{"k": tc.val}
		if got := stateBool(m, "k"); got != tc.want {
			t.Errorf("%s: %v, want %v", tc.name, got, tc.want)
		}
	}
	// Missing key
	if got := stateBool(map[string]interface{}{}, "k"); got != false {
		t.Errorf("missing: %v, want false", got)
	}
}

// --- sha256File ---

func TestSha256File(t *testing.T) {
	dir := t.TempDir()

	// Normal file — should produce a 64-char hex hash.
	path := filepath.Join(dir, "hello.bin")
	os.WriteFile(path, []byte("hello world"), 0644)
	h := sha256File(path)
	if len(h) != 64 {
		t.Fatalf("expected 64-char hex hash, got %q (%d)", h, len(h))
	}

	// Same content → same hash (deterministic).
	path2 := filepath.Join(dir, "hello2.bin")
	os.WriteFile(path2, []byte("hello world"), 0644)
	if sha256File(path2) != h {
		t.Error("identical content should produce identical hash")
	}

	// Different content → different hash.
	path3 := filepath.Join(dir, "other.bin")
	os.WriteFile(path3, []byte("other"), 0644)
	if sha256File(path3) == h {
		t.Error("different content should produce different hash")
	}
}

func TestSha256File_Missing(t *testing.T) {
	if got := sha256File("/nonexistent/path"); got != "" {
		t.Errorf("missing file should return empty, got %q", got)
	}
}

// --- loharNeedsInjection ---

func TestLoharNeedsInjection_EmptyHash(t *testing.T) {
	// Empty cached hash means no lohar binary (dev mode) — never inject.
	if loharNeedsInjection("/any/image.ext4", "/any/datadir", "") {
		t.Error("empty cachedHash should return false (dev mode)")
	}
}

func TestLoharNeedsInjection_NoStamp(t *testing.T) {
	dir := t.TempDir()
	image := filepath.Join(dir, "rootfs.ext4")
	os.WriteFile(image, []byte("fake"), 0644)

	// No stamp file exists → needs injection.
	if !loharNeedsInjection(image, dir, "abc123") {
		t.Error("missing stamp should require injection")
	}
}

func TestLoharNeedsInjection_StampMatches(t *testing.T) {
	dir := t.TempDir()
	image := filepath.Join(dir, "rootfs.ext4")
	os.WriteFile(image, []byte("fake"), 0644)

	hash := "deadbeef1234567890abcdef"
	os.WriteFile(image+".lohar-sha256", []byte(hash+"\n"), 0644)

	if loharNeedsInjection(image, dir, hash) {
		t.Error("matching stamp should skip injection")
	}
}

func TestLoharNeedsInjection_StampMismatch(t *testing.T) {
	dir := t.TempDir()
	image := filepath.Join(dir, "rootfs.ext4")
	os.WriteFile(image, []byte("fake"), 0644)

	os.WriteFile(image+".lohar-sha256", []byte("old-hash\n"), 0644)

	if !loharNeedsInjection(image, dir, "new-hash") {
		t.Error("mismatched stamp should require injection")
	}
}

// --- ensureImagesHaveCurrentLohar ---

func TestEnsureImagesHaveCurrentLohar_NoLohar(t *testing.T) {
	dir := t.TempDir()
	// No lohar binary → return empty hash.
	if got := ensureImagesHaveCurrentLohar(dir); got != "" {
		t.Errorf("no lohar binary should return empty, got %q", got)
	}
}

func TestEnsureImagesHaveCurrentLohar_NoImagesDir(t *testing.T) {
	dir := t.TempDir()
	// Write a lohar binary but no images/ subdir.
	os.WriteFile(filepath.Join(dir, "lohar"), []byte("fake-lohar"), 0755)

	got := ensureImagesHaveCurrentLohar(dir)
	if got == "" {
		t.Error("should return the lohar hash even without images/ dir")
	}
	// Verify it's a valid SHA-256 hex string.
	if len(got) != 64 {
		t.Errorf("expected 64-char hex hash, got %q (%d)", got, len(got))
	}
}
