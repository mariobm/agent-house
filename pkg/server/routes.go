package server

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"

	"github.com/sahil-shubham/bhatti/pkg/engine"
	"github.com/sahil-shubham/bhatti/pkg/store"
)

// saveVMState persists Firecracker VM state to the store if the engine supports it.
func (s *Server) saveVMState(sandboxID, engineID string) {
	provider, ok := s.engine.(engine.VMStateProvider)
	if !ok {
		return
	}
	state := provider.VMState(engineID)
	if state == nil {
		return
	}
	s.store.SaveFirecrackerState(sandboxID, store.FirecrackerState{
		RootfsPath:      strOrEmpty(state, "rootfs_path"),
		SnapMemPath:     strOrEmpty(state, "snap_mem_path"),
		SnapVMPath:      strOrEmpty(state, "snap_vm_path"),
		VsockCID:        intOrZero(state, "vsock_cid"),
		TapDevice:       strOrEmpty(state, "tap_device"),
		GuestIP:         strOrEmpty(state, "guest_ip"),
		GuestMAC:        strOrEmpty(state, "guest_mac"),
		VcpuCount:       floatOrZero(state, "vcpu_count"),
		MemSizeMib:      intOrZero(state, "mem_size_mib"),
		SocketPath:      strOrEmpty(state, "socket_path"),
		VsockPath:       strOrEmpty(state, "vsock_path"),
		AgentToken:      strOrEmpty(state, "agent_token"),
		HasBaseSnapshot: boolOrFalse(state, "has_base_snapshot"),
		FCPathOrigin:    strOrEmpty(state, "fc_path_origin"),
	})
}

func strOrEmpty(m map[string]interface{}, k string) string {
	if v, ok := m[k].(string); ok {
		return v
	}
	return ""
}

func intOrZero(m map[string]interface{}, k string) int {
	switch v := m[k].(type) {
	case int:
		return v
	case int64:
		return int(v)
	case uint32:
		return int(v)
	case float64:
		return int(v)
	}
	return 0
}

func floatOrZero(m map[string]interface{}, k string) float64 {
	switch v := m[k].(type) {
	case float64:
		return v
	case int64:
		return float64(v)
	}
	return 0
}

func boolOrFalse(m map[string]interface{}, k string) bool {
	switch v := m[k].(type) {
	case bool:
		return v
	case int:
		return v != 0
	case float64:
		return v != 0
	}
	return false
}

func (s *Server) routes() {
	s.mux.HandleFunc("/health", s.handleHealth)
	s.mux.HandleFunc("/metrics", s.handleMetrics)
	s.mux.HandleFunc("/templates", s.handleTemplates)
	s.mux.HandleFunc("/templates/", s.handleTemplate)
	s.mux.HandleFunc("/sandboxes", s.handleSandboxes)
	s.mux.HandleFunc("/sandboxes/", s.handleSandbox)
	s.mux.HandleFunc("/secrets", s.handleSecrets)
	s.mux.HandleFunc("/secrets/", s.handleSecret)
	s.mux.HandleFunc("/volumes", s.handlePersistentVolumes)
	s.mux.HandleFunc("/volumes/", s.handlePersistentVolume)
	s.mux.HandleFunc("/images", s.handleImages)
	s.mux.HandleFunc("/images/", s.handleImage)
	s.mux.HandleFunc("/snapshots", s.handleSnapshots)
	s.mux.HandleFunc("/snapshots/", s.handleSnapshot)
	s.mux.HandleFunc("/tasks/", s.handleTask)
	s.mux.HandleFunc("/ports", s.handleAllPorts)
}

