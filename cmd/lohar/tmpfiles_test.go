//go:build linux

package main

import (
	"os"
	"path/filepath"
	"testing"
)

func TestApplyTmpfilesCreatesDirectory(t *testing.T) {
	// 'd' directive: create dir if missing, set mode.
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "runtime")
	os.WriteFile(filepath.Join(conf, "00-test.conf"),
		[]byte("d "+target+" 0750 - - -\n"), 0644)

	applyTmpfiles([]string{conf})

	st, err := os.Stat(target)
	if err != nil {
		t.Fatalf("target not created: %v", err)
	}
	if !st.IsDir() {
		t.Errorf("target is not a directory")
	}
	if st.Mode().Perm() != 0750 {
		t.Errorf("mode = %o, want 0750", st.Mode().Perm())
	}
}

func TestApplyTmpfilesIdempotent(t *testing.T) {
	// 'd' on an existing directory should NOT empty it. (That's what 'D'
	// is for.)
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "existing")
	os.MkdirAll(target, 0755)
	os.WriteFile(filepath.Join(target, "keepme.txt"), []byte("important"), 0644)

	os.WriteFile(filepath.Join(conf, "00-test.conf"),
		[]byte("d "+target+" 0755 - - -\n"), 0644)

	applyTmpfiles([]string{conf})

	if _, err := os.Stat(filepath.Join(target, "keepme.txt")); err != nil {
		t.Errorf("'d' directive deleted existing file: %v", err)
	}
}

func TestApplyTmpfilesCapitalDEmptiesDir(t *testing.T) {
	// 'D' on an existing directory should EMPTY it.
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "ephemeral")
	os.MkdirAll(target, 0755)
	os.WriteFile(filepath.Join(target, "stale.pid"), []byte("999"), 0644)

	os.WriteFile(filepath.Join(conf, "00-test.conf"),
		[]byte("D "+target+" 0755 - - -\n"), 0644)

	applyTmpfiles([]string{conf})

	if _, err := os.Stat(filepath.Join(target, "stale.pid")); !os.IsNotExist(err) {
		t.Errorf("'D' directive did not empty dir; stale.pid still present")
	}
	if _, err := os.Stat(target); err != nil {
		t.Errorf("'D' directive removed the dir itself: %v", err)
	}
}

func TestApplyTmpfilesCreatesFile(t *testing.T) {
	// 'f' creates an empty file if missing.
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "marker")
	os.WriteFile(filepath.Join(conf, "00-test.conf"),
		[]byte("f "+target+" 0644 - - -\n"), 0644)

	applyTmpfiles([]string{conf})

	st, err := os.Stat(target)
	if err != nil {
		t.Fatalf("file not created: %v", err)
	}
	if st.Size() != 0 {
		t.Errorf("size = %d, want 0", st.Size())
	}
}

func TestApplyTmpfilesCreateSymlink(t *testing.T) {
	// 'L+' creates a symlink, replacing any existing target.
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "link")
	dest := "/etc/hostname"

	os.WriteFile(filepath.Join(conf, "00-test.conf"),
		[]byte("L+ "+target+" - - - - "+dest+"\n"), 0644)

	applyTmpfiles([]string{conf})

	got, err := os.Readlink(target)
	if err != nil {
		t.Fatalf("symlink not created: %v", err)
	}
	if got != dest {
		t.Errorf("symlink target = %q, want %q", got, dest)
	}
}

func TestApplyTmpfilesRemoveContents(t *testing.T) {
	// 'R' empties a directory but keeps the dir itself.
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "lockdir")
	os.MkdirAll(target, 0755)
	os.WriteFile(filepath.Join(target, "leftover.lock"), []byte{}, 0644)
	os.MkdirAll(filepath.Join(target, "subdir"), 0755)

	os.WriteFile(filepath.Join(conf, "00-test.conf"),
		[]byte("R "+target+" - - - -\n"), 0644)

	applyTmpfiles([]string{conf})

	if _, err := os.Stat(target); err != nil {
		t.Errorf("'R' removed the dir itself: %v", err)
	}
	if _, err := os.Stat(filepath.Join(target, "leftover.lock")); !os.IsNotExist(err) {
		t.Error("'R' did not remove file")
	}
	if _, err := os.Stat(filepath.Join(target, "subdir")); !os.IsNotExist(err) {
		t.Error("'R' did not remove subdir")
	}
}

