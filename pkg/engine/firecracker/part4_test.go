//go:build linux

package firecracker

import (
	"strings"
	"testing"
)

func TestExecRunsAsLohar(t *testing.T) {
	eng := testEngine(t)
	info, err := eng.Create(t.Context(), testSpec("whoami-test"))
	if err != nil {
		t.Fatal(err)
	}
	defer eng.Destroy(t.Context(), info.ID)

	r, err := execWithTimeout(t, eng, info.ID, []string{"whoami"})
	if err != nil {
		t.Fatal(err)
	}
	who := strings.TrimSpace(r.Stdout)
	if who != "lohar" {
		t.Fatalf("whoami = %q, want lohar", who)
	}
	t.Logf("exec runs as %s", who)
}

func TestExecCanSudo(t *testing.T) {
	eng := testEngine(t)
	info, err := eng.Create(t.Context(), testSpec("sudo-test"))
	if err != nil {
		t.Fatal(err)
	}
	defer eng.Destroy(t.Context(), info.ID)

	r, err := execWithTimeout(t, eng, info.ID, []string{"sudo", "whoami"})
	if err != nil {
		t.Fatal(err)
	}
	who := strings.TrimSpace(r.Stdout)
	if who != "root" {
		t.Fatalf("sudo whoami = %q, want root", who)
	}
	t.Logf("sudo works: %s", who)
}

func TestConfigDriveUnmounted(t *testing.T) {
	eng := testEngine(t)
	info, err := eng.Create(t.Context(), testSpec("config-test"))
	if err != nil {
		t.Fatal(err)
	}
	defer eng.Destroy(t.Context(), info.ID)

	r, _ := execWithTimeout(t, eng, info.ID, []string{"cat", "/run/bhatti/config/config.json"})
	if r.ExitCode == 0 {
		t.Fatal("config drive should not be readable after boot")
	}
	t.Logf("config drive unmounted (exit=%d)", r.ExitCode)
}
