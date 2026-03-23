package store

import (
	"database/sql"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	_ "modernc.org/sqlite"
)

// TemplateMountSpec defines a default volume mount for a template.
type TemplateMountSpec struct {
	VolumeName string `json:"volume_name"` // empty = "bhatti-{sandbox_name}-workspace"
	Target     string `json:"target"`
	ReadOnly   bool   `json:"readonly"`
	AutoCreate bool   `json:"auto_create"` // create volume if missing
}

// Template is a sandbox blueprint.
type Template struct {
	ID         string              `json:"id"`
	Name       string              `json:"name"`
	Engine     string              `json:"engine"`
	Image      string              `json:"image"`
	CPUs       float64             `json:"cpus"`
	MemoryMB   int                 `json:"memory_mb"`
	DiskSizeMB int                 `json:"disk_size_mb"`
	UserData   string              `json:"userdata"`
	Secrets    []string            `json:"secrets"`
	Labels     map[string]string   `json:"labels"`
	Mounts     []TemplateMountSpec `json:"mounts"`
	CreatedAt  time.Time           `json:"created_at"`
}

// Volume is a named Docker volume tracked by bhatti.
type Volume struct {
	Name      string    `json:"name"`
	CreatedAt time.Time `json:"created_at"`
}

// SandboxVolume records a volume mounted to a sandbox.
type SandboxVolume struct {
	SandboxID  string `json:"sandbox_id"`
	VolumeName string `json:"volume_name"`
	Target     string `json:"target"`
	ReadOnly   bool   `json:"readonly"`
}

// Sandbox is a running or stopped sandbox instance.
type Sandbox struct {
	ID         string          `json:"id"`
	Name       string          `json:"name"`
	TemplateID string          `json:"template_id"`
	EngineID   string          `json:"engine_id"`
	Status     string          `json:"status"`
	IP         string          `json:"ip"`
	EngineMeta json.RawMessage `json:"engine_meta"`
	CreatedBy  string          `json:"created_by"`
	CreatedAt  time.Time       `json:"created_at"`
	StoppedAt  *time.Time      `json:"stopped_at,omitempty"`
}

// SecretRecord tracks an encrypted secret.
type SecretRecord struct {
	Name      string    `json:"name"`
	CreatedAt time.Time `json:"created_at"`
	UpdatedAt time.Time `json:"updated_at"`
	// Encrypted value is NOT included in JSON serialization
}

// User is an authenticated API user.
type User struct {
	ID                    string    `json:"id"`
	Name                  string    `json:"name"`
	APIKeyHash            string    `json:"-"`                        // never serialized
	MaxSandboxes          int       `json:"max_sandboxes"`
	MaxCPUsPerSandbox     int       `json:"max_cpus_per_sandbox"`
	MaxMemoryMBPerSandbox int       `json:"max_memory_mb_per_sandbox"`
	SubnetIndex           int       `json:"subnet_index"`
	CreatedAt             time.Time `json:"created_at"`
}

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

	return &Store{db: db}, nil
}

// Close closes the database.
func (s *Store) Close() error { return s.db.Close() }

// --- Users ---

