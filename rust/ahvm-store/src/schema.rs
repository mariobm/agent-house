//! v2 schema. Timestamps are unix seconds (`INTEGER`); ids are caller-minted
//! strings (UUIDs in practice). `events.seq` is the global cursor for the
//! dashboard event stream.

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS users (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    api_key_hash    TEXT NOT NULL,
    max_sandboxes   INTEGER NOT NULL DEFAULT 5,
    max_cpus        INTEGER NOT NULL DEFAULT 4,
    max_memory_mb   INTEGER NOT NULL DEFAULT 4096,
    max_volumes_mb  INTEGER NOT NULL DEFAULT 20480,
    max_snapshots   INTEGER NOT NULL DEFAULT 5,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sandboxes (
    id              TEXT PRIMARY KEY,
    owner_user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    backend         TEXT NOT NULL, -- 'krucible' | 'firecracker'
    state           TEXT NOT NULL, -- creating|running|stopped|failed
    thermal         TEXT NOT NULL, -- hot|warm|cold
    cpus            INTEGER NOT NULL,
    memory_mb       INTEGER NOT NULL,
    ip              TEXT NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sandboxes_owner
    ON sandboxes(owner_user_id, created_at DESC, id DESC);
CREATE TABLE IF NOT EXISTS preview_ports (
    sandbox_id TEXT NOT NULL REFERENCES sandboxes(id) ON DELETE CASCADE,
    port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
    generation BLOB NOT NULL DEFAULT (randomblob(16)),
    token_hash TEXT,
    token_expires INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (sandbox_id, port)
);
CREATE TABLE IF NOT EXISTS snapshots (
    id                  TEXT PRIMARY KEY,
    owner_user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    sandbox_id          TEXT REFERENCES sandboxes(id) ON DELETE SET NULL,
    name                TEXT NOT NULL,
    kind                TEXT NOT NULL, -- 'memory' | 'filesystem'
    state               TEXT NOT NULL, -- local|uploading|backed|failed
    local_bytes         INTEGER NOT NULL DEFAULT 0,
    remote_state        TEXT NOT NULL DEFAULT 'none', -- none|uploading|backed
    remote_manifest_key TEXT,
    created_at          INTEGER NOT NULL,
    expires_at          INTEGER
);
CREATE INDEX IF NOT EXISTS idx_snapshots_owner
    ON snapshots(owner_user_id, created_at DESC);
CREATE TABLE IF NOT EXISTS volumes (
    id              TEXT PRIMARY KEY,
    owner_user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    size_mb         INTEGER NOT NULL,
    attached_to     TEXT REFERENCES sandboxes(id) ON DELETE SET NULL,
    created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_volumes_owner ON volumes(owner_user_id);
CREATE TABLE IF NOT EXISTS images (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    source      TEXT NOT NULL,
    size_mb     INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL, -- image-pull | snapshot-push | snapshot-pull
    state       TEXT NOT NULL, -- pending|running|done|failed
    progress    REAL NOT NULL DEFAULT 0.0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    type        TEXT NOT NULL, -- '<resource>.<verb>', e.g. 'sandbox.created'
    user_id     TEXT NOT NULL DEFAULT '',
    sandbox_id  TEXT NOT NULL DEFAULT '',
    payload     TEXT NOT NULL DEFAULT '{}',
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_type_seq ON events(type, seq DESC);
CREATE INDEX IF NOT EXISTS idx_events_sandbox ON events(sandbox_id, seq DESC);
";

pub fn init_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}
