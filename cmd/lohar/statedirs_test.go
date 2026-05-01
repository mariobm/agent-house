//go:build linux

package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// stateDirsTestUnit builds a synthetic Unit with the given Service
// directives. Avoids the round-trip through Resolve so the test
// doesn't depend on a Registry; all that matters is u.Sections.
func stateDirsTestUnit(directives map[string]string) *Unit {
	kvs := []kvPair{}
	for k, v := range directives {
		kvs = append(kvs, kvPair{key: k, value: v})
	}
	return &Unit{
		Sections: serviceFile{sections: map[string][]kvPair{
			"Service": kvs,
		}},
	}
}

// testBases sandboxes the four base directories under tempdirs.
func testBases(t *testing.T) map[string]string {
	t.Helper()
	state := t.TempDir()
	cache := t.TempDir()
	logs := t.TempDir()
	conf := t.TempDir()
	return map[string]string{
		"StateDirectory":         state,
		"CacheDirectory":         cache,
		"LogsDirectory":          logs,
		"ConfigurationDirectory": conf,
	}
}

func TestApplyStateDirectoriesCreatesDirs(t *testing.T) {
	bases := testBases(t)
	u := stateDirsTestUnit(map[string]string{
		"StateDirectory":         "myservice",
		"CacheDirectory":         "myservice",
		"LogsDirectory":          "myservice",
		"ConfigurationDirectory": "myservice",
	})

	if err := u.ApplyStateDirectoriesIn(bases); err != nil {
		t.Fatalf("ApplyStateDirectoriesIn: %v", err)
	}

	for directive, base := range bases {
		path := filepath.Join(base, "myservice")
		st, err := os.Stat(path)
		if err != nil {
			t.Errorf("%s: dir not created at %s: %v", directive, path, err)
			continue
		}
		if !st.IsDir() {
			t.Errorf("%s: %s is not a directory", directive, path)
		}
	}
}

func TestApplyStateDirectoriesMultipleNamesOnOneLine(t *testing.T) {
	// systemd allows multiple dirs on one line:
	//   StateDirectory=foo bar baz
	bases := testBases(t)
	u := stateDirsTestUnit(map[string]string{
		"StateDirectory": "foo bar baz",
	})

	if err := u.ApplyStateDirectoriesIn(bases); err != nil {
		t.Fatalf("ApplyStateDirectoriesIn: %v", err)
	}

	for _, name := range []string{"foo", "bar", "baz"} {
		path := filepath.Join(bases["StateDirectory"], name)
		if _, err := os.Stat(path); err != nil {
			t.Errorf("dir %s not created: %v", path, err)
		}
	}
}

func TestApplyStateDirectoriesRespectMode(t *testing.T) {
	// StateDirectoryMode=0700 controls the mode of all StateDirectory=
	// dirs. Default is 0755.
	bases := testBases(t)
	u := stateDirsTestUnit(map[string]string{
		"StateDirectory":     "secret",
		"StateDirectoryMode": "0700",
	})

	if err := u.ApplyStateDirectoriesIn(bases); err != nil {
		t.Fatalf("ApplyStateDirectoriesIn: %v", err)
	}

	st, _ := os.Stat(filepath.Join(bases["StateDirectory"], "secret"))
	if st.Mode().Perm() != 0700 {
		t.Errorf("mode = %o, want 0700", st.Mode().Perm())
	}
}

func TestApplyStateDirectoriesIdempotent(t *testing.T) {
	// Calling twice on the same unit should not fail.
	bases := testBases(t)
	u := stateDirsTestUnit(map[string]string{
		"StateDirectory": "twice",
	})
	for i := 0; i < 2; i++ {
		if err := u.ApplyStateDirectoriesIn(bases); err != nil {
			t.Fatalf("call %d: %v", i, err)
		}
	}
}

