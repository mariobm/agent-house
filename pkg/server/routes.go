package server

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"os/exec"
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

// --- Persistent Volumes (v0.3) ---

func (s *Server) handlePersistentVolumes(w http.ResponseWriter, r *http.Request) {
	user := UserFromContext(r.Context())

	switch r.Method {
	case http.MethodGet:
		list, err := s.store.ListPersistentVolumes(user.ID)
		if err != nil {
			errRespInternal(w, r, "list volumes failed", err)
			return
		}
		if list == nil {
			list = []store.PersistentVolume{}
		}
		writeJSON(w, 200, list)
	case http.MethodPost:
		var req struct {
			Name   string `json:"name"`
			SizeMB int    `json:"size_mb"`
		}
		if err := readJSON(r, &req); err != nil {
			errResp(w, 400, "invalid json: "+err.Error())
			return
		}
		if req.Name == "" || req.SizeMB <= 0 {
			errResp(w, 400, "name and size_mb (> 0) required")
			return
		}
		if !isValidName(req.Name) {
			errResp(w, 400, "invalid volume name: must match [a-zA-Z0-9][a-zA-Z0-9._-]{0,62}")
			return
		}

		// Check quota
		used, _ := s.store.UserVolumeStorageUsed(user.ID)
		userObj, _ := s.store.GetUser(user.ID)
		maxStorage := 20480 // default 20GB
		if userObj != nil {
			maxStorage = userObj.MaxVolumeStorageMB
		}
		if used+req.SizeMB > maxStorage {
			errResp(w, 429, fmt.Sprintf("volume storage quota exceeded (%dMB used, %dMB max)", used, maxStorage))
			return
		}

		volDir := filepath.Join(s.dataDir, "volumes", user.ID)
		os.MkdirAll(volDir, 0700)
		volPath := filepath.Join(volDir, req.Name+".ext4")

		vol := store.PersistentVolume{
			ID: genID(), UserID: user.ID, Name: req.Name,
			SizeMB: req.SizeMB, FilePath: volPath,
			Status: "creating", CreatedAt: time.Now(),
		}
		if err := s.store.CreatePersistentVolume(vol); err != nil {
			if strings.Contains(err.Error(), "UNIQUE") {
				errResp(w, 409, fmt.Sprintf("volume %q already exists", req.Name))
			} else {
				errRespInternal(w, r, "create volume failed", err)
			}
			return
		}

		if err := createVolumeFile(volPath, req.SizeMB); err != nil {
			s.store.DeletePersistentVolume(user.ID, req.Name)
			errRespInternal(w, r, "create volume file failed", err)
			return
		}
		s.store.UpdatePersistentVolumeStatus(user.ID, req.Name, "ready")
		vol.Status = "ready"

		slog.Info("volume.created", "name", req.Name, "user", user.Name, "size_mb", req.SizeMB)
		writeJSON(w, 201, vol)
	default:
		errResp(w, 405, "method not allowed")
	}
}

func (s *Server) handlePersistentVolume(w http.ResponseWriter, r *http.Request) {
	user := UserFromContext(r.Context())
	urlPath := strings.TrimPrefix(r.URL.Path, "/volumes/")
	parts := strings.SplitN(urlPath, "/", 2)
	name := parts[0]

	if name == "" {
		errResp(w, 400, "missing volume name")
		return
	}
	if !isValidName(name) {
		errResp(w, 400, "invalid volume name")
		return
	}

	// Sub-routes: /volumes/{name}/{action}[/{id}]
	if len(parts) == 2 {
		sub := parts[1]
		// Handle /volumes/{name}/backups and /volumes/{name}/backups/{id}
		if sub == "backups" || strings.HasPrefix(sub, "backups/") {
			s.handleVolumeBackups(w, r, user, name, strings.TrimPrefix(sub, "backups"))
			return
		}
		switch sub {
		case "resize":
			s.handleVolumeResize(w, r, user, name)
		case "snapshot":
			s.handleVolumeSnapshot(w, r, user, name)
		default:
			errResp(w, 404, "not found")
		}
		return
	}

	switch r.Method {
	case http.MethodGet:
		vol, err := s.store.GetPersistentVolume(user.ID, name)
		if err != nil {
			errResp(w, 404, "not found")
			return
		}
		writeJSON(w, 200, vol)
	case http.MethodDelete:
		// Check for file path before deleting from store
		vol, err := s.store.GetPersistentVolume(user.ID, name)
		if err != nil {
			errResp(w, 404, err.Error())
			return
		}
		if err := s.store.DeletePersistentVolume(user.ID, name); err != nil {
			if strings.Contains(err.Error(), "attachment") {
				errResp(w, 409, err.Error())
			} else if strings.Contains(err.Error(), "not found") {
				errResp(w, 404, err.Error())
			} else {
				errRespInternal(w, r, "delete volume failed", err)
			}
			return
		}
		// Delete file (orphan cleanup on startup handles failures)
		if vol.FilePath != "" {
			os.Remove(vol.FilePath)
		}
		slog.Info("volume.deleted", "name", name, "user", user.Name)
		writeJSON(w, 200, map[string]string{"status": "deleted"})
	default:
		errResp(w, 405, "method not allowed")
	}
}

