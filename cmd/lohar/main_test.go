//go:build linux

package main

import (
	"os"
	"testing"
)

// TestMain wires the spawn-helper indirection so existing startDaemon
// tests (TestRestartOnFailure, TestStopSuppressesRestart, the cgroup
// integration tests, etc.) work in a test binary.
//
// The problem this solves: startDaemon spawns a subprocess via
// `/proc/self/exe spawn ...`. In production /proc/self/exe is lohar's
// binary and main() dispatches argv[1] to runSpawn. In a test binary,
// /proc/self/exe is the test binary, whose main is testing.M.Run \u2014
// the argv-verb dispatch in cmd/lohar/main.go is never reached, and
// the forked child re-runs the whole test suite, recursively forking
// itself until the test timeout fires.
//
// Two responsibilities here:
//
//  1. Subprocess dispatch. When this test binary is exec'd by a parent
//     test process with LOHAR_SPAWN_HELPER=1 in the env, route directly
//     to runSpawn before any test runs. This mirrors the argv[1]=spawn
//     dispatch in production's main().
//
//  2. Parent configuration. In the parent test process (no env var),
//     point startDaemon at the test binary itself (os.Args[0]) and tell
//     it to set LOHAR_SPAWN_HELPER=1 on the subprocess env, so the
//     subprocess hits branch (1).
//
// The "--helper-args" sentinel separates go test's own flags from the
// args we want runSpawn to see. Without it, "--cgroup" and the daemon
// argv would collide with the testing package's flag parser.
func TestMain(m *testing.M) {
	if os.Getenv("LOHAR_SPAWN_HELPER") == "1" {
		// Subprocess role. Find the "--helper-args" sentinel and pass
		// everything after it to runSpawn. runSpawn either Execs into
		// the daemon (PID preserved, never returns) or calls os.Exit
		// on failure; the os.Exit(99) below is a defensive marker for
		// "spawn unexpectedly returned" \u2014 should never fire.
		args := os.Args[1:]
		for i, a := range args {
			if a == "--helper-args" {
				args = args[i+1:]
				break
			}
		}
		runSpawn(args)
		os.Exit(99)
	}

	// Parent role: tell startDaemon to invoke us-as-helper.
	spawnHelperPath = os.Args[0]
	spawnHelperPrefix = []string{"--helper-args"}
	spawnHelperEnv = []string{"LOHAR_SPAWN_HELPER=1"}

	os.Exit(m.Run())
}
