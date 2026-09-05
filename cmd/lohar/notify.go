//go:build linux

package main

import (
	"bytes"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
)

// startNotifyReceiver binds the sd_notify socket and routes messages to
// the appropriate Unit. Each datagram contains key=value lines:
//
//	READY=1                 <- unit is now actively serving
//	STATUS=loading config   <- free-text status line
//	MAINPID=1234            <- daemon advertising its true main pid
//	WATCHDOG=1              <- liveness ping (deferred; we no-op for now)
//
// Sender attribution uses SO_PASSCRED + SCM_CREDENTIALS to learn the
// sending PID from the kernel (a client-claimed PID would not be
// trustworthy), then walks /proc/<pid>/cgroup to find which Unit's
// cgroup the sender lives in. This depends on F1's cgroup-per-unit:
// every spawned daemon and its descendants live in
// /sys/fs/cgroup/system.slice/<unit>.service/, so the cgroup line in
// /proc/<pid>/cgroup tells us which unit the sender belongs to.
func startNotifyReceiver(reg *Registry) {
	sockPath := reg.Config.NotifySocketPath
	os.MkdirAll(filepath.Dir(sockPath), 0755)
	os.Remove(sockPath)
	conn, err := net.ListenUnixgram("unixgram", &net.UnixAddr{Name: sockPath, Net: "unixgram"})
	if err != nil {
		fmt.Fprintf(os.Stderr, "lohar: notify receiver: %v\n", err)
		return
	}
	// World-writable so daemons running as any uid can send.
	if err := os.Chmod(sockPath, 0666); err != nil {
		fmt.Fprintf(os.Stderr, "lohar: chmod %s: %v\n", sockPath, err)
	}
	// Enable SO_PASSCRED so each recvmsg returns the sender's
	// kernel-vouched (pid, uid, gid) via SCM_CREDENTIALS in the cmsg.
	if rawConn, err := conn.SyscallConn(); err == nil {
		rawConn.Control(func(fd uintptr) {
			syscall.SetsockoptInt(int(fd), syscall.SOL_SOCKET, syscall.SO_PASSCRED, 1)
		})
	}

	buf := make([]byte, 8192)
	oob := make([]byte, 1024)
	for {
		n, oobn, _, _, err := conn.ReadMsgUnix(buf, oob)
		if err != nil {
			continue
		}
		pid := senderPID(oob[:oobn])
		// pid==0 means the kernel didn't give us credentials. Drop the
		// message rather than guess at attribution.
		if pid <= 1 {
			continue
		}
		msg := parseNotifyMessage(buf[:n])
		reg.applyNotify(pid, msg)
	}
}

// senderPID extracts the kernel-vouched sender PID from the cmsg buffer
// of a SO_PASSCRED-enabled recvmsg. Returns 0 if no SCM_CREDENTIALS
// cmsg is present (which shouldn't happen with SO_PASSCRED enabled, but
// defensive).
func senderPID(oob []byte) int {
	cmsgs, err := syscall.ParseSocketControlMessage(oob)
	if err != nil {
		return 0
	}
	for _, cmsg := range cmsgs {
		if cmsg.Header.Level != syscall.SOL_SOCKET || cmsg.Header.Type != syscall.SCM_CREDENTIALS {
			continue
		}
		ucred, err := syscall.ParseUnixCredentials(&cmsg)
		if err != nil {
			return 0
		}
		return int(ucred.Pid)
	}
	return 0
}

// parseNotifyMessage splits a sd_notify datagram into its key=value
// pairs. Multi-line: each line is one assignment. Lines without '=' are
// ignored. Trailing newlines are tolerated. Matches the format daemons
// produce via libsystemd's sd_notify(3).
func parseNotifyMessage(buf []byte) map[string]string {
	out := map[string]string{}
	for _, line := range bytes.Split(buf, []byte("\n")) {
		line = bytes.TrimSpace(line)
		if len(line) == 0 {
			continue
		}
		idx := bytes.IndexByte(line, '=')
		if idx <= 0 {
			continue
		}
		out[string(line[:idx])] = string(line[idx+1:])
	}
	return out
}

// applyNotify processes a notify datagram:
//   - READY=1 -> clear the .activating marker, the unit transitions
//     from activating to active. svcStart's poll loop sees this and
//     returns success.
//   - MAINPID=N -> overwrite the pidfile with N. Some daemons fork or
//     re-exec and want to advertise their true main PID. Watcher (C6)
//     keeps observing the original *os.Process until it exits, but
//     status queries via the pidfile see the right PID for kill -HUP
//     and similar admin operations.
//   - STATUS=... -> deferred; not stored or shown yet.
//   - WATCHDOG=1 -> deferred; matters only when WatchdogSec= is set,
//     which we don't enforce yet.
//
// Drops messages we can't attribute to a known unit (cgroup lookup
// returns nil). That covers daemons started outside the shim's control
// and anything in a cgroup we don't manage.
func (r *Registry) applyNotify(senderPID int, msg map[string]string) {
	u := r.unitForPID(senderPID)
	if u == nil {
		return
	}
	if msg["READY"] == "1" {
		u.ClearActivating()
	}
	if mainPID := msg["MAINPID"]; mainPID != "" {
		if n, err := strconv.Atoi(strings.TrimSpace(mainPID)); err == nil && n > 1 {
			u.WritePID(n)
		}
	}
	// STATUS and WATCHDOG: parsed but not acted on yet. Including them
	// here so we don't error on unrecognised keys; future work just
	// fills in the handlers.
}

// unitForPID returns the Unit owning the cgroup that pid is a member
// of, or nil if pid is outside any unit cgroup we know about.
//
// On cgroup v2 the file format is:
//
//	0::/system.slice/<unit>.service
//	0::/system.slice/<unit>.service/<sub-cgroup>     (e.g. for nested daemons)
//
// We extract the substring after "/system.slice/" up to the next "/"
// (or end of line) and resolve it as a unit name. This handles both
// the direct case and nested cgroups (Docker-in-VM scenarios).
func (r *Registry) unitForPID(pid int) *Unit {
	data, err := os.ReadFile(fmt.Sprintf("/proc/%d/cgroup", pid))
	if err != nil {
		return nil
	}
	return r.unitFromCgroupLine(strings.TrimSpace(string(data)))
}

// unitFromCgroupLine is the parser piece of unitForPID, extracted so it
// can be unit-tested without spawning a real process and reading /proc.
func (r *Registry) unitFromCgroupLine(line string) *Unit {
	// Lines look like: "0::/system.slice/foo.service"
	// or with sub-cgroups: "0::/system.slice/foo.service/sub"
	parts := strings.SplitN(line, ":", 3)
	if len(parts) < 3 {
		return nil
	}
	path := parts[2]
	const sliceMarker = "/system.slice/"
	idx := strings.Index(path, sliceMarker)
	if idx < 0 {
		return nil
	}
	rest := path[idx+len(sliceMarker):]
	// Take up to the next '/' or end. The unit's own cgroup is the
	// directory directly under system.slice; anything deeper is a
	// nested cgroup the unit's daemon may have created itself.
	if end := strings.IndexByte(rest, '/'); end >= 0 {
		rest = rest[:end]
	}
	if rest == "" {
		return nil
	}
	u, err := r.Resolve(rest)
	if err != nil {
		return nil
	}
	return u
}