func (s *Server) handleVolumeBackups(w http.ResponseWriter, r *http.Request, user *store.User, volumeName, subPath string) {
	if s.backupBackend == nil {
		errResp(w, 501, "backup not configured — add backup section to config.yaml")
		return
	}

	// subPath is "" for /backups, "/{id}" for /backups/{id}, "/restore" for /backups/restore
	subPath = strings.TrimPrefix(subPath, "/")

	switch {
	case subPath == "" && r.Method == http.MethodGet:
		// List backups
		backups, err := s.store.ListVolumeBackups(user.ID, volumeName)
		if err != nil {
			errRespInternal(w, r, "list backups failed", err)
			return
		}
		if backups == nil {
			backups = []store.VolumeBackup{}
		}
		writeJSON(w, 200, backups)

	case subPath == "" && r.Method == http.MethodPost:
		// Trigger backup
		vol, err := s.store.GetPersistentVolume(user.ID, volumeName)
		if err != nil {
			errResp(w, 404, "volume not found")
			return
		}
		result, err := s.performVolumeBackup(r.Context(), user, vol)
		if err != nil {
			errRespInternal(w, r, "backup failed", err)
			return
		}
		writeJSON(w, 201, result)

	case subPath == "restore" && r.Method == http.MethodPost:
		// Restore from backup
		var req struct {
			BackupID string `json:"backup_id"`
		}
		if err := json.NewDecoder(r.Body).Decode(&req); err != nil || req.BackupID == "" {
			errResp(w, 400, "missing backup_id")
			return
		}
		vol, err := s.store.GetPersistentVolume(user.ID, volumeName)
		if err != nil {
			errResp(w, 404, "volume not found")
			return
		}
		// Volume must be detached
		if len(vol.Attachments) > 0 {
			errResp(w, 409, "volume is attached to a sandbox — detach before restoring")
			return
		}
		backup, err := s.store.GetVolumeBackup(user.ID, req.BackupID)
		if err != nil {
			errResp(w, 404, "backup not found")
			return
		}
		if err := s.performVolumeRestore(r.Context(), vol, backup); err != nil {
			errRespInternal(w, r, "restore failed", err)
			return
		}
		writeJSON(w, 200, map[string]string{"status": "restored", "backup_id": req.BackupID})

	case subPath != "" && subPath != "restore" && r.Method == http.MethodDelete:
		// Delete a backup
		backupID := subPath
		b, err := s.store.GetVolumeBackup(user.ID, backupID)
		if err != nil {
			errResp(w, 404, "backup not found")
			return
		}
		if err := s.backupBackend.Delete(r.Context(), b.S3Key); err != nil {
			slog.Warn("s3 delete failed", "key", b.S3Key, "error", err)
			// Continue — remove from DB even if S3 delete fails
		}
		s.store.DeleteVolumeBackup(user.ID, backupID)
		writeJSON(w, 200, map[string]string{"status": "deleted"})

	default:
		errResp(w, 405, "method not allowed")
	}
}

