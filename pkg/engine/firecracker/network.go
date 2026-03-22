//go:build linux

package firecracker

import (
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"strings"
	"sync"
)

// UserNetwork holds the network state for a single user.
// Each user gets their own bridge and /24 subnet for L2 isolation.
type UserNetwork struct {
	BridgeName string
	GatewayIP  string // e.g. "10.0.1.1"
	Subnet     string // e.g. "10.0.1.0/24"
	Pool       *ipPool
}

// subnetFromIndex converts a 1-based subnet index to network parameters.
//
//	index 1   → 10.0.1.0/24,   gateway 10.0.1.1,   bridge brbhatti-1
//	index 254 → 10.0.254.0/24, gateway 10.0.254.1, bridge brbhatti-254
//	index 255 → 10.1.0.0/24,   gateway 10.1.0.1,   bridge brbhatti-255
//	max: 10.255.254.0/24 → 65,024 users, 253 VMs each
func subnetFromIndex(index int) (gateway, subnet, bridge string) {
	hi := (index - 1) / 254
	lo := ((index - 1) % 254) + 1
	gateway = fmt.Sprintf("10.%d.%d.1", hi, lo)
	subnet = fmt.Sprintf("10.%d.%d.0/24", hi, lo)
	bridge = fmt.Sprintf("brbhatti-%d", index)
	return
}

// ipPool manages IP allocation within a /24 subnet.
// Usable range: .2 through .254 (253 addresses).
// .0 = network, .1 = bridge/gateway, .255 = broadcast.
type ipPool struct {
	mu      sync.Mutex
	used    [256]bool
	gateway string // e.g. "10.0.1.1" — needed to format full IPs
	prefix  string // e.g. "10.0.1." — for formatting
}

func newIPPool(gateway string) *ipPool {
	// Extract prefix: "10.0.1.1" → "10.0.1."
	lastDot := strings.LastIndex(gateway, ".")
	prefix := gateway[:lastDot+1]

	p := &ipPool{gateway: gateway, prefix: prefix}
	p.used[0] = true   // network
	p.used[1] = true   // gateway
	p.used[255] = true // broadcast
	return p
}

// Allocate returns the next free IP in the subnet.
func (p *ipPool) Allocate() (string, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for i := 2; i < 255; i++ {
		if !p.used[i] {
			p.used[i] = true
			return fmt.Sprintf("%s%d", p.prefix, i), nil
		}
	}
	return "", fmt.Errorf("IP pool exhausted (253 VMs per user)")
}

// Release frees an IP back to the pool.
func (p *ipPool) Release(ip string) {
	var octet int
	fmt.Sscanf(ip, p.prefix+"%d", &octet)
	if octet < 2 || octet > 254 {
		return
	}
	p.mu.Lock()
	p.used[octet] = false
	p.mu.Unlock()
}

// Mark reserves an IP (used during startup recovery).
func (p *ipPool) Mark(ip string) {
	var octet int
	fmt.Sscanf(ip, p.prefix+"%d", &octet)
	if octet < 2 || octet > 254 {
		return
	}
	p.mu.Lock()
	p.used[octet] = true
	p.mu.Unlock()
}

// ensureUserBridge creates a user's bridge if it doesn't exist.
// Idempotent — safe to call on every sandbox creation.
func ensureUserBridge(net *UserNetwork) error {
	runQuiet("ip", "link", "add", net.BridgeName, "type", "bridge")
	runQuiet("ip", "addr", "add", net.GatewayIP+"/24", "dev", net.BridgeName)
	if err := run("ip", "link", "set", net.BridgeName, "up"); err != nil {
		return fmt.Errorf("bring up bridge %s: %w", net.BridgeName, err)
	}
	return nil
}

// destroyUserBridge removes a user's bridge device.
func destroyUserBridge(bridgeName string) {
	run("ip", "link", "del", bridgeName)
}

