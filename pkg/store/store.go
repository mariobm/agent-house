package store

import (
	"database/sql"
	"fmt"
	"strings"
	"time"

	_ "modernc.org/sqlite"
)

// TemplateMountSpec defines a default volume mount for a template.
// Volume is a named Docker volume tracked by bhatti (legacy v0.1/v0.2).
type Volume struct {
	Name      string    `json:"name"`
	CreatedAt time.Time `json:"created_at"`
}

// SandboxVolume records a volume mounted to a sandbox (legacy v0.1/v0.2).
type SandboxVolume struct {
	SandboxID  string `json:"sandbox_id"`
	VolumeName string `json:"volume_name"`
	Target     string `json:"target"`
	ReadOnly   bool   `json:"readonly"`
}

// PersistentVolume is a v0.3 persistent ext4 volume with its own lifecycle.
type PersistentVolume struct {
	ID          string             `json:"id"`
	UserID      string             `json:"user_id"`
	Name        string             `json:"name"`
	SizeMB      int                `json:"size_mb"`
	Status      string             `json:"status"` // "creating" or "ready"
	FilePath    string             `json:"-"`
	Attachments []VolumeAttachment `json:"attachments"`
	CreatedAt   time.Time          `json:"created_at"`
}

// VolumeBackup records a backup of a persistent volume to S3.
type VolumeBackup struct {
	ID         string    `json:"id"`
	VolumeName string    `json:"volume_name"`
	UserID     string    `json:"user_id"`
	S3Key      string    `json:"s3_key"`
	SizeBytes  int64     `json:"size_bytes"`
	SHA256     string    `json:"sha256"`
	CreatedAt  time.Time `json:"created_at"`
}

// VolumeAttachment records a volume attached to a sandbox.
type VolumeAttachment struct {
	SandboxID string `json:"sandbox_id"`
	Mount     string `json:"mount"`
	ReadOnly  bool   `json:"read_only"`
}

// ImageRecord is a v0.3 rootfs image (admin or user-scoped).
type ImageRecord struct {
	ID            string    `json:"id"`
	UserID        string    `json:"user_id"` // "" = admin/global
	Name          string    `json:"name"`
	Source        string    `json:"source"`
	FilePath      string    `json:"-"`
	SizeMB        int       `json:"size_mb"`
	OCIDigest     string    `json:"oci_digest,omitempty"`
	OCIConfigJSON string    `json:"-"`
	CreatedAt     time.Time `json:"created_at"`
}

// SnapshotRecord is a v0.3 named VM snapshot.
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

// TaskRecord tracks an async operation (e.g., image pull).
type TaskRecord struct {
	ID          string    `json:"id"`
	UserID      string    `json:"user_id"`
	Type        string    `json:"type"`
	Status      string    `json:"status"` // "running", "completed", "failed"
	Progress    string    `json:"progress"`
	ResultJSON  string    `json:"result,omitempty"`
	Error       string    `json:"error,omitempty"`
	CreatedAt   time.Time `json:"created_at"`
	CompletedAt *time.Time `json:"completed_at,omitempty"`
}

// Sandbox is a running or stopped sandbox instance.
// Store wraps SQLite operations.
type Store struct {
	db *sql.DB
}

const schema = `
CREATE TABLE IF NOT EXISTS templates (
	id TEXT PRIMARY KEY,
	name TEXT NOT NULL,
	engine TEXT NOT NULL DEFAULT 'docker',
	image TEXT NOT NULL,
	cpus REAL NOT NULL DEFAULT 1,
	memory_mb INTEGER NOT NULL DEFAULT 512,
	disk_size_mb INTEGER NOT NULL DEFAULT 0,
	userdata TEXT NOT NULL DEFAULT '',
	secrets_json TEXT NOT NULL DEFAULT '[]',
	labels_json TEXT NOT NULL DEFAULT '{}',
	mounts_json TEXT NOT NULL DEFAULT '[]',
	created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS volumes (
	name TEXT PRIMARY KEY,
	created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS sandbox_volumes (
	sandbox_id TEXT NOT NULL,
	volume_name TEXT NOT NULL,
	target TEXT NOT NULL,
	readonly INTEGER NOT NULL DEFAULT 0,
	PRIMARY KEY (sandbox_id, volume_name)
);

CREATE TABLE IF NOT EXISTS sandboxes (
	id TEXT PRIMARY KEY,
	name TEXT NOT NULL,
	template_id TEXT NOT NULL DEFAULT '',
	engine_id TEXT NOT NULL DEFAULT '',
	status TEXT NOT NULL DEFAULT 'unknown',
	ip TEXT NOT NULL DEFAULT '',
	engine_meta_json TEXT NOT NULL DEFAULT '{}',
	created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
	stopped_at DATETIME
);

CREATE TABLE IF NOT EXISTS secrets (
	name TEXT PRIMARY KEY,
	path TEXT NOT NULL,
	created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS users (
	id TEXT PRIMARY KEY,
	name TEXT NOT NULL UNIQUE,
	api_key_hash TEXT NOT NULL UNIQUE,
	max_sandboxes INTEGER NOT NULL DEFAULT 5,
	max_cpus_per_sandbox INTEGER NOT NULL DEFAULT 4,
	max_memory_mb_per_sandbox INTEGER NOT NULL DEFAULT 4096,
	subnet_index INTEGER NOT NULL DEFAULT 0,
	created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
`