// CreateUser creates a new API user.
func (s *Store) CreateUser(u User) error {
	_, err := s.db.Exec(
		`INSERT INTO users (id, name, api_key_hash, max_sandboxes, max_cpus_per_sandbox, max_memory_mb_per_sandbox, subnet_index, created_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
		u.ID, u.Name, u.APIKeyHash, u.MaxSandboxes, u.MaxCPUsPerSandbox, u.MaxMemoryMBPerSandbox, u.SubnetIndex, u.CreatedAt,
	)
	return err
}

// GetUserByKeyHash looks up a user by the SHA-256 hash of their API key.
func (s *Store) GetUserByKeyHash(hash string) (*User, error) {
	var u User
	err := s.db.QueryRow(
		`SELECT id, name, api_key_hash, max_sandboxes, max_cpus_per_sandbox, max_memory_mb_per_sandbox, subnet_index, created_at
		 FROM users WHERE api_key_hash = ?`, hash).Scan(
		&u.ID, &u.Name, &u.APIKeyHash, &u.MaxSandboxes, &u.MaxCPUsPerSandbox, &u.MaxMemoryMBPerSandbox, &u.SubnetIndex, &u.CreatedAt,
	)
	if err != nil {
		return nil, err
	}
	return &u, nil
}

// GetUser looks up a user by ID.
func (s *Store) GetUser(id string) (*User, error) {
	var u User
	err := s.db.QueryRow(
		`SELECT id, name, api_key_hash, max_sandboxes, max_cpus_per_sandbox, max_memory_mb_per_sandbox, subnet_index, created_at
		 FROM users WHERE id = ?`, id).Scan(
		&u.ID, &u.Name, &u.APIKeyHash, &u.MaxSandboxes, &u.MaxCPUsPerSandbox, &u.MaxMemoryMBPerSandbox, &u.SubnetIndex, &u.CreatedAt,
	)
	if err != nil {
		return nil, err
	}
	return &u, nil
}

// ListUsers returns all users.
func (s *Store) ListUsers() ([]User, error) {
	rows, err := s.db.Query(
		`SELECT id, name, api_key_hash, max_sandboxes, max_cpus_per_sandbox, max_memory_mb_per_sandbox, subnet_index, created_at
		 FROM users ORDER BY created_at`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []User
	for rows.Next() {
		var u User
		if err := rows.Scan(&u.ID, &u.Name, &u.APIKeyHash, &u.MaxSandboxes, &u.MaxCPUsPerSandbox, &u.MaxMemoryMBPerSandbox, &u.SubnetIndex, &u.CreatedAt); err != nil {
			return nil, err
		}
		out = append(out, u)
	}
	return out, rows.Err()
}

// DeleteUser removes a user. Fails if the user has active sandboxes or secrets.
func (s *Store) DeleteUser(id string) error {
	count, _ := s.CountUserSandboxes(id)
	if count > 0 {
		return fmt.Errorf("user has %d active sandbox(es) — destroy them first", count)
	}
	secrets, _ := s.ListUserSecrets(id)
	if len(secrets) > 0 {
		return fmt.Errorf("user has %d secret(s) — delete them first", len(secrets))
	}
	res, err := s.db.Exec(`DELETE FROM users WHERE id = ?`, id)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("user %q not found", id)
	}
	return nil
}

// NextSubnetIndex returns MAX(subnet_index)+1 for allocating new user networks.
func (s *Store) NextSubnetIndex() (int, error) {
	var maxIdx sql.NullInt64
	s.db.QueryRow(`SELECT MAX(subnet_index) FROM users`).Scan(&maxIdx)
	if !maxIdx.Valid {
		return 1, nil
	}
	return int(maxIdx.Int64) + 1, nil
}

// RotateUserKey updates a user's API key hash. Returns error if user not found.
func (s *Store) RotateUserKey(id, newKeyHash string) error {
	res, err := s.db.Exec(`UPDATE users SET api_key_hash = ? WHERE id = ?`, newKeyHash, id)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("user %q not found", id)
	}
	return nil
}

// --- Templates ---

func (s *Store) CreateTemplate(t Template) error {
	secretsJSON, _ := json.Marshal(t.Secrets)
	labelsJSON, _ := json.Marshal(t.Labels)
	mountsJSON, _ := json.Marshal(t.Mounts)
	if t.Mounts == nil {
		mountsJSON = []byte("[]")
	}
	_, err := s.db.Exec(
		`INSERT INTO templates (id, name, engine, image, cpus, memory_mb, disk_size_mb, userdata, secrets_json, labels_json, mounts_json, created_at)
		 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		t.ID, t.Name, t.Engine, t.Image, t.CPUs, t.MemoryMB, t.DiskSizeMB, t.UserData,
		string(secretsJSON), string(labelsJSON), string(mountsJSON), t.CreatedAt,
	)
	return err
}

func (s *Store) GetTemplate(id string) (*Template, error) {
	row := s.db.QueryRow(`SELECT id, name, engine, image, cpus, memory_mb, disk_size_mb, userdata, secrets_json, labels_json, mounts_json, created_at FROM templates WHERE id = ?`, id)
	return scanTemplate(row)
}

