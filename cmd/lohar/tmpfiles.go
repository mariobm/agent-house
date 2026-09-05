//go:build linux

package main

import (
	"fmt"
	"os"
	"os/user"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
)

// applyTmpfiles processes systemd-tmpfiles-style configuration files
// from the search path (lower-priority dirs first; lexically later
// entries win for duplicate paths). Called by runAgent at boot,
// before startEnabledServices, so runtime directories declared by
// packages (e.g. /run/sshd for openssh-server) are materialised
// before the daemons that need them launch.
//
// Real systemd has systemd-tmpfiles(8) doing this work as a separate
// binary invoked at boot. We inline a minimal subset matching the
// directives that real-world packages produce. Format per-line:
//
//	TYPE PATH MODE UID GID AGE ARG
//
// Supported TYPEs (the 80%-coverage cut; deferring binfmt_misc,
// volatile-tmpfile, attribute, ACL, age cleanup, and more exotic
// directives until a real package needs them):
//
//	d     create directory; idempotent
//	D     create directory and empty its contents
//	f     create empty file if missing
//	F     create empty file, truncate if exists
//	L+    create symlink; replace if exists (ARG = target)
//	R     recursively remove contents of directory
//
// Unknown TYPEs are skipped silently. The entire pass is best-effort:
// a bad line never aborts boot, just logs and continues.
//
// Within a search dir, files are loaded alphabetically. Across dirs,
// /usr/lib/tmpfiles.d/ loads first, /run/tmpfiles.d/ next, /etc/
// tmpfiles.d/ last, so admin overrides win. Real systemd uses the
// same precedence.
func applyTmpfiles(dirs []string) {
	// Collect every basename from every dir, mapping basename to its
	// full path. A later dir wins for duplicate basenames -- mirrors
	// systemd's drop-in-style precedence where /etc/ overrides
	// /usr/lib/.
	byName := map[string]string{}
	for _, dir := range dirs {
		entries, _ := os.ReadDir(dir)
		for _, e := range entries {
			if !strings.HasSuffix(e.Name(), ".conf") {
				continue
			}
			byName[e.Name()] = filepath.Join(dir, e.Name())
		}
	}
	names := make([]string, 0, len(byName))
	for n := range byName {
		names = append(names, n)
	}
	sort.Strings(names)

	for _, name := range names {
		applyTmpfilesFile(byName[name])
	}
}

// applyTmpfilesFile parses one .conf file and applies each directive.
// Comment lines (starting with #) and blank lines are skipped, matching
// systemd-tmpfiles' parser.
func applyTmpfilesFile(path string) {
	data, err := os.ReadFile(path)
	if err != nil {
		fmt.Fprintf(os.Stderr, "lohar: tmpfiles read %s: %v\n", path, err)
		return
	}
	for lineno, line := range strings.Split(string(data), "\n") {
		raw := strings.TrimSpace(line)
		if raw == "" || strings.HasPrefix(raw, "#") {
			continue
		}
		if err := applyTmpfilesLine(raw); err != nil {
			fmt.Fprintf(os.Stderr, "lohar: tmpfiles %s:%d: %v\n", path, lineno+1, err)
		}
	}
}

