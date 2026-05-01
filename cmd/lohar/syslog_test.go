//go:build linux

package main

import (
	"net"
	"os"
	"path/filepath"
	"testing"
	"time"
)

// sendSyslogMessage writes a syslog datagram to the receiver socket. The
// message format mirrors what libc's syslog(3) produces: a `<priority>`
// prefix, a tag with optional `[pid]`, then `: ` and the message body.
func sendSyslogMessage(t *testing.T, sock, raw string) {
	t.Helper()
	c, err := net.DialUnix("unixgram", nil, &net.UnixAddr{Name: sock, Net: "unixgram"})
	if err != nil {
		t.Fatalf("dial syslog: %v", err)
	}
	defer c.Close()
	if _, err := c.Write([]byte(raw)); err != nil {
		t.Fatalf("write: %v", err)
	}
}

// waitForFile polls until path appears (with non-empty contents) or a timeout
// elapses. The receiver writes to disk in a goroutine, so a small amount of
// async slack is needed before assertions can run.
func waitForFile(t *testing.T, path string) string {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if data, err := os.ReadFile(path); err == nil && len(data) > 0 {
			return string(data)
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatalf("file %s did not appear within timeout", path)
	return ""
}

func TestSyslogTagReconciledToCanonicalUnit(t *testing.T) {
	// The whole point of C5: a daemon that calls libc syslog(3) tags its
	// messages after its binary name (sshd), but the unit it belongs to
	// has a different canonical name (ssh, with Alias=sshd.service in
	// [Install]). Without reconciliation, the daemon's syslog output
	// landed in /var/log/bhatti/sshd.log while svcStart's stdout/stderr
	// capture wrote to /var/log/bhatti/ssh.log \u2014 same daemon, two log
	// files, status -n5 showed the wrong slice.
	//
	// After C5: the receiver looks up the tag in the Unit registry. If
	// it's an alias for a known canonical name, the message lands in the
	// canonical file. So `journalctl -u ssh` and `journalctl -u sshd`
	// both show the full picture.

	dir := t.TempDir()
	logDirSandbox := t.TempDir()
	sock := filepath.Join(t.TempDir(), "log.sock")

	origDirs := serviceDirs
	origLog := logDir
	origSock := syslogSocketPath
	origReg := globalRegistry
	serviceDirs = []string{dir}
	logDir = logDirSandbox
	syslogSocketPath = sock
	globalRegistry = NewRegistry()
	defer func() {
		serviceDirs = origDirs
		logDir = origLog
		syslogSocketPath = origSock
		globalRegistry = origReg
	}()

	// ssh.service with sshd as the alias. This is the real Ubuntu shape.
	os.WriteFile(filepath.Join(dir, "ssh.service"),
		[]byte("[Service]\nExecStart=/usr/sbin/sshd\n[Install]\nAlias=sshd.service\n"),
		0644)

	go startSyslogReceiver()
	// Wait for the socket to appear before sending.
	for i := 0; i < 50; i++ {
		if _, err := os.Stat(sock); err == nil {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}

	// Send a libc-shaped syslog message tagged with the alias.
	sendSyslogMessage(t, sock, "<38>Apr 30 06:50:00 hostname sshd[1234]: Server listening on 0.0.0.0 port 22.")

	// Should land in the CANONICAL log file, not the alias-named one.
	canonical := filepath.Join(logDirSandbox, "ssh.log")
	contents := waitForFile(t, canonical)
	if !contains(contents, "Server listening") {
		t.Errorf("ssh.log doesn't contain expected message: %q", contents)
	}

	// And the alias-named file should NOT exist (we routed past it).
	aliasPath := filepath.Join(logDirSandbox, "sshd.log")
	if _, err := os.Stat(aliasPath); err == nil {
		t.Errorf("sshd.log was created \u2014 syslog reconciliation didn't route to canonical (Fastidious-class regression)")
	}
}

func TestSyslogUnknownTagFallsBack(t *testing.T) {
	// Tags that don't resolve to any unit (kernel, cron, login, custom
	// daemons) keep the legacy behaviour: a tag-keyed file under logDir.
	// This is what makes the receiver useful even for things lohar
	// doesn't manage.
	dir := t.TempDir()
	logDirSandbox := t.TempDir()
	sock := filepath.Join(t.TempDir(), "log2.sock")

	origDirs := serviceDirs
	origLog := logDir
	origSock := syslogSocketPath
	origReg := globalRegistry
	serviceDirs = []string{dir}
	logDir = logDirSandbox
	syslogSocketPath = sock
	globalRegistry = NewRegistry()
	defer func() {
		serviceDirs = origDirs
		logDir = origLog
		syslogSocketPath = origSock
		globalRegistry = origReg
	}()

	go startSyslogReceiver()
	for i := 0; i < 50; i++ {
		if _, err := os.Stat(sock); err == nil {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}

	sendSyslogMessage(t, sock, "<13>Apr 30 06:50:00 hostname kernel: usb 1-1: new high-speed USB device")

	// No ssh.service, no kernel.service \u2014 falls back to kernel.log.
	fallback := filepath.Join(logDirSandbox, "kernel.log")
	contents := waitForFile(t, fallback)
	if !contains(contents, "usb 1-1") {
		t.Errorf("kernel.log unexpected contents: %q", contents)
	}
}

// contains is a tiny helper so we don't need to import strings just for one
// test assertion.
func contains(s, substr string) bool {
	return len(s) >= len(substr) && (func() bool {
		for i := 0; i+len(substr) <= len(s); i++ {
			if s[i:i+len(substr)] == substr {
				return true
			}
		}
		return false
	})()
}