// setupGlobalFirewall configures isolation rules for all VM traffic.
// Called once from Engine.New(). Idempotent. 6 rules total regardless
// of user/VM count.
func setupGlobalFirewall() error {
	defaultIface := detectDefaultInterface()

	rules := []struct {
		table string
		chain string
		args  []string
	}{
		// 1. Block cross-bridge routing (user A's VMs cannot reach user B's VMs).
		// Same-bridge traffic stays at L2 (switched, never enters FORWARD).
		{"filter", "FORWARD", []string{"-s", "10.0.0.0/8", "-d", "10.0.0.0/8", "-j", "DROP"}},

		// 2. Allow VM → internet
		{"filter", "FORWARD", []string{"-s", "10.0.0.0/8", "!", "-d", "10.0.0.0/8", "-j", "ACCEPT"}},

		// 3. Allow return traffic from internet → VM
		{"filter", "FORWARD", []string{"-d", "10.0.0.0/8", "-m", "state",
			"--state", "RELATED,ESTABLISHED", "-j", "ACCEPT"}},

		// 4. Allow return traffic from VMs to host (agent TCP responses).
		// The bhatti daemon initiates TCP connections to VMs. The SYN-ACK
		// enters INPUT with source 10.0.0.0/8. Without this rule, rule 5
		// would kill all agent connections.
		// MUST come before rule 5.
		{"filter", "INPUT", []string{"-s", "10.0.0.0/8", "-m", "state",
			"--state", "RELATED,ESTABLISHED", "-j", "ACCEPT"}},

		// 5. Block VM-initiated connections to host (API, SSH, everything).
		// Only NEW connections are dropped. Prevents compromised VMs from
		// reaching the bhatti API or SSH.
		{"filter", "INPUT", []string{"-s", "10.0.0.0/8", "-m", "state",
			"--state", "NEW", "-j", "DROP"}},

		// 6. NAT for outbound
		{"nat", "POSTROUTING", []string{"-s", "10.0.0.0/8", "-o", defaultIface,
			"-j", "MASQUERADE"}},
	}

	for _, r := range rules {
		checkArgs := append([]string{"-t", r.table, "-C", r.chain}, r.args...)
		if runQuiet("iptables", checkArgs...) != nil {
			addArgs := append([]string{"-t", r.table, "-A", r.chain}, r.args...)
			if err := run("iptables", addArgs...); err != nil {
				return fmt.Errorf("iptables rule %v: %w", r.args, err)
			}
		}
	}

	os.WriteFile("/proc/sys/net/ipv4/ip_forward", []byte("1"), 0644)
	return nil
}

// cleanupOldBridge removes the legacy single-bridge setup (192.168.137.0/24)
// from before per-user bridges. Called once during engine startup migration.
func cleanupOldBridge() {
	// Remove old bridge if it exists
	runQuiet("ip", "link", "del", "brbhatti0")
	// Remove old iptables rules (best effort)
	defaultIface := detectDefaultInterface()
	runQuiet("iptables", "-t", "nat", "-D", "POSTROUTING",
		"-s", "192.168.137.0/24", "-o", defaultIface, "-j", "MASQUERADE")
	for _, dir := range []string{"-i", "-o"} {
		runQuiet("iptables", "-D", "FORWARD",
			dir, "brbhatti0", "-j", "ACCEPT")
	}
}

func createTapDevice(sandboxID string, bridge string) (tapName string, err error) {
	tapName = "tap" + sandboxID[:8]
	if err := run("ip", "tuntap", "add", tapName, "mode", "tap"); err != nil {
		return "", fmt.Errorf("create tap: %w", err)
	}
	if err := run("ip", "link", "set", tapName, "master", bridge); err != nil {
		run("ip", "link", "del", tapName)
		return "", fmt.Errorf("add to bridge %s: %w", bridge, err)
	}
	if err := run("ip", "link", "set", tapName, "up"); err != nil {
		run("ip", "link", "del", tapName)
		return "", fmt.Errorf("bring up tap: %w", err)
	}
	return tapName, nil
}

func destroyTapDevice(tapName string) {
	run("ip", "link", "del", tapName)
}

func detectDefaultInterface() string {
	out, err := exec.Command("ip", "route", "show", "default").Output()
	if err != nil {
		return "eth0"
	}
	fields := strings.Fields(string(out))
	for i, f := range fields {
		if f == "dev" && i+1 < len(fields) {
			return fields[i+1]
		}
	}
	return "eth0"
}

// cleanupOrphanedTapDevices removes any TAP devices prefixed with "tap" that
// don't belong to a known VM. Called on engine startup to recover from crashes.
func cleanupOrphanedTapDevices(knownTaps map[string]bool) {
	out, err := exec.Command("ip", "-o", "link", "show", "type", "tun").Output()
	if err != nil {
		return
	}
	for _, line := range strings.Split(string(out), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 {
			continue
		}
		name := strings.TrimSuffix(fields[1], ":")
		if !strings.HasPrefix(name, "tap") {
			continue
		}
		if knownTaps[name] {
			continue
		}
		slog.Info("cleaning orphaned TAP", "device", name)
		run("ip", "link", "del", name)
	}
}

// cleanupAllTapDevices removes all bhatti-created TAP devices.
func cleanupAllTapDevices() {
	out, err := exec.Command("ip", "-o", "link", "show", "type", "tun").Output()
	if err != nil {
		return
	}
	for _, line := range strings.Split(string(out), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 {
			continue
		}
		name := strings.TrimSuffix(fields[1], ":")
		if strings.HasPrefix(name, "tap") {
			run("ip", "link", "del", name)
		}
	}
}

// cleanupAllUserBridges removes all bhatti user bridges (brbhatti-*).
func cleanupAllUserBridges() {
	out, err := exec.Command("ip", "-o", "link", "show", "type", "bridge").Output()
	if err != nil {
		return
	}
	for _, line := range strings.Split(string(out), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 {
			continue
		}
		name := strings.TrimSuffix(fields[1], ":")
		if strings.HasPrefix(name, "brbhatti-") {
			run("ip", "link", "del", name)
		}
	}
}

func run(name string, args ...string) error {
	cmd := exec.Command(name, args...)
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

// runQuiet runs a command suppressing stderr. Used for idempotent operations
// where "already exists" errors are expected and not useful to log.
func runQuiet(name string, args ...string) error {
	return exec.Command(name, args...).Run()
}