func TestApplyStateDirectoriesRejectsPathTraversal(t *testing.T) {
	// Unit-file bug or malicious input: '/' or '..' in the name should
	// be rejected, not allowed to write outside the base dir. Note that
	// an entirely-empty value (StateDirectory= or StateDirectory=" ")
	// is NOT an error -- strings.Fields filters those out before
	// validation; matches systemd's no-op-on-empty semantics.
	bases := testBases(t)
	cases := []string{
		"../escape",
		"foo/bar",
		"..",
		".",
	}
	for _, name := range cases {
		u := stateDirsTestUnit(map[string]string{
			"StateDirectory": name,
		})
		err := u.ApplyStateDirectoriesIn(bases)
		if err == nil {
			t.Errorf("StateDirectory=%q: should have errored, got nil", name)
		}
	}
}

func TestApplyStateDirectoriesEmptyValueIsNoOp(t *testing.T) {
	// StateDirectory= or StateDirectory="   " with no actual names
	// is a no-op, not an error. systemd uses the empty form as a
	// reset of accumulated values (matches drop-in semantics).
	bases := testBases(t)
	for _, value := range []string{"", "   ", "\t\n"} {
		u := stateDirsTestUnit(map[string]string{
			"StateDirectory": value,
		})
		if err := u.ApplyStateDirectoriesIn(bases); err != nil {
			t.Errorf("StateDirectory=%q should be no-op, got %v", value, err)
		}
	}
}

func TestApplyStateDirectoriesEmptyDirectivesNoOp(t *testing.T) {
	// A unit with no StateDirectory= etc. should succeed without
	// creating anything.
	bases := testBases(t)
	u := stateDirsTestUnit(map[string]string{
		"ExecStart": "/bin/true",
	})

	if err := u.ApplyStateDirectoriesIn(bases); err != nil {
		t.Fatalf("ApplyStateDirectoriesIn: %v", err)
	}

	for _, base := range bases {
		entries, _ := os.ReadDir(base)
		if len(entries) > 0 {
			t.Errorf("base %s should be empty, got %d entries", base, len(entries))
		}
	}
}

func TestApplyStateDirectoriesMultipleDirectiveLines(t *testing.T) {
	// systemd accumulates multiple StateDirectory= lines. We use a
	// directly-constructed Sections map to simulate this since the
	// stateDirsTestUnit helper takes a single value per key.
	bases := testBases(t)
	u := &Unit{
		Sections: serviceFile{sections: map[string][]kvPair{
			"Service": {
				{key: "StateDirectory", value: "first"},
				{key: "StateDirectory", value: "second third"},
			},
		}},
	}

	if err := u.ApplyStateDirectoriesIn(bases); err != nil {
		t.Fatalf("ApplyStateDirectoriesIn: %v", err)
	}

	for _, name := range []string{"first", "second", "third"} {
		path := filepath.Join(bases["StateDirectory"], name)
		if _, err := os.Stat(path); err != nil {
			t.Errorf("dir %s not created: %v", path, err)
		}
	}
}

func TestParseDirMode(t *testing.T) {
	cases := []struct {
		in   string
		def  os.FileMode
		want os.FileMode
	}{
		{"0755", 0644, 0755},
		{"0700", 0644, 0700},
		{"", 0644, 0644},
		{"  ", 0644, 0644},
		{"garbage", 0755, 0755},
	}
	for _, c := range cases {
		got := parseDirMode(c.in, c.def)
		if got != c.want {
			t.Errorf("parseDirMode(%q, %o) = %o, want %o", c.in, c.def, got, c.want)
		}
	}
}

func TestValidateStateDirName(t *testing.T) {
	good := []string{"foo", "my-service", "service123", "with.dots"}
	bad := []string{"", "/abs", "rel/path", ".", ".."}

	for _, n := range good {
		if err := validateStateDirName(n); err != nil {
			t.Errorf("%q should be valid: %v", n, err)
		}
	}
	for _, n := range bad {
		err := validateStateDirName(n)
		if err == nil {
			t.Errorf("%q should be invalid", n)
			continue
		}
		// Make sure the error message is informative.
		if !strings.Contains(err.Error(), "basename") &&
			!strings.Contains(err.Error(), "empty") &&
			!strings.Contains(err.Error(), "dotfile") {
			t.Errorf("%q error %q lacks reason", n, err)
		}
	}
}