// migrations runs ALTER TABLE statements for columns added after initial schema.
// Duplicate column errors are silently ignored (idempotent).
const migrations = `
ALTER TABLE templates ADD COLUMN mounts_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE sandboxes ADD COLUMN rootfs_path TEXT DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN snap_mem_path TEXT DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN snap_vm_path TEXT DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN vsock_cid INTEGER DEFAULT 0;
ALTER TABLE sandboxes ADD COLUMN tap_device TEXT DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN guest_ip TEXT DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN guest_mac TEXT DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN vcpu_count REAL DEFAULT 1;
ALTER TABLE sandboxes ADD COLUMN mem_size_mib INTEGER DEFAULT 512;
ALTER TABLE sandboxes ADD COLUMN socket_path TEXT DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN vsock_path TEXT DEFAULT '';
ALTER TABLE secrets ADD COLUMN value_encrypted BLOB DEFAULT NULL;
ALTER TABLE secrets ADD COLUMN updated_at DATETIME DEFAULT CURRENT_TIMESTAMP;
ALTER TABLE sandboxes ADD COLUMN created_by TEXT NOT NULL DEFAULT '';
ALTER TABLE secrets ADD COLUMN user_id TEXT NOT NULL DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN agent_token TEXT DEFAULT '';
ALTER TABLE sandboxes ADD COLUMN has_base_snapshot INTEGER DEFAULT 0;
ALTER TABLE users ADD COLUMN max_volume_storage_mb INTEGER NOT NULL DEFAULT 20480;
ALTER TABLE users ADD COLUMN max_images INTEGER NOT NULL DEFAULT 10;
ALTER TABLE users ADD COLUMN max_snapshots INTEGER NOT NULL DEFAULT 5;
ALTER TABLE sandboxes ADD COLUMN keep_hot INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sandboxes ADD COLUMN fc_path_origin TEXT DEFAULT '';
`