func (s *Server) performVolumeBackup(ctx context.Context, user *store.User, vol *store.PersistentVolume) (*store.VolumeBackup, error) {
	// Clone the volume file for a consistent snapshot.
	// On btrfs this is instant (reflink), on ext4 it's a full copy.
	tmpFile := vol.FilePath + ".backup-tmp"
	defer os.Remove(tmpFile)
	if err := exec.CommandContext(ctx, "cp", "--reflink=auto", "--sparse=always", vol.FilePath, tmpFile).Run(); err != nil {
		return nil, fmt.Errorf("clone volume: %w", err)
	}

	// Compress with zstd and stream to S3
	timestamp := time.Now().UTC().Format(time.RFC3339)
	s3Key := fmt.Sprintf("volumes/%s/%s/%s.ext4.zst", user.ID, vol.Name, timestamp)

	compressedFile := tmpFile + ".zst"
	defer os.Remove(compressedFile)
	if err := exec.CommandContext(ctx, "zstd", "-3", "-q", tmpFile, "-o", compressedFile).Run(); err != nil {
		return nil, fmt.Errorf("compress volume: %w", err)
	}
	os.Remove(tmpFile) // free space early

	// Get compressed size and compute hash
	f, err := os.Open(compressedFile)
	if err != nil {
		return nil, fmt.Errorf("open compressed: %w", err)
	}
	defer f.Close()

	fi, err := f.Stat()
	if err != nil {
		return nil, err
	}

	// Upload to S3
	if err := s.backupBackend.Upload(ctx, s3Key, f, fi.Size()); err != nil {
		return nil, fmt.Errorf("upload to s3: %w", err)
	}

	// Record in DB
	id := genID()
	record := store.VolumeBackup{
		ID:         id,
		VolumeName: vol.Name,
		UserID:     user.ID,
		S3Key:      s3Key,
		SizeBytes:  fi.Size(),
		CreatedAt:  time.Now().UTC(),
	}
	if err := s.store.CreateVolumeBackup(record); err != nil {
		return nil, fmt.Errorf("record backup: %w", err)
	}

	slog.Info("volume backup created",
		"volume", vol.Name, "backup_id", id,
		"size", fi.Size(), "s3_key", s3Key)

	return &record, nil
}

func (s *Server) performVolumeRestore(ctx context.Context, vol *store.PersistentVolume, b *store.VolumeBackup) error {
	// Download from S3
	reader, err := s.backupBackend.Download(ctx, b.S3Key)
	if err != nil {
		return fmt.Errorf("download from s3: %w", err)
	}
	defer reader.Close()

	// Write compressed data to temp file
	compressedFile := vol.FilePath + ".restore-tmp.zst"
	defer os.Remove(compressedFile)
	out, err := os.Create(compressedFile)
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, reader); err != nil {
		out.Close()
		return fmt.Errorf("download: %w", err)
	}
	out.Close()

	// Decompress over the volume file
	if err := exec.CommandContext(ctx, "zstd", "-d", "-q", "-f", compressedFile, "-o", vol.FilePath).Run(); err != nil {
		return fmt.Errorf("decompress: %w", err)
	}

	slog.Info("volume restored from backup",
		"volume", vol.Name, "backup_id", b.ID)

	return nil
}