func (s *Store) ListTemplates() ([]Template, error) {
	rows, err := s.db.Query(`SELECT id, name, engine, image, cpus, memory_mb, disk_size_mb, userdata, secrets_json, labels_json, mounts_json, created_at FROM templates ORDER BY created_at DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Template
	for rows.Next() {
		t, err := scanTemplate(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, *t)
	}
	return out, rows.Err()
}

func (s *Store) DeleteTemplate(id string) error {
	res, err := s.db.Exec(`DELETE FROM templates WHERE id = ?`, id)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("template %q not found", id)
	}
	return nil
}

// scanner is satisfied by both *sql.Row and *sql.Rows.
type scanner interface {
	Scan(dest ...any) error
}

func scanTemplate(s scanner) (*Template, error) {
	var t Template
	var secretsJSON, labelsJSON, mountsJSON string
	err := s.Scan(&t.ID, &t.Name, &t.Engine, &t.Image, &t.CPUs, &t.MemoryMB, &t.DiskSizeMB, &t.UserData, &secretsJSON, &labelsJSON, &mountsJSON, &t.CreatedAt)
	if err != nil {
		return nil, err
	}
	if err := json.Unmarshal([]byte(secretsJSON), &t.Secrets); err != nil {
		return nil, fmt.Errorf("unmarshal secrets: %w", err)
	}
	if err := json.Unmarshal([]byte(labelsJSON), &t.Labels); err != nil {
		return nil, fmt.Errorf("unmarshal labels: %w", err)
	}
	if err := json.Unmarshal([]byte(mountsJSON), &t.Mounts); err != nil {
		return nil, fmt.Errorf("unmarshal mounts: %w", err)
	}
	return &t, nil
}

// --- Sandboxes ---

const sandboxCols = `id, name, template_id, engine_id, status, ip, engine_meta_json, created_by, created_at, stopped_at`

func (s *Store) CreateSandbox(sb Sandbox) error {
	if sb.EngineMeta == nil {
		sb.EngineMeta = json.RawMessage("{}")
	}
	_, err := s.db.Exec(
		`INSERT INTO sandboxes (id, name, template_id, engine_id, status, ip, engine_meta_json, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
		sb.ID, sb.Name, sb.TemplateID, sb.EngineID, sb.Status, sb.IP, string(sb.EngineMeta), sb.CreatedBy, sb.CreatedAt,
	)
	return err
}

// GetSandbox returns a sandbox scoped to a user. Use GetSandboxByID for internal/unscoped access.
func (s *Store) GetSandbox(userID, id string) (*Sandbox, error) {
	row := s.db.QueryRow(`SELECT `+sandboxCols+` FROM sandboxes WHERE id = ? AND created_by = ?`, id, userID)
	return scanSandbox(row)
}

// GetSandboxByID returns a sandbox by ID regardless of owner. For internal use (thermal manager, recovery).
func (s *Store) GetSandboxByID(id string) (*Sandbox, error) {
	row := s.db.QueryRow(`SELECT `+sandboxCols+` FROM sandboxes WHERE id = ?`, id)
	return scanSandbox(row)
}

// ListSandboxes returns sandboxes for a user.
func (s *Store) ListSandboxes(userID string) ([]Sandbox, error) {
	rows, err := s.db.Query(`SELECT `+sandboxCols+` FROM sandboxes WHERE created_by = ? ORDER BY created_at DESC`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Sandbox
	for rows.Next() {
		sb, err := scanSandbox(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, *sb)
	}
	return out, rows.Err()
}

// ListAllSandboxes returns all sandboxes regardless of owner. For internal use (thermal manager, recovery, port scanner).
func (s *Store) ListAllSandboxes() ([]Sandbox, error) {
	rows, err := s.db.Query(`SELECT ` + sandboxCols + ` FROM sandboxes ORDER BY created_at DESC`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	var out []Sandbox
	for rows.Next() {
		sb, err := scanSandbox(rows)
		if err != nil {
			return nil, err
		}
		out = append(out, *sb)
	}
	return out, rows.Err()
}

// CountUserSandboxes returns the number of non-destroyed sandboxes for a user.
func (s *Store) CountUserSandboxes(userID string) (int, error) {
	var count int
	err := s.db.QueryRow(`SELECT COUNT(*) FROM sandboxes WHERE created_by = ? AND status != 'destroyed'`, userID).Scan(&count)
	return count, err
}

func (s *Store) UpdateSandboxStatus(id, status string) error {
	_, err := s.db.Exec(`UPDATE sandboxes SET status = ? WHERE id = ?`, status, id)
	return err
}

func (s *Store) UpdateSandboxEngine(id, engineID, ip string) error {
	_, err := s.db.Exec(`UPDATE sandboxes SET engine_id = ?, ip = ? WHERE id = ?`, engineID, ip, id)
	return err
}

func (s *Store) StopSandbox(id string) error {
	now := time.Now()
	_, err := s.db.Exec(`UPDATE sandboxes SET status = 'stopped', stopped_at = ? WHERE id = ?`, now, id)
	return err
}

// DeleteSandbox removes a sandbox scoped to a user.
func (s *Store) DeleteSandbox(userID, id string) error {
	res, err := s.db.Exec(`DELETE FROM sandboxes WHERE id = ? AND created_by = ?`, id, userID)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("sandbox %q not found", id)
	}
	return nil
}

// DeleteSandboxByID removes a sandbox by ID regardless of owner. For internal cleanup.
func (s *Store) DeleteSandboxByID(id string) error {
	res, err := s.db.Exec(`DELETE FROM sandboxes WHERE id = ?`, id)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("sandbox %q not found", id)
	}
	return nil
}

func scanSandbox(s scanner) (*Sandbox, error) {
	var sb Sandbox
	var metaJSON string
	var stoppedAt sql.NullTime
	err := s.Scan(&sb.ID, &sb.Name, &sb.TemplateID, &sb.EngineID, &sb.Status, &sb.IP, &metaJSON, &sb.CreatedBy, &sb.CreatedAt, &stoppedAt)
	if err != nil {
		return nil, err
	}
	sb.EngineMeta = json.RawMessage(metaJSON)
	if stoppedAt.Valid {
		sb.StoppedAt = &stoppedAt.Time
	}
	return &sb, nil
}

// --- Secrets ---

// SetSecret creates or updates an encrypted secret for a user.
func (s *Store) SetSecret(userID, name string, encrypted []byte) error {
	now := time.Now()
	_, err := s.db.Exec(
		`INSERT INTO secrets (user_id, name, value_encrypted, created_at, updated_at)
		 VALUES (?, ?, ?, ?, ?)
		 ON CONFLICT(user_id, name) DO UPDATE SET
		     value_encrypted = excluded.value_encrypted,
		     updated_at = excluded.updated_at`,
		userID, name, encrypted, now, now)
	return err
}

// GetSecretValue returns the encrypted bytes for a user's secret.
func (s *Store) GetSecretValue(userID, name string) ([]byte, error) {
	var encrypted []byte
	err := s.db.QueryRow(`SELECT value_encrypted FROM secrets WHERE name = ? AND user_id = ?`, name, userID).Scan(&encrypted)
	if err != nil {
		return nil, fmt.Errorf("secret %q not found", name)
	}
	return encrypted, nil
}

// ListUserSecrets returns metadata for a user's secrets (no values).
func (s *Store) ListUserSecrets(userID string) ([]SecretRecord, error) {
	rows, err := s.db.Query(`SELECT name, created_at, COALESCE(updated_at, created_at) FROM secrets WHERE user_id = ? ORDER BY name`, userID)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanSecretRecords(rows)
}

// ListAllSecrets returns metadata for all secrets (no values). For admin/internal use.
func (s *Store) ListAllSecrets() ([]SecretRecord, error) {
	rows, err := s.db.Query(`SELECT name, created_at, COALESCE(updated_at, created_at) FROM secrets ORDER BY name`)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	return scanSecretRecords(rows)
}

func scanSecretRecords(rows *sql.Rows) ([]SecretRecord, error) {
	var out []SecretRecord
	for rows.Next() {
		var sr SecretRecord
		var createdStr, updatedStr string
		if err := rows.Scan(&sr.Name, &createdStr, &updatedStr); err != nil {
			return nil, err
		}
		sr.CreatedAt, _ = time.Parse(time.DateTime, createdStr)
		sr.UpdatedAt, _ = time.Parse(time.DateTime, updatedStr)
		out = append(out, sr)
	}
	return out, rows.Err()
}

// GetSecret returns metadata for a user's secret (no value).
func (s *Store) GetSecret(userID, name string) (*SecretRecord, error) {
	var sr SecretRecord
	var createdStr, updatedStr string
	err := s.db.QueryRow(`SELECT name, created_at, COALESCE(updated_at, created_at) FROM secrets WHERE name = ? AND user_id = ?`, name, userID).
		Scan(&sr.Name, &createdStr, &updatedStr)
	if err != nil {
		return nil, err
	}
	sr.CreatedAt, _ = time.Parse(time.DateTime, createdStr)
	sr.UpdatedAt, _ = time.Parse(time.DateTime, updatedStr)
	return &sr, nil
}

// --- Volumes ---

// CreateVolume creates a named volume record. Idempotent — ignores duplicates.
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

func (s *Store) DeleteSecret(userID, name string) error {
	res, err := s.db.Exec(`DELETE FROM secrets WHERE name = ? AND user_id = ?`, name, userID)
	if err != nil {
		return err
	}
	n, _ := res.RowsAffected()
	if n == 0 {
		return fmt.Errorf("secret %q not found", name)
	}
	return nil
}

// --- Firecracker-specific state persistence ---

// FirecrackerState holds the VM state needed to reconnect or resume.
type FirecrackerState struct {
	RootfsPath      string
	SnapMemPath     string
	SnapVMPath      string
	VsockCID        int
	TapDevice       string
	GuestIP         string
	GuestMAC        string
	VcpuCount       float64
	MemSizeMib      int
	SocketPath      string
	VsockPath       string
	AgentToken      string
	HasBaseSnapshot bool
}

// SaveFirecrackerState persists Firecracker-specific VM state.
func (s *Store) SaveFirecrackerState(id string, st FirecrackerState) error {
	hasSnap := 0
	if st.HasBaseSnapshot {
		hasSnap = 1
	}
	_, err := s.db.Exec(`UPDATE sandboxes SET
		rootfs_path = ?, snap_mem_path = ?, snap_vm_path = ?,
		vsock_cid = ?, tap_device = ?, guest_ip = ?, guest_mac = ?,
		vcpu_count = ?, mem_size_mib = ?, socket_path = ?, vsock_path = ?,
		agent_token = ?, has_base_snapshot = ?
		WHERE id = ?`,
		st.RootfsPath, st.SnapMemPath, st.SnapVMPath,
		st.VsockCID, st.TapDevice, st.GuestIP, st.GuestMAC,
		st.VcpuCount, st.MemSizeMib, st.SocketPath, st.VsockPath,
		st.AgentToken, hasSnap,
		id)
	return err
}

// LoadFirecrackerState loads Firecracker-specific VM state.
func (s *Store) LoadFirecrackerState(id string) (*FirecrackerState, error) {
	var st FirecrackerState
	var hasSnap int
	err := s.db.QueryRow(`SELECT
		COALESCE(rootfs_path,''), COALESCE(snap_mem_path,''), COALESCE(snap_vm_path,''),
		COALESCE(vsock_cid,0), COALESCE(tap_device,''), COALESCE(guest_ip,''), COALESCE(guest_mac,''),
		COALESCE(vcpu_count,1), COALESCE(mem_size_mib,512), COALESCE(socket_path,''), COALESCE(vsock_path,''),
		COALESCE(agent_token,''), COALESCE(has_base_snapshot,0)
		FROM sandboxes WHERE id = ?`, id).Scan(
		&st.RootfsPath, &st.SnapMemPath, &st.SnapVMPath,
		&st.VsockCID, &st.TapDevice, &st.GuestIP, &st.GuestMAC,
		&st.VcpuCount, &st.MemSizeMib, &st.SocketPath, &st.VsockPath,
		&st.AgentToken, &hasSnap)
	if err != nil {
		return nil, err
	}
	st.HasBaseSnapshot = hasSnap != 0
	return &st, nil
}