func TestApplyTmpfilesIgnoresCommentsAndBlanks(t *testing.T) {
	// Real tmpfiles.d files have a header comment and blank lines.
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "real")
	os.WriteFile(filepath.Join(conf, "00-test.conf"), []byte(
		"# This is a comment\n"+
			"\n"+
			"  # Indented comment\n"+
			"d "+target+" 0755 - - -\n"+
			"\n"), 0644)

	applyTmpfiles([]string{conf})

	if _, err := os.Stat(target); err != nil {
		t.Errorf("comments/blanks broke parsing: target not created: %v", err)
	}
}

func TestApplyTmpfilesAlphabeticalOrder(t *testing.T) {
	// Files load alphabetically; later files override earlier ones for
	// the same path. We verify by having two files target the same dir
	// with different modes.
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "ordered")

	os.WriteFile(filepath.Join(conf, "00-first.conf"),
		[]byte("d "+target+" 0700 - - -\n"), 0644)
	os.WriteFile(filepath.Join(conf, "99-last.conf"),
		[]byte("d "+target+" 0755 - - -\n"), 0644)

	applyTmpfiles([]string{conf})

	st, _ := os.Stat(target)
	if st.Mode().Perm() != 0755 {
		t.Errorf("mode = %o, want 0755 (lexically last conf should win)", st.Mode().Perm())
	}
}

func TestApplyTmpfilesDirPriority(t *testing.T) {
	// /etc/-equivalent overrides /usr/lib/-equivalent for the same
	// basename. Verifies the precedence between dirs (lower-priority
	// dirs FIRST in the input slice).
	low := t.TempDir()  // simulates /usr/lib/tmpfiles.d
	high := t.TempDir() // simulates /etc/tmpfiles.d
	target := filepath.Join(t.TempDir(), "priority")

	os.WriteFile(filepath.Join(low, "shared.conf"),
		[]byte("d "+target+" 0700 - - -\n"), 0644)
	os.WriteFile(filepath.Join(high, "shared.conf"),
		[]byte("d "+target+" 0755 - - -\n"), 0644)

	// Lowest-priority first; high-priority last (overrides).
	applyTmpfiles([]string{low, high})

	st, _ := os.Stat(target)
	if st.Mode().Perm() != 0755 {
		t.Errorf("mode = %o, want 0755 (high-priority dir should override low)", st.Mode().Perm())
	}
}

func TestApplyTmpfilesIgnoresUnknownTypes(t *testing.T) {
	// A directive type we don't handle should be silently skipped, not
	// abort the rest of the file. The 'd' on the next line should
	// still run.
	conf := t.TempDir()
	target := filepath.Join(t.TempDir(), "after-unknown")

	os.WriteFile(filepath.Join(conf, "00-test.conf"),
		[]byte("p /run/some-fifo 0644 - - -\n"+
			"d "+target+" 0755 - - -\n"), 0644)

	applyTmpfiles([]string{conf})

	if _, err := os.Stat(target); err != nil {
		t.Errorf("unknown 'p' directive aborted the file; subsequent 'd' didn't run")
	}
}

func TestParseTmpfilesMode(t *testing.T) {
	cases := []struct {
		in   string
		want os.FileMode
	}{
		{"0644", 0644},
		{"0755", 0755},
		{"~0750", 0750}, // ~ prefix stripped
		{":0700", 0700}, // : prefix stripped
		{"644", 0644},   // bare octal
	}
	for _, c := range cases {
		got, err := parseTmpfilesMode(c.in)
		if err != nil {
			t.Errorf("parseTmpfilesMode(%q): %v", c.in, err)
			continue
		}
		if got != c.want {
			t.Errorf("parseTmpfilesMode(%q) = %o, want %o", c.in, got, c.want)
		}
	}
}
