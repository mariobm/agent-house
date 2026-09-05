package store

import (
	"fmt"
	"time"
)

type SnapshotRecord struct {
	ID            string    `json:"id"`
	UserID        string    `json:"user_id"`
	Name          string    `json:"name"`
	SourceSandbox string    `json:"source_sandbox"`
	MemPath       string    `json:"-"`
	VMPath        string    `json:"-"`
	RootfsPath    string    `json:"-"`
	ConfigPath    string    `json:"-"`
	ManifestJSON  string    `json:"-"`
	SizeMB        int       `json:"size_mb"`
	CreatedAt     time.Time `json:"created_at"`
}

func (s *Store) CreateSnapshot(snap SnapshotRecord) error {
	_, err := s.db.Exec(
		`INSERT INTO snapshots (id, user_id, name, source_sandbox, mem_path, vm_path,
		 rootfs_path, config_path, manifest_json, size_mb, created_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		snap.ID, snap.UserID, snap.Name, snap.SourceSandbox,
		snap.MemPath, snap.VMPath, snap.RootfsPath, snap.ConfigPath,
		snap.ManifestJSON, snap.SizeMB, snap.CreatedAt,
	)
	return err
}

// GetSnapshot retrieves a snapshot by user and name.
func (s *Store) GetSnapshot(userID, name string) (*SnapshotRecord, error) {
	var snap SnapshotRecord
	err := s.db.QueryRow(
		`SELECT id, user_id, name, source_sandbox, mem_path, vm_path,
		 rootfs_path, config_path, manifest_json, size_mb, created_at
		 FROM snapshots WHERE user_id = ? AND name = ?`, userID, name).Scan(
		&snap.ID, &snap.UserID, &snap.Name, &snap.SourceSandbox,
		&snap.MemPath, &snap.VMPath, &snap.RootfsPath, &snap.ConfigPath,
		&snap.ManifestJSON, &snap.SizeMB, &snap.CreatedAt,
	)
	if err != nil {
		return nil, fmt.Errorf("snapshot %q not found", name)
	}
	return &snap, nil
}

// ListSnapshots returns all snapshots for a user.
func (s *Store) ListSnapshots(userID string) ([]SnapshotRecord, error) {
	rows, err := s.db.Query(
		`SELECT id, user_id, name, source_sandbox, mem_path, vm_path,
		 rootfs_path, config_path, manifest_json, size_mb, created_at
		 FROM snapshots WHERE user_id = ? ORDER BY created_at DESC`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []SnapshotRecord
	for rows.Next() {
		var snap SnapshotRecord
		if err := rows.Scan(&snap.ID, &snap.UserID, &snap.Name, &snap.SourceSandbox,
			&snap.MemPath, &snap.VMPath, &snap.RootfsPath, &snap.ConfigPath,
			&snap.ManifestJSON, &snap.SizeMB, &snap.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, snap)
	}
	return out, rows.Err()
}

// DeleteSnapshot removes a snapshot record.
func (s *Store) DeleteSnapshot(userID, name string) error {
	res, err := s.db.Exec(`DELETE FROM snapshots WHERE user_id = ? AND name = ?`, userID, name)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("snapshot %q not found", name)
	}
	return nil
}

// ==========================================================================
// v0.3 Tasks (async operations)
// ==========================================================================