// New opens (or creates) the SQLite database and runs migrations.
func New(dbPath string) (*Store, error) {
	db, err := sql.Open("sqlite", dbPath+"?_pragma=journal_mode(WAL)&_pragma=busy_timeout(5000)")
	if err != nil {
		return nil, fmt.Errorf("open db: %w", err)
	}
	if _, err := db.Exec(schema); err != nil {
		return nil, fmt.Errorf("migrate: %w", err)
	}
	// Run additive migrations — ignore "duplicate column" errors
	for _, stmt := range strings.Split(migrations, ";") {
		stmt = strings.TrimSpace(stmt)
		if stmt == "" || strings.HasPrefix(stmt, "--") {
			continue
		}
		db.Exec(stmt) // ignore errors (column already exists)
	}

	// v0.3 tables: persistent volumes, images, snapshots, tasks
	db.Exec(`CREATE TABLE IF NOT EXISTS volumes_v2 (
		id TEXT PRIMARY KEY,
		user_id TEXT NOT NULL,
		name TEXT NOT NULL,
		size_mb INTEGER NOT NULL,
		file_path TEXT NOT NULL,
		status TEXT NOT NULL DEFAULT 'ready',
		created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
		UNIQUE(user_id, name)
	)`)
	db.Exec(`CREATE TABLE IF NOT EXISTS volume_attachments (
		volume_id TEXT NOT NULL,
		sandbox_id TEXT NOT NULL,
		mount TEXT NOT NULL,
		read_only INTEGER NOT NULL DEFAULT 0,
		attached_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
		PRIMARY KEY (volume_id, sandbox_id)
	)`)
	db.Exec(`CREATE TABLE IF NOT EXISTS images (
		id TEXT PRIMARY KEY,
		user_id TEXT NOT NULL DEFAULT '',
		name TEXT NOT NULL,
		source TEXT NOT NULL DEFAULT '',
		file_path TEXT NOT NULL,
		size_mb INTEGER NOT NULL DEFAULT 0,
		oci_digest TEXT NOT NULL DEFAULT '',
		oci_config_json TEXT NOT NULL DEFAULT '{}',
		created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
		UNIQUE(user_id, name)
	)`)
	db.Exec(`CREATE TABLE IF NOT EXISTS snapshots (
		id TEXT PRIMARY KEY,
		user_id TEXT NOT NULL,
		name TEXT NOT NULL,
		source_sandbox TEXT NOT NULL,
		mem_path TEXT NOT NULL,
		vm_path TEXT NOT NULL,
		rootfs_path TEXT NOT NULL,
		config_path TEXT NOT NULL,
		manifest_json TEXT NOT NULL,
		size_mb INTEGER NOT NULL DEFAULT 0,
		created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
		UNIQUE(user_id, name)
	)`)
	db.Exec(`CREATE TABLE IF NOT EXISTS tasks (
		id TEXT PRIMARY KEY,
		user_id TEXT NOT NULL,
		type TEXT NOT NULL,
		status TEXT NOT NULL DEFAULT 'running',
		progress TEXT NOT NULL DEFAULT '',
		result_json TEXT NOT NULL DEFAULT '{}',
		error TEXT NOT NULL DEFAULT '',
		created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
		completed_at DATETIME
	)`)
	db.Exec(`CREATE INDEX IF NOT EXISTS idx_tasks_created_at ON tasks(created_at)`)

	// v0.4: publish rules for public proxy
	db.Exec(`CREATE TABLE IF NOT EXISTS publish_rules (
		id TEXT PRIMARY KEY,
		sandbox_id TEXT NOT NULL,
		user_id TEXT NOT NULL,
		port INTEGER NOT NULL,
		alias TEXT NOT NULL UNIQUE,
		created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
		UNIQUE(sandbox_id, port)
	)`)
	db.Exec(`CREATE INDEX IF NOT EXISTS idx_publish_rules_sandbox ON publish_rules(sandbox_id)`)

	// v0.5: volume backups
	db.Exec(`CREATE TABLE IF NOT EXISTS volume_backups (
		id TEXT PRIMARY KEY,
		volume_name TEXT NOT NULL,
		user_id TEXT NOT NULL,
		s3_key TEXT NOT NULL,
		size_bytes INTEGER NOT NULL,
		sha256 TEXT NOT NULL DEFAULT '',
		created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
	)`)
	db.Exec(`CREATE INDEX IF NOT EXISTS idx_volume_backups_name ON volume_backups(user_id, volume_name, created_at DESC)`)

	// Create unique index on (created_by, name) for non-destroyed sandboxes.
	// Prevents a user from having two sandboxes with the same name.
	db.Exec(`CREATE UNIQUE INDEX IF NOT EXISTS idx_sandboxes_user_name
		ON sandboxes(created_by, name) WHERE status != 'destroyed'`)

	// Migrate secrets table to composite primary key (user_id, name).
	// The original table had PRIMARY KEY(name) which prevents two users
	// from having a secret with the same name. This migration recreates
	// the table with the correct composite key.
	db.Exec(`CREATE TABLE IF NOT EXISTS secrets_v2 (
		user_id TEXT NOT NULL DEFAULT '',
		name TEXT NOT NULL,
		path TEXT NOT NULL DEFAULT '',
		value_encrypted BLOB DEFAULT NULL,
		created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
		updated_at DATETIME DEFAULT CURRENT_TIMESTAMP,
		PRIMARY KEY (user_id, name)
	)`)
	// Copy data from old table if it exists and secrets_v2 is empty
	db.Exec(`INSERT OR IGNORE INTO secrets_v2 (user_id, name, path, value_encrypted, created_at, updated_at)
		SELECT COALESCE(user_id, ''), name, COALESCE(path, ''), value_encrypted,
		       created_at, COALESCE(updated_at, created_at) FROM secrets`)
	db.Exec(`DROP TABLE IF EXISTS secrets`)
	db.Exec(`ALTER TABLE secrets_v2 RENAME TO secrets`)

	// Image sharing table — allows sharing images with specific users
	db.Exec(`CREATE TABLE IF NOT EXISTS image_shares (
		image_id TEXT NOT NULL,
		user_id TEXT NOT NULL,
		PRIMARY KEY (image_id, user_id)
	)`)

	return &Store{db: db}, nil
}

// Close closes the database.
func (s *Store) Close() error { return s.db.Close() }

// --- Users ---

// CreateUser creates a new API user.


// SetSecret creates or updates an encrypted secret for a user.
func (s *Store) CreateVolume(name string) error {
	_, err := s.db.Exec(
		`INSERT OR IGNORE INTO volumes (name, created_at) VALUES (?, ?)`,
		name, time.Now(),
	)
	return err
}

// GetVolume retrieves a volume by name.
func (s *Store) GetVolume(name string) (*Volume, error) {
	var v Volume
	err := s.db.QueryRow(`SELECT name, created_at FROM volumes WHERE name = ?`, name).
		Scan(&v.Name, &v.CreatedAt)
	if err != nil {
		return nil, err
	}
	return &v, nil
}

