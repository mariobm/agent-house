//go:build linux

package main

import (
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// stateDirBases is the production location for each unit-private
// runtime directory category. systemd's documented defaults; we match
// them. Tests inject their own via ApplyStateDirectoriesIn().
//
// We deliberately don't promote these to Config because the paths are
// systemd-spec, not deployment configuration. Real ssh.service has
// StateDirectory=ssh expecting /var/lib/ssh -- if we changed the base,
// we'd diverge from systemd's contract.
var stateDirBases = map[string]string{
	"StateDirectory":         "/var/lib",
	"CacheDirectory":         "/var/cache",
	"LogsDirectory":          "/var/log",
	"ConfigurationDirectory": "/etc",
}

// ApplyStateDirectories creates the per-unit runtime directories
// declared by StateDirectory= / CacheDirectory= / LogsDirectory= /
// ConfigurationDirectory= and chowns them to the unit's User=/Group=.
//
// systemd creates these implicitly at startup so service code can
// assume they exist. Without this, a unit declaring StateDirectory=foo
// (assuming systemd will create /var/lib/foo) gets a permission-denied
// or no-such-file error opening it -- common in newer services and in
// custom units users write themselves.
//
// Each directive's value is whitespace-separated so a single line can
// declare multiple dirs:
//
//	StateDirectory=foo bar baz
//
// produces /var/lib/foo, /var/lib/bar, /var/lib/baz. Multiple lines
// also accumulate (handled via Sections.getAll).
//
// Path traversal: a directive value containing '/' is rejected. systemd
// requires the value to be a basename to prevent unit files from
// reaching outside the documented directory categories.
//
// Mode: defaults to 0755 unless <Type>Mode= overrides (e.g.
// StateDirectoryMode=0700). Mode is applied on every call (idempotent).
//
// Ownership: User=/Group= from [Service] decide chown. Defaults skip
// chown (leave the dir owned by lohar/root, which matters for daemons
// that don't drop privileges).
func (u *Unit) ApplyStateDirectories() error {
	return u.ApplyStateDirectoriesIn(stateDirBases)
}

// ApplyStateDirectoriesIn is the test seam: takes an explicit map of
// directive-name -> base-path so tests can sandbox.
func (u *Unit) ApplyStateDirectoriesIn(bases map[string]string) error {
	svc := u.Sections
	user := svc.get("Service", "User")
	group := svc.get("Service", "Group")

	for directive, base := range bases {
		mode := parseDirMode(svc.get("Service", directive+"Mode"), 0755)
		for _, raw := range svc.getAll("Service", directive) {
			for _, name := range strings.Fields(raw) {
				if err := validateStateDirName(name); err != nil {
					return fmt.Errorf("%s=%q: %w", directive, name, err)
				}
				path := filepath.Join(base, name)
				if err := os.MkdirAll(path, mode); err != nil {
					return fmt.Errorf("%s=%s: mkdir %s: %w", directive, name, path, err)
				}
				// Chmod separately because MkdirAll ignores the mode if
				// the dir already exists.
				os.Chmod(path, mode)
				if user != "" || group != "" {
					applyUserGroup(path, user, group)
				}
			}
		}
	}
	return nil
}

// validateStateDirName rejects values that would escape the base dir.
// systemd requires a basename (no '/'); a value like "../etc/passwd"
// would otherwise let a malicious unit file write outside the intended
// category.
func validateStateDirName(name string) error {
	if name == "" {
		return fmt.Errorf("empty value")
	}
	if strings.Contains(name, "/") {
		return fmt.Errorf("must be a basename (no '/')")
	}
	if name == "." || name == ".." {
		return fmt.Errorf("dotfile reference rejected")
	}
	return nil
}

// parseDirMode parses an octal mode string. Returns the default if
// empty or unparseable -- matches the permissive parser systemd uses
// for unit files (a malformed mode is a unit-file bug, not a fail).
func parseDirMode(s string, def os.FileMode) os.FileMode {
	s = strings.TrimSpace(s)
	if s == "" {
		return def
	}
	n, err := strconv.ParseUint(s, 8, 32)
	if err != nil {
		return def
	}
	return os.FileMode(n)
}

// applyUserGroup chowns path to the named user/group. Names go through
// os/user lookup; numeric IDs are parsed directly. A lookup failure is
// silently ignored -- the user/group might be created later by the
// unit's ExecStartPre, and we'd rather have a wrong-owner directory
// that the daemon can fix than fail to start over a name.
func applyUserGroup(path, user, group string) {
	uid := -1
	if user != "" {
		uid = lookupUser(user)
	}
	gid := -1
	if group != "" {
		gid = lookupGroup(group)
	}
	if uid >= 0 || gid >= 0 {
		os.Chown(path, uid, gid)
	}
}