func (s *Server) handleVolumeResize(w http.ResponseWriter, r *http.Request, user *store.User, name string) {
	if r.Method != http.MethodPost {
		errResp(w, 405, "method not allowed")
		return
	}
	if !isValidName(name) {
		errResp(w, 400, "invalid volume name")
		return
	}
	var req struct {
		SizeMB int `json:"size_mb"`
	}
	if err := readJSON(r, &req); err != nil {
		errResp(w, 400, "invalid json: "+err.Error())
		return
	}

	vol, err := s.store.GetPersistentVolume(user.ID, name)
	if err != nil {
		errResp(w, 404, "volume not found")
		return
	}
	if len(vol.Attachments) > 0 {
		errResp(w, 409, "volume is attached — detach before resizing")
		return
	}
	if req.SizeMB <= vol.SizeMB {
		errResp(w, 400, fmt.Sprintf("new size (%dMB) must be larger than current size (%dMB)", req.SizeMB, vol.SizeMB))
		return
	}

	// e2fsck + truncate + resize2fs
	if out, err := exec.Command("e2fsck", "-f", "-y", vol.FilePath).CombinedOutput(); err != nil {
		slog.Warn("e2fsck before resize", "output", string(out), "error", err)
	}
	if err := exec.Command("truncate", "-s", fmt.Sprintf("%dM", req.SizeMB), vol.FilePath).Run(); err != nil {
		errRespInternal(w, r, "truncate volume failed", err)
		return
	}
	if err := exec.Command("resize2fs", vol.FilePath).Run(); err != nil {
		errRespInternal(w, r, "resize2fs failed", err)
		return
	}

	s.store.UpdatePersistentVolumeSize(user.ID, name, req.SizeMB)
	slog.Info("volume.resized", "name", name, "user", user.Name, "new_size_mb", req.SizeMB)
	writeJSON(w, 200, map[string]any{"status": "resized", "size_mb": req.SizeMB})
}

// handleVolumeSnapshot creates an independent copy of a volume.
func (s *Server) handleVolumeSnapshot(w http.ResponseWriter, r *http.Request, user *store.User, srcName string) {
	if r.Method != http.MethodPost {
		errResp(w, 405, "method not allowed")
		return
	}
	var req struct {
		Name string `json:"name"`
	}
	if err := readJSON(r, &req); err != nil {
		errResp(w, 400, "invalid json: "+err.Error())
		return
	}
	if req.Name == "" || !isValidName(req.Name) {
		errResp(w, 400, "valid name required")
		return
	}

	src, err := s.store.GetPersistentVolume(user.ID, srcName)
	if err != nil {
		errResp(w, 404, "source volume not found")
		return
	}
	if len(src.Attachments) > 0 {
		errResp(w, 409, "source volume is attached — detach before snapshotting")
		return
	}

	dstDir := filepath.Join(s.dataDir, "volumes", user.ID)
	os.MkdirAll(dstDir, 0700)
	dstPath := filepath.Join(dstDir, req.Name+".ext4")

	vol := store.PersistentVolume{
		ID: genID(), UserID: user.ID, Name: req.Name,
		SizeMB: src.SizeMB, FilePath: dstPath,
		Status: "creating", CreatedAt: time.Now(),
	}
	if err := s.store.CreatePersistentVolume(vol); err != nil {
		if strings.Contains(err.Error(), "UNIQUE") {
			errResp(w, 409, fmt.Sprintf("volume %q already exists", req.Name))
		} else {
			errRespInternal(w, r, "create volume snapshot record failed", err)
		}
		return
	}

	if err := exec.Command("cp", "--sparse=always", src.FilePath, dstPath).Run(); err != nil {
		s.store.DeletePersistentVolume(user.ID, req.Name)
		errRespInternal(w, r, "copy volume file failed", err)
		return
	}

	s.store.UpdatePersistentVolumeStatus(user.ID, req.Name, "ready")
	vol.Status = "ready"

	slog.Info("volume.snapshot", "src", srcName, "dst", req.Name, "user", user.Name, "size_mb", src.SizeMB)
	writeJSON(w, 201, vol)
}

// volumeIsClean checks if an ext4 volume has a clean journal.
// Returns false if the journal is dirty (needs e2fsck before RO mount).
func volumeIsClean(path string) bool {
	out, err := exec.Command("tune2fs", "-l", path).Output()
	if err != nil {
		return false
	}
	return strings.Contains(string(out), "Filesystem state:            clean")
}

// createVolumeFile creates an ext4 image of the specified size in MB.
func createVolumeFile(path string, sizeMB int) error {
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	if err := f.Truncate(int64(sizeMB) << 20); err != nil {
		f.Close()
		return err
	}
	f.Close()
	// mkfs.ext4 may not be available on non-Linux platforms (tests)
	if err := exec.Command("mkfs.ext4", "-F", "-q", path).Run(); err != nil {
		// File exists as a sparse file — usable for store tests, not for real VMs
		slog.Warn("mkfs.ext4 failed (expected on non-Linux)", "path", path, "error", err)
	}
	return nil
}

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