// applyTmpfilesLine handles one TYPE PATH MODE UID GID AGE ARG entry.
// Tokens beyond ARG are folded into ARG (which may legitimately contain
// whitespace, e.g. for L+ symlink targets with spaces).
func applyTmpfilesLine(line string) error {
	fields := strings.Fields(line)
	if len(fields) < 2 {
		return fmt.Errorf("too few fields: %q", line)
	}
	typ := fields[0]
	path := fields[1]
	mode := os.FileMode(0644)
	if len(fields) >= 3 && fields[2] != "-" {
		if m, err := parseTmpfilesMode(fields[2]); err == nil {
			mode = m
		}
	}
	uid, gid := -1, -1
	if len(fields) >= 4 && fields[3] != "-" {
		uid = lookupUser(fields[3])
	}
	if len(fields) >= 5 && fields[4] != "-" {
		gid = lookupGroup(fields[4])
	}
	// fields[5] is AGE (cleanup interval) -- we don't run a cleaner.
	arg := ""
	if len(fields) >= 7 {
		arg = strings.Join(fields[6:], " ")
	}

	switch typ {
	case "d":
		// d: create directory if missing.
		if err := os.MkdirAll(path, mode); err != nil {
			return fmt.Errorf("mkdir %s: %w", path, err)
		}
		os.Chmod(path, mode)
		applyOwnership(path, uid, gid)

	case "D":
		// D: create directory; empty contents if it already exists.
		if err := os.MkdirAll(path, mode); err != nil {
			return fmt.Errorf("mkdir %s: %w", path, err)
		}
		os.Chmod(path, mode)
		applyOwnership(path, uid, gid)
		entries, _ := os.ReadDir(path)
		for _, e := range entries {
			os.RemoveAll(filepath.Join(path, e.Name()))
		}

	case "f":
		// f: create empty file if missing; chmod/chown either way.
		if _, err := os.Stat(path); os.IsNotExist(err) {
			os.MkdirAll(filepath.Dir(path), 0755)
			if err := os.WriteFile(path, []byte{}, mode); err != nil {
				return fmt.Errorf("create %s: %w", path, err)
			}
		}
		os.Chmod(path, mode)
		applyOwnership(path, uid, gid)

	case "F":
		// F: create empty file, truncating if it exists.
		os.MkdirAll(filepath.Dir(path), 0755)
		if err := os.WriteFile(path, []byte{}, mode); err != nil {
			return fmt.Errorf("truncate %s: %w", path, err)
		}
		os.Chmod(path, mode)
		applyOwnership(path, uid, gid)

	case "L+":
		// L+: create symlink, replacing existing target if any.
		// ARG is the symlink's target.
		if arg == "" {
			return fmt.Errorf("L+ requires target argument")
		}
		os.Remove(path)
		if err := os.Symlink(arg, path); err != nil {
			return fmt.Errorf("symlink %s -> %s: %w", path, arg, err)
		}

	case "R":
		// R: recursively remove contents of directory (not the dir
		// itself). Used at boot to clear stale state under /run.
		entries, err := os.ReadDir(path)
		if err != nil {
			return nil // missing dir is not an error for R
		}
		for _, e := range entries {
			os.RemoveAll(filepath.Join(path, e.Name()))
		}

	default:
		// Skip silently for types we don't handle: L (without +),
		// p, c, b, q, Q, z, Z, t, T, h, H, a, A, x, X, r, =, etc.
		// Most aren't used by mainstream packages.
		return nil
	}
	return nil
}

// parseTmpfilesMode parses an octal mode string with optional ~ or :
// prefix (which we ignore -- systemd uses them for mode-of-existing
// vs mode-only-on-create distinctions; we always set the mode).
func parseTmpfilesMode(s string) (os.FileMode, error) {
	s = strings.TrimLeft(s, "~:")
	n, err := strconv.ParseUint(s, 8, 32)
	if err != nil {
		return 0, err
	}
	return os.FileMode(n), nil
}

// lookupUser resolves a UID/username to a uid integer. Numeric IDs are
// returned as-is. Names go through os/user. Returns -1 on lookup
// failure (caller skips the chown).
func lookupUser(s string) int {
	if n, err := strconv.Atoi(s); err == nil {
		return n
	}
	u, err := user.Lookup(s)
	if err != nil {
		return -1
	}
	n, _ := strconv.Atoi(u.Uid)
	return n
}

func lookupGroup(s string) int {
	if n, err := strconv.Atoi(s); err == nil {
		return n
	}
	g, err := user.LookupGroup(s)
	if err != nil {
		return -1
	}
	n, _ := strconv.Atoi(g.Gid)
	return n
}

func applyOwnership(path string, uid, gid int) {
	if uid < 0 && gid < 0 {
		return
	}
	os.Chown(path, uid, gid)
}
