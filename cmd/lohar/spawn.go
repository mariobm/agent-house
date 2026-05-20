//go:build linux

package main

import (
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"
	"syscall"
)

// runSpawn is the `lohar spawn` helper. It does exactly two things and
// then disappears:
//
//  1. Write its own PID into <cgroup>/cgroup.procs.
//  2. execve(2) into the daemon command.
//
// Doing the cgroup write here, *before* the execve to the daemon binary,
// closes the cgroup-placement race that exists when the supervisor moves
// the PID after cmd.Start() returns. Any descendant the daemon forks
// later (X servers' daemon(3) detach, dbus pre-fork workers, classic
// Apache, etc.) inherits the unit's cgroup, so stop-time cgroup.kill
// catches all of them.
//
// Two kernel invariants do all the work:
//
//   - cgroup.procs write is observed by the kernel before any descendant
//     of the writer is born. We write at the very top of runSpawn; the
//     only Go-side work before it is argv parsing (no fork(2) calls,
//     no os/exec, no goroutines).
//
//   - execve does not change cgroup membership. Cgroup membership is a
//     property of the process, not its loaded image. After syscall.Exec
//     into the daemon (commonly via /bin/sh as an intermediate), the
//     PID — same throughout the chain — is still in the unit's cgroup.
//
// Real systemd uses clone3(CLONE_INTO_CGROUP) to do this atomically in
// one syscall. Exposing clone3 from Go requires reproducing what
// syscall.ForkExec does post-fork (close-on-exec, signal-handler reset,
// credentials, working dir) under syscall.ForkLock — real surgery on a
// piece of code where bugs are silent and expensive. The helper achieves
// the same correctness with plain Go.
//
// Invocation:
//
//	/proc/self/exe spawn --cgroup <path> -- <argv...>
//
// Dispatched from main() by argv[1] verb. That's a different axis than
// the busybox-style argv[0] basename dispatch used for systemctl and
// journalctl: `spawn` is a private supervisor primitive, not a user-
// facing verb, and is deliberately not symlinked into PATH. The verb
// dispatch keeps it discoverable only via the source.
//
// Implementation notes for the race window between cgroup-write and
// execve:
//
//   - No `defer` statements in the write→exec span.
//   - No heap allocations (no fmt.Errorf, no log lines on success).
//   - No other file descriptors opened.
//
// In practice the helper is: parse argv → os.WriteFile → check err →
// syscall.Exec. The window is one syscall wide on the happy path.
func runSpawn(args []string) {
	cgroupPath, argv, err := parseSpawnArgs(args)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		fmt.Fprintln(os.Stderr, "usage: lohar spawn --cgroup <path> -- <argv...>")
		os.Exit(2)
	}

	// Pre-format the PID into a small allocation outside the write→exec
	// span. Keeps that span as tight as possible.
	pid := strconv.Itoa(os.Getpid())
	procs := cgroupPath + "/cgroup.procs"

	// --- start of race-sensitive span ---
	if err := os.WriteFile(procs, []byte(pid), 0644); err != nil {
		fmt.Fprintf(os.Stderr, "lohar spawn: write %s: %v\n", procs, err)
		os.Exit(1)
	}
	if err := syscall.Exec(argv[0], argv, os.Environ()); err != nil {
		// syscall.Exec only returns on failure (e.g. argv[0] missing or
		// not executable). On success the process image is replaced and
		// this code is unreachable.
		fmt.Fprintf(os.Stderr, "lohar spawn: exec %s: %v\n", argv[0], err)
		os.Exit(1)
	}
	// --- end of race-sensitive span ---
}

// parseSpawnArgs extracts --cgroup <path> and the post-`--` argv from
// args. Returns errors suitable for printing to stderr alongside the
// usage line.
//
// Acceptable forms:
//
//	--cgroup <path> -- <argv...>
//	--cgroup=<path> -- <argv...>
//
// The `--` separator is required even though it's commonly optional in
// flag parsers: without it, we can't disambiguate the daemon's argv
// when the daemon's first arg starts with `-` (e.g.
// `Xkasmvnc -ac -dontvtswitch ...`). Requiring `--` keeps the contract
// unambiguous.
func parseSpawnArgs(args []string) (cgroupPath string, argv []string, err error) {
	const cgEq = "--cgroup="

	i := 0
	sawCgroup := false
	for i < len(args) {
		a := args[i]
		switch {
		case a == "--":
			if !sawCgroup {
				return "", nil, errors.New("lohar spawn: --cgroup is required")
			}
			argv = args[i+1:]
			if len(argv) == 0 {
				return "", nil, errors.New("lohar spawn: empty argv after --")
			}
			return cgroupPath, argv, nil
		case a == "--cgroup":
			if i+1 >= len(args) {
				return "", nil, errors.New("lohar spawn: --cgroup requires a value")
			}
			cgroupPath = args[i+1]
			sawCgroup = true
			i += 2
		case strings.HasPrefix(a, cgEq):
			cgroupPath = a[len(cgEq):]
			if cgroupPath == "" {
				return "", nil, errors.New("lohar spawn: --cgroup requires a value")
			}
			sawCgroup = true
			i++
		default:
			return "", nil, fmt.Errorf("lohar spawn: unexpected argument %q (use '--' before daemon argv)", a)
		}
	}
	return "", nil, errors.New("lohar spawn: missing '--' separator before daemon argv")
}