func (s *Server) handleMetrics(w http.ResponseWriter, r *http.Request) {
	sandboxes, _ := s.store.ListAllSandboxes()
	users, _ := s.store.ListUsers()

	// Count thermal states
	var hot, warm, cold int
	if te, ok := s.engine.(ThermalEngine); ok {
		for _, sb := range sandboxes {
			if sb.Status != "running" {
				cold++
				continue
			}
			switch te.ThermalState(sb.EngineID) {
			case "hot":
				hot++
			case "warm":
				warm++
			default:
				cold++
			}
		}
	} else {
		for _, sb := range sandboxes {
			if sb.Status == "running" {
				hot++
			} else {
				cold++
			}
		}
	}

	// Count active users (users with at least one non-destroyed sandbox)
	activeUsers := 0
	userHasSandbox := make(map[string]bool)
	for _, sb := range sandboxes {
		userHasSandbox[sb.CreatedBy] = true
	}
	activeUsers = len(userHasSandbox)

	// Host stats (best effort — works on Linux, graceful on others)
	host := map[string]any{}
	if data, err := os.ReadFile("/proc/loadavg"); err == nil {
		var load1 float64
		fmt.Sscanf(string(data), "%f", &load1)
		host["load_1m"] = load1
	}
	if data, err := os.ReadFile("/proc/meminfo"); err == nil {
		lines := strings.Split(string(data), "\n")
		for _, line := range lines {
			if strings.HasPrefix(line, "MemTotal:") {
				var kb int64
				fmt.Sscanf(line, "MemTotal: %d kB", &kb)
				host["memory_total_mb"] = kb / 1024
			}
			if strings.HasPrefix(line, "MemAvailable:") {
				var kb int64
				fmt.Sscanf(line, "MemAvailable: %d kB", &kb)
				host["memory_available_mb"] = kb / 1024
			}
		}
	}

	writeJSON(w, 200, map[string]any{
		"uptime": time.Since(s.startTime).Round(time.Second).String(),
		"sandboxes": map[string]any{
			"total": len(sandboxes),
			"hot":   hot,
			"warm":  warm,
			"cold":  cold,
		},
		"users": map[string]any{
			"total":  len(users),
			"active": activeUsers,
		},
		"host": host,
		"requests": map[string]any{
			"total":         s.requestTotal.Load(),
			"errors_5xx":    s.requestErrors.Load(),
			"auth_failures": s.authFailures.Load(),
		},
	})
}

func (s *Server) handleHealth(w http.ResponseWriter, r *http.Request) {
	sandboxes, _ := s.store.ListAllSandboxes()
	writeJSON(w, 200, map[string]any{
		"status":    "ok",
		"sandboxes": len(sandboxes),
		"uptime":    time.Since(s.startTime).Round(time.Second).String(),
	})
}

// --- Templates ---

// --- Ports ---

func genID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return hex.EncodeToString(b)
}

var validNameRe = regexp.MustCompile(`^[a-zA-Z0-9][a-zA-Z0-9._-]{0,62}$`)

func isValidName(name string) bool {
	return validNameRe.MatchString(name)
}

// isValidMountPath validates that a volume mount path is safe.
// Rejects system paths that would overlay critical guest filesystems.
func isValidMountPath(mount string) bool {
	if mount == "" || mount[0] != '/' {
		return false // must be absolute
	}
	clean := filepath.Clean(mount)
	if strings.Contains(clean, "..") {
		return false
	}
	// Reject system mount points that lohar or the kernel use
	forbidden := []string{"/", "/proc", "/sys", "/dev", "/dev/pts",
		"/run", "/tmp", "/etc", "/bin", "/sbin", "/lib", "/lib64",
		"/usr", "/usr/local/bin", "/boot", "/root"}
	for _, f := range forbidden {
		if clean == f {
			return false
		}
	}
	return true
}

// getUserSandbox is a helper that retrieves a sandbox scoped to the authenticated user.
// Returns nil and writes a 404 error if not found.
func (s *Server) getUserSandbox(w http.ResponseWriter, r *http.Request, id string) *store.Sandbox {
	user := UserFromContext(r.Context())
	sb, err := s.store.GetSandbox(user.ID, id)
	if err != nil {
		errResp(w, 404, "not found")
		return nil
	}
	return sb
}