// ListVolumes returns all tracked volumes.
func (s *Store) ListVolumes() ([]Volume, error) {
	rows, err := s.db.Query(`SELECT name, created_at FROM volumes ORDER BY created_at DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Volume
	for rows.Next() {
		var v Volume
		if err := rows.Scan(&v.Name, &v.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, rows.Err()
}

// DeleteVolume removes a volume record. Fails if any sandbox is using it.
func (s *Store) DeleteVolume(name string) error {
	var count int
	s.db.QueryRow(`SELECT COUNT(*) FROM sandbox_volumes WHERE volume_name = ?`, name).Scan(&count)
	if count > 0 {
		return fmt.Errorf("volume %q is in use by %d sandbox(es)", name, count)
	}
	res, err := s.db.Exec(`DELETE FROM volumes WHERE name = ?`, name)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("volume %q not found", name)
	}
	return nil
}

// AttachVolume records a volume mount for a sandbox.
func (s *Store) AttachVolume(sandboxID, volumeName, target string, readonly bool) error {
	ro := 0
	if readonly {
		ro = 1
	}
	_, err := s.db.Exec(
		`INSERT OR REPLACE INTO sandbox_volumes (sandbox_id, volume_name, target, readonly) VALUES (?, ?, ?, ?)`,
		sandboxID, volumeName, target, ro,
	)
	return err
}

// GetSandboxVolumes returns all volume mounts for a sandbox.
func (s *Store) GetSandboxVolumes(sandboxID string) ([]SandboxVolume, error) {
	rows, err := s.db.Query(
		`SELECT sandbox_id, volume_name, target, readonly FROM sandbox_volumes WHERE sandbox_id = ?`,
		sandboxID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []SandboxVolume
	for rows.Next() {
		var sv SandboxVolume
		var ro int
		if err := rows.Scan(&sv.SandboxID, &sv.VolumeName, &sv.Target, &ro); err != nil {
			return nil, err
		}
		sv.ReadOnly = ro != 0
		out = append(out, sv)
	}
	return out, rows.Err()
}

// DetachVolumes removes all volume mount records for a sandbox (called on destroy).
func (s *Store) DetachVolumes(sandboxID string) error {
	_, err := s.db.Exec(`DELETE FROM sandbox_volumes WHERE sandbox_id = ?`, sandboxID)
	return err
}

// Returns error on UNIQUE violation (not idempotent — for race coordination).
func (s *Store) CreatePersistentVolume(v PersistentVolume) error {
	_, err := s.db.Exec(
		`INSERT INTO volumes_v2 (id, user_id, name, size_mb, file_path, status, created_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?)`,
		v.ID, v.UserID, v.Name, v.SizeMB, v.FilePath, v.Status, v.CreatedAt,
	)
	return err
}

// GetPersistentVolume retrieves a persistent volume by user and name, including attachments.
func (s *Store) GetPersistentVolume(userID, name string) (*PersistentVolume, error) {
	var v PersistentVolume
	err := s.db.QueryRow(
		`SELECT id, user_id, name, size_mb, file_path, status, created_at
		 FROM volumes_v2 WHERE user_id = ? AND name = ?`, userID, name).Scan(
		&v.ID, &v.UserID, &v.Name, &v.SizeMB, &v.FilePath, &v.Status, &v.CreatedAt,
	)
	if err != nil {
		return nil, err
	}
	// Load attachments
	rows, err := s.db.Query(
		`SELECT sandbox_id, mount, read_only FROM volume_attachments WHERE volume_id = ?`, v.ID)
	if err == nil {
		defer rows.Close()
		for rows.Next() {
			var a VolumeAttachment
			var ro int
			if err := rows.Scan(&a.SandboxID, &a.Mount, &ro); err == nil {
				a.ReadOnly = ro != 0
				v.Attachments = append(v.Attachments, a)
			}
		}
	}
	return &v, nil
}

// ListPersistentVolumes returns all persistent volumes for a user.
func (s *Store) ListPersistentVolumes(userID string) ([]PersistentVolume, error) {
	rows, err := s.db.Query(
		`SELECT id, user_id, name, size_mb, file_path, status, created_at
		 FROM volumes_v2 WHERE user_id = ? ORDER BY created_at DESC`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []PersistentVolume
	for rows.Next() {
		var v PersistentVolume
		if err := rows.Scan(&v.ID, &v.UserID, &v.Name, &v.SizeMB, &v.FilePath, &v.Status, &v.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, v)
	}
	return out, rows.Err()
}

// DeletePersistentVolume removes a persistent volume record. Fails if any attachments exist.
func (s *Store) DeletePersistentVolume(userID, name string) error {
	tx, err := s.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	var volID string
	err = tx.QueryRow(`SELECT id FROM volumes_v2 WHERE user_id = ? AND name = ?`,
		userID, name).Scan(&volID)
	if err != nil {
		return fmt.Errorf("volume %q not found", name)
	}

	var count int
	tx.QueryRow(`SELECT COUNT(*) FROM volume_attachments WHERE volume_id = ?`, volID).Scan(&count)
	if count > 0 {
		return fmt.Errorf("volume %q has %d active attachment(s)", name, count)
	}

	tx.Exec(`DELETE FROM volumes_v2 WHERE id = ?`, volID)
	return tx.Commit()
}

// AttachPersistentVolume attaches a persistent volume to a sandbox with concurrency checks.
func (s *Store) AttachPersistentVolume(userID, name, sandboxID, mount string, readOnly bool) error {
	tx, err := s.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()

	var volID, status string
	err = tx.QueryRow(`SELECT id, status FROM volumes_v2 WHERE user_id = ? AND name = ?`,
		userID, name).Scan(&volID, &status)
	if err != nil {
		return fmt.Errorf("volume %q not found", name)
	}
	if status == "creating" {
		return fmt.Errorf("volume %q is being created, retry shortly", name)
	}

	var rwCount, roCount int
	tx.QueryRow(`SELECT COUNT(*) FROM volume_attachments WHERE volume_id = ? AND read_only = 0`,
		volID).Scan(&rwCount)
	tx.QueryRow(`SELECT COUNT(*) FROM volume_attachments WHERE volume_id = ? AND read_only = 1`,
		volID).Scan(&roCount)

	if !readOnly {
		if rwCount > 0 || roCount > 0 {
			return fmt.Errorf("volume %q already attached (rw=%d, ro=%d)", name, rwCount, roCount)
		}
	} else {
		if rwCount > 0 {
			return fmt.Errorf("volume %q has a read-write attachment, cannot attach read-only", name)
		}
	}

	ro := 0
	if readOnly {
		ro = 1
	}
	_, err = tx.Exec(
		`INSERT INTO volume_attachments (volume_id, sandbox_id, mount, read_only) VALUES (?, ?, ?, ?)`,
		volID, sandboxID, mount, ro)
	if err != nil {
		return err
	}
	return tx.Commit()
}

// DetachPersistentVolume removes a specific volume attachment.
func (s *Store) DetachPersistentVolume(userID, name, sandboxID string) error {
	var volID string
	err := s.db.QueryRow(`SELECT id FROM volumes_v2 WHERE user_id = ? AND name = ?`,
		userID, name).Scan(&volID)
	if err != nil {
		return fmt.Errorf("volume %q not found", name)
	}
	_, err = s.db.Exec(
		`DELETE FROM volume_attachments WHERE volume_id = ? AND sandbox_id = ?`,
		volID, sandboxID)
	return err
}

// DetachAllPersistentVolumesForSandbox removes all persistent volume attachments for a sandbox.
func (s *Store) DetachAllPersistentVolumesForSandbox(sandboxID string) error {
	_, err := s.db.Exec(`DELETE FROM volume_attachments WHERE sandbox_id = ?`, sandboxID)
	return err
}

// AttachedPersistentVolumesForSandbox returns all persistent volumes attached
// to a sandbox, with their file paths and mount info. Used during recovery to
// rebuild the VM's volume list so resume can hard-link them into the jail.
func (s *Store) AttachedPersistentVolumesForSandbox(sandboxID string) ([]struct {
	VolumeName string
	FilePath   string
	Mount      string
	ReadOnly   bool
}, error) {
	rows, err := s.db.Query(
		`SELECT v.name, v.file_path, va.mount, va.read_only
		 FROM volume_attachments va
		 JOIN volumes_v2 v ON v.id = va.volume_id
		 WHERE va.sandbox_id = ?`, sandboxID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []struct {
		VolumeName string
		FilePath   string
		Mount      string
		ReadOnly   bool
	}
	for rows.Next() {
		var v struct {
			VolumeName string
			FilePath   string
			Mount      string
			ReadOnly   bool
		}
		var ro int
		if err := rows.Scan(&v.VolumeName, &v.FilePath, &v.Mount, &ro); err != nil {
			return nil, err
		}
		v.ReadOnly = ro != 0
		out = append(out, v)
	}
	return out, rows.Err()
}

// DetachOrphanedPersistentVolumes removes attachments for destroyed/missing sandboxes.
// Must be called AFTER recoverVMs updates sandbox statuses.
func (s *Store) DetachOrphanedPersistentVolumes() (int64, error) {
	res, err := s.db.Exec(`DELETE FROM volume_attachments
		WHERE sandbox_id IN (
			SELECT va.sandbox_id FROM volume_attachments va
			LEFT JOIN sandboxes s ON va.sandbox_id = s.id
			WHERE s.id IS NULL
			   OR s.status IN ('destroyed', 'unknown')
		)`)
	if err != nil {
		return 0, err
	}
	n, _ := res.RowsAffected()
	return n, nil
}

// UpdatePersistentVolumeSize updates the size_mb field after a resize.
func (s *Store) UpdatePersistentVolumeSize(userID, name string, sizeMB int) error {
	res, err := s.db.Exec(`UPDATE volumes_v2 SET size_mb = ? WHERE user_id = ? AND name = ?`,
		sizeMB, userID, name)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("volume %q not found", name)
	}
	return nil
}

// UpdatePersistentVolumeStatus updates the status field (e.g., "creating" → "ready").
func (s *Store) UpdatePersistentVolumeStatus(userID, name, status string) error {
	_, err := s.db.Exec(`UPDATE volumes_v2 SET status = ? WHERE user_id = ? AND name = ?`,
		status, userID, name)
	return err
}

// UserVolumeStorageUsed returns the total size_mb of all persistent volumes for a user.
func (s *Store) UserVolumeStorageUsed(userID string) (int, error) {
	var total sql.NullInt64
	s.db.QueryRow(`SELECT SUM(size_mb) FROM volumes_v2 WHERE user_id = ?`, userID).Scan(&total)
	if !total.Valid {
		return 0, nil
	}
	return int(total.Int64), nil
}

// ==========================================================================
// v0.3 Images
// ==========================================================================

// CreateImage inserts a new image record.
func (s *Store) CreateImage(img ImageRecord) error {
	_, err := s.db.Exec(
		`INSERT INTO images (id, user_id, name, source, file_path, size_mb, oci_digest, oci_config_json, created_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		img.ID, img.UserID, img.Name, img.Source, img.FilePath,
		img.SizeMB, img.OCIDigest, img.OCIConfigJSON, img.CreatedAt,
	)
	return err
}

// GetImage retrieves an image by user and name. Falls back to admin images (user_id='').
func (s *Store) GetImage(userID, name string) (*ImageRecord, error) {
	var img ImageRecord
	const cols = `id, user_id, name, source, file_path, size_mb, oci_digest, oci_config_json, created_at`
	scanImg := func(row *sql.Row) error {
		return row.Scan(&img.ID, &img.UserID, &img.Name, &img.Source, &img.FilePath,
			&img.SizeMB, &img.OCIDigest, &img.OCIConfigJSON, &img.CreatedAt)
	}

	// 1. User's own image
	if scanImg(s.db.QueryRow(`SELECT `+cols+` FROM images WHERE user_id = ? AND name = ?`, userID, name)) == nil {
		return &img, nil
	}
	// 2. Image shared with this user
	if scanImg(s.db.QueryRow(`SELECT i.id, i.user_id, i.name, i.source, i.file_path,
		i.size_mb, i.oci_digest, i.oci_config_json, i.created_at
		FROM images i JOIN image_shares s ON s.image_id = i.id
		WHERE s.user_id = ? AND i.name = ?`, userID, name)) == nil {
		return &img, nil
	}
	// 3. Global admin image (user_id = '')
	if scanImg(s.db.QueryRow(`SELECT `+cols+` FROM images WHERE user_id = '' AND name = ?`, name)) == nil {
		return &img, nil
	}
	return nil, fmt.Errorf("image %q not found", name)
}

// ListImages returns all images visible to a user (their own + shared + admin).
func (s *Store) ListImages(userID string) ([]ImageRecord, error) {
	rows, err := s.db.Query(
		`SELECT id, user_id, name, source, file_path, size_mb, oci_digest, oci_config_json, created_at
		 FROM images
		 WHERE user_id = ?
		    OR user_id = ''
		    OR id IN (SELECT image_id FROM image_shares WHERE user_id = ?)
		 ORDER BY created_at DESC`, userID, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []ImageRecord
	for rows.Next() {
		var img ImageRecord
		if err := rows.Scan(&img.ID, &img.UserID, &img.Name, &img.Source, &img.FilePath,
			&img.SizeMB, &img.OCIDigest, &img.OCIConfigJSON, &img.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, img)
	}
	return out, rows.Err()
}

// DeleteImage removes an image record.
func (s *Store) DeleteImage(userID, name string) error {
	res, err := s.db.Exec(`DELETE FROM images WHERE user_id = ? AND name = ?`, userID, name)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("image %q not found", name)
	}
	return nil
}

// --- Image Sharing ---

// ShareImage grants a user access to an image.
func (s *Store) ShareImage(imageID, userID string) error {
	_, err := s.db.Exec(`INSERT OR IGNORE INTO image_shares (image_id, user_id) VALUES (?, ?)`, imageID, userID)
	return err
}

// UnshareImage revokes a user's access to an image.
func (s *Store) UnshareImage(imageID, userID string) error {
	_, err := s.db.Exec(`DELETE FROM image_shares WHERE image_id = ? AND user_id = ?`, imageID, userID)
	return err
}

// ListImageShares returns user IDs that an image is shared with.
func (s *Store) ListImageShares(imageID string) ([]string, error) {
	rows, err := s.db.Query(`SELECT s.user_id, COALESCE(u.name, s.user_id)
		FROM image_shares s LEFT JOIN users u ON u.id = s.user_id
		WHERE s.image_id = ?`, imageID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var names []string
	for rows.Next() {
		var uid, name string
		rows.Scan(&uid, &name)
		names = append(names, name)
	}
	return names, rows.Err()
}

// GetImageByName looks up an image by name regardless of owner.
func (s *Store) GetImageByName(name string) (*ImageRecord, error) {
	var img ImageRecord
	err := s.db.QueryRow(
		`SELECT id, user_id, name, source, file_path, size_mb, oci_digest, oci_config_json, created_at
		 FROM images WHERE name = ? LIMIT 1`, name).Scan(
		&img.ID, &img.UserID, &img.Name, &img.Source, &img.FilePath,
		&img.SizeMB, &img.OCIDigest, &img.OCIConfigJSON, &img.CreatedAt)
	if err != nil {
		return nil, fmt.Errorf("image %q not found", name)
	}
	return &img, nil
}

// GetUserByName looks up a user by name.

// CreateSnapshot inserts a new snapshot record.
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

// CreateTask inserts a new task record.
func (s *Store) CreateTask(t TaskRecord) error {
	_, err := s.db.Exec(
		`INSERT INTO tasks (id, user_id, type, status, progress, result_json, error, created_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
		t.ID, t.UserID, t.Type, t.Status, t.Progress, t.ResultJSON, t.Error, t.CreatedAt,
	)
	return err
}

// GetTask retrieves a task by ID.
func (s *Store) GetTask(id string) (*TaskRecord, error) {
	var t TaskRecord
	var completedAt sql.NullTime
	err := s.db.QueryRow(
		`SELECT id, user_id, type, status, progress, result_json, error, created_at, completed_at
		 FROM tasks WHERE id = ?`, id).Scan(
		&t.ID, &t.UserID, &t.Type, &t.Status, &t.Progress,
		&t.ResultJSON, &t.Error, &t.CreatedAt, &completedAt,
	)
	if err != nil {
		return nil, fmt.Errorf("task %q not found", id)
	}
	if completedAt.Valid {
		t.CompletedAt = &completedAt.Time
	}
	return &t, nil
}

// UpdateTaskProgress updates the progress string on a running task.
func (s *Store) UpdateTaskProgress(id, progress string) error {
	_, err := s.db.Exec(`UPDATE tasks SET progress = ? WHERE id = ?`, progress, id)
	return err
}

// CompleteTask marks a task as completed with a result.
func (s *Store) CompleteTask(id, resultJSON string) error {
	now := time.Now()
	_, err := s.db.Exec(`UPDATE tasks SET status = 'completed', result_json = ?, completed_at = ? WHERE id = ?`,
		resultJSON, now, id)
	return err
}

// FailTask marks a task as failed with an error.
func (s *Store) FailTask(id, errMsg string) error {
	now := time.Now()
	_, err := s.db.Exec(`UPDATE tasks SET status = 'failed', error = ?, completed_at = ? WHERE id = ?`,
		errMsg, now, id)
	return err
}

// CleanupOldTasks removes completed/failed tasks older than the given duration.
func (s *Store) CleanupOldTasks(maxAge time.Duration) (int64, error) {
	cutoff := time.Now().Add(-maxAge)
	res, err := s.db.Exec(`DELETE FROM tasks WHERE created_at < ? AND status IN ('completed', 'failed')`, cutoff)
	if err != nil {
		return 0, err
	}
	n, _ := res.RowsAffected()
	return n, nil
}

// ==========================================================================
// v0.4 Publish Rules (public proxy)
// ==========================================================================

// PublishRule maps a public alias to a sandbox port.
type PublishRule struct {
	ID        string    `json:"id"`
	SandboxID string    `json:"sandbox_id"`
	UserID    string    `json:"user_id"`
	Port      int       `json:"port"`
	Alias     string    `json:"alias"`
	CreatedAt time.Time `json:"created_at"`
}

// CreatePublishRule inserts a new publish rule.
// Returns a descriptive error on UNIQUE constraint violations.
func (s *Store) CreatePublishRule(rule PublishRule) error {
	_, err := s.db.Exec(
		`INSERT INTO publish_rules (id, sandbox_id, user_id, port, alias)
		 VALUES (?, ?, ?, ?, ?)`,
		rule.ID, rule.SandboxID, rule.UserID, rule.Port, rule.Alias,
	)
	if err != nil {
		if strings.Contains(err.Error(), "UNIQUE constraint") {
			if strings.Contains(err.Error(), "alias") {
				return fmt.Errorf("alias %q is already taken", rule.Alias)
			}
			return fmt.Errorf("port %d is already published for this sandbox", rule.Port)
		}
		return err
	}
	return nil
}

// GetPublishRuleByAlias looks up a publish rule by its public alias.
func (s *Store) GetPublishRuleByAlias(alias string) (*PublishRule, error) {
	var r PublishRule
	err := s.db.QueryRow(
		`SELECT id, sandbox_id, user_id, port, alias, created_at
		 FROM publish_rules WHERE alias = ?`, alias,
	).Scan(&r.ID, &r.SandboxID, &r.UserID, &r.Port, &r.Alias, &r.CreatedAt)
	if err == sql.ErrNoRows {
		return nil, fmt.Errorf("no publish rule for alias %q", alias)
	}
	return &r, err
}

// ListPublishRules returns all publish rules for a sandbox.
func (s *Store) ListPublishRules(sandboxID string) ([]PublishRule, error) {
	rows, err := s.db.Query(
		`SELECT id, sandbox_id, user_id, port, alias, created_at
		 FROM publish_rules WHERE sandbox_id = ?`, sandboxID,
	)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var rules []PublishRule
	for rows.Next() {
		var r PublishRule
		if err := rows.Scan(&r.ID, &r.SandboxID, &r.UserID, &r.Port, &r.Alias, &r.CreatedAt); err != nil {
			return nil, err
		}
		rules = append(rules, r)
	}
	return rules, rows.Err()
}

// ListUserPublishRules returns all publish rules for sandboxes owned by a user.
func (s *Store) ListUserPublishRules(userID string) ([]PublishRule, error) {
	rows, err := s.db.Query(
		`SELECT id, sandbox_id, user_id, port, alias, created_at
		 FROM publish_rules WHERE user_id = ?`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var rules []PublishRule
	for rows.Next() {
		var r PublishRule
		if err := rows.Scan(&r.ID, &r.SandboxID, &r.UserID, &r.Port, &r.Alias, &r.CreatedAt); err != nil {
			return nil, err
		}
		rules = append(rules, r)
	}
	return rules, rows.Err()
}

// DeletePublishRule removes a publish rule scoped to user + sandbox + port.
func (s *Store) DeletePublishRule(userID, sandboxID string, port int) error {
	res, err := s.db.Exec(
		`DELETE FROM publish_rules WHERE user_id = ? AND sandbox_id = ? AND port = ?`,
		userID, sandboxID, port,
	)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return fmt.Errorf("no publish rule for port %d on this sandbox", port)
	}
	return nil
}

// DeletePublishRulesForSandbox removes all publish rules for a sandbox.
func (s *Store) DeletePublishRulesForSandbox(sandboxID string) (int64, error) {
	res, err := s.db.Exec(`DELETE FROM publish_rules WHERE sandbox_id = ?`, sandboxID)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// CleanupOrphanedPublishRules removes rules for destroyed/unknown/missing sandboxes.
func (s *Store) CleanupOrphanedPublishRules() (int64, error) {
	res, err := s.db.Exec(`
		DELETE FROM publish_rules WHERE sandbox_id NOT IN (
			SELECT id FROM sandboxes WHERE status NOT IN ('destroyed', 'unknown')
		)`)
	if err != nil {
		return 0, err
	}
	return res.RowsAffected()
}

// --- Volume Backups ---

// CreateVolumeBackup records a new backup.
func (s *Store) CreateVolumeBackup(b VolumeBackup) error {
	_, err := s.db.Exec(
		`INSERT INTO volume_backups (id, volume_name, user_id, s3_key, size_bytes, sha256, created_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?)`,
		b.ID, b.VolumeName, b.UserID, b.S3Key, b.SizeBytes, b.SHA256, b.CreatedAt)
	return err
}

// ListVolumeBackups returns backups for a volume, newest first.
func (s *Store) ListVolumeBackups(userID, volumeName string) ([]VolumeBackup, error) {
	rows, err := s.db.Query(
		`SELECT id, volume_name, user_id, s3_key, size_bytes, sha256, created_at
		 FROM volume_backups WHERE user_id = ? AND volume_name = ?
		 ORDER BY created_at DESC`, userID, volumeName)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []VolumeBackup
	for rows.Next() {
		var b VolumeBackup
		if err := rows.Scan(&b.ID, &b.VolumeName, &b.UserID, &b.S3Key, &b.SizeBytes, &b.SHA256, &b.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, b)
	}
	return out, rows.Err()
}

// GetVolumeBackup returns a single backup by ID.
func (s *Store) GetVolumeBackup(userID, backupID string) (*VolumeBackup, error) {
	var b VolumeBackup
	err := s.db.QueryRow(
		`SELECT id, volume_name, user_id, s3_key, size_bytes, sha256, created_at
		 FROM volume_backups WHERE id = ? AND user_id = ?`, backupID, userID).Scan(
		&b.ID, &b.VolumeName, &b.UserID, &b.S3Key, &b.SizeBytes, &b.SHA256, &b.CreatedAt)
	if err != nil {
		return nil, err
	}
	return &b, nil
}

// DeleteVolumeBackup removes a backup record.
func (s *Store) DeleteVolumeBackup(userID, backupID string) error {
	_, err := s.db.Exec(`DELETE FROM volume_backups WHERE id = ? AND user_id = ?`, backupID, userID)
	return err
}

// OldestVolumeBackups returns the oldest backups beyond the retention count.
func (s *Store) OldestVolumeBackups(userID, volumeName string, keepCount int) ([]VolumeBackup, error) {
	rows, err := s.db.Query(
		`SELECT id, volume_name, user_id, s3_key, size_bytes, sha256, created_at
		 FROM volume_backups WHERE user_id = ? AND volume_name = ?
		 ORDER BY created_at DESC LIMIT -1 OFFSET ?`, userID, volumeName, keepCount)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []VolumeBackup
	for rows.Next() {
		var b VolumeBackup
		if err := rows.Scan(&b.ID, &b.VolumeName, &b.UserID, &b.S3Key, &b.SizeBytes, &b.SHA256, &b.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, b)
	}
	return out, rows.Err()
}
