//! ahvm-store: durable metadata (users, sandboxes, snapshots, volumes).
//!
//! Clean-break schema (no Go compatibility): cursor-friendly listing
//! everywhere (`seq`/`created_at` ordering, `LIMIT`+cursor params), one
//! typed event bus table feeding the dashboard's SSE stream, backend column
//! on sandboxes/snapshots so krucible and Firecracker rows coexist.

mod entities;
mod events;
mod schema;
#[cfg(test)]
mod tests;

pub use entities::*;
pub use events::*;
pub use schema::init_schema;

use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::Mutex;

/// Thread-safe handle. SQLite with `busy_timeout`; writers serialize on the
/// mutex, readers share the connection (WAL mode for concurrent readers).
#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::from_conn(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        f(&conn)
    }

    /// Insert-or-replace a user by id. Name conflicts with a *different* id
    /// are an error, not a silent rename.
    pub fn upsert_user(&self, u: &User) -> Result<()> {
        self.with_conn(|c| {
            let clash: Option<String> = c
                .query_row(
                    "SELECT id FROM users WHERE name = ?1 AND id != ?2",
                    params![u.name, u.id],
                    |r| r.get(0),
                )
                .ok();
            if clash.is_some() {
                return Err(Error::Conflict(format!("user name {:?}", u.name)));
            }
            c.execute(
                "INSERT INTO users
                   (id, name, api_key_hash, max_sandboxes, max_cpus, max_memory_mb,
                    max_volumes_mb, max_snapshots, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
                 ON CONFLICT(id) DO UPDATE SET
                   name=excluded.name, api_key_hash=excluded.api_key_hash,
                   max_sandboxes=excluded.max_sandboxes, max_cpus=excluded.max_cpus,
                   max_memory_mb=excluded.max_memory_mb,
                   max_volumes_mb=excluded.max_volumes_mb,
                   max_snapshots=excluded.max_snapshots, updated_at=excluded.updated_at",
                params![
                    u.id, u.name, u.api_key_hash, u.max_sandboxes, u.max_cpus,
                    u.max_memory_mb, u.max_volumes_mb, u.max_snapshots,
                    u.created_at, u.updated_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_user(&self, id: &str) -> Result<User> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, name, api_key_hash, max_sandboxes, max_cpus,
                        max_memory_mb, max_volumes_mb, max_snapshots,
                        created_at, updated_at
                 FROM users WHERE id = ?1",
                params![id],
                User::from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("user {id}"))
                }
                e => Error::Sqlite(e),
            })
        })
    }

    pub fn get_user_by_key_hash(&self, hash: &str) -> Result<User> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, name, api_key_hash, max_sandboxes, max_cpus,
                        max_memory_mb, max_volumes_mb, max_snapshots,
                        created_at, updated_at
                 FROM users WHERE api_key_hash = ?1",
                params![hash],
                User::from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Error::NotFound("user by key".into()),
                e => Error::Sqlite(e),
            })
        })
    }

    pub fn create_sandbox(&self, s: &Sandbox) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO sandboxes
                   (id, owner_user_id, name, backend, state, thermal,
                    cpus, memory_mb, ip, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    s.id, s.owner_user_id, s.name, s.backend.as_str(), s.state,
                    s.thermal, s.cpus, s.memory_mb, s.ip, s.created_at, s.updated_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_sandbox(&self, id: &str) -> Result<Sandbox> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, owner_user_id, name, backend, state, thermal,
                        cpus, memory_mb, ip, created_at, updated_at
                 FROM sandboxes WHERE id = ?1",
                params![id],
                Sandbox::from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("sandbox {id}"))
                }
                e => Error::Sqlite(e),
            })
        })
    }

    pub fn set_sandbox_state(&self, id: &str, state: &str, thermal: &str, now: i64) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE sandboxes SET state=?1, thermal=?2, updated_at=?3 WHERE id=?4",
                params![state, thermal, now, id],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("sandbox {id}")));
            }
            Ok(())
        })
    }

    pub fn delete_sandbox(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM sandboxes WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    /// Newest-first sandbox listing for an owner, cursor = (created_at, id)
    /// of the last row seen; `None` starts from the newest.
    pub fn list_sandboxes(
        &self,
        owner: &str,
        after: Option<(i64, String)>,
        limit: usize,
    ) -> Result<Vec<Sandbox>> {
        self.with_conn(|c| {
            let (after_ts, after_id) = after.unwrap_or((i64::MAX, String::new()));
            let mut stmt = c.prepare(
                "SELECT id, owner_user_id, name, backend, state, thermal,
                        cpus, memory_mb, ip, created_at, updated_at
                 FROM sandboxes
                 WHERE owner_user_id = ?1
                   AND (created_at < ?2 OR (created_at = ?2 AND id < ?3))
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?4",
            )?;
            let rows = stmt.query_map(params![owner, after_ts, after_id, limit as i64], Sandbox::from_row)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::Sqlite)
        })
    }

    pub fn create_snapshot(&self, s: &Snapshot) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO snapshots
                   (id, owner_user_id, sandbox_id, name, kind, state,
                    local_bytes, remote_state, remote_manifest_key,
                    created_at, expires_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    s.id, s.owner_user_id, s.sandbox_id, s.name, s.kind, s.state,
                    s.local_bytes, s.remote_state, s.remote_manifest_key,
                    s.created_at, s.expires_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_snapshot(&self, id: &str) -> Result<Snapshot> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, owner_user_id, sandbox_id, name, kind, state,
                        local_bytes, remote_state, remote_manifest_key,
                        created_at, expires_at
                 FROM snapshots WHERE id = ?1",
                params![id],
                Snapshot::from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("snapshot {id}"))
                }
                e => Error::Sqlite(e),
            })
        })
    }

    pub fn set_snapshot_remote(
        &self,
        id: &str,
        remote_state: &str,
        manifest_key: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE snapshots SET remote_state=?1, remote_manifest_key=?2 WHERE id=?3",
                params![remote_state, manifest_key, id],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("snapshot {id}")));
            }
            Ok(())
        })
    }

    pub fn delete_snapshot(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM snapshots WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn create_volume(&self, v: &Volume) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO volumes
                   (id, owner_user_id, name, size_mb, attached_to, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![v.id, v.owner_user_id, v.name, v.size_mb, v.attached_to, v.created_at],
            )?;
            Ok(())
        })
    }

    pub fn get_volume(&self, id: &str) -> Result<Volume> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, owner_user_id, name, size_mb, attached_to, created_at
                 FROM volumes WHERE id = ?1",
                params![id],
                Volume::from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("volume {id}"))
                }
                e => Error::Sqlite(e),
            })
        })
    }

    pub fn attach_volume(&self, id: &str, sandbox_id: Option<&str>) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE volumes SET attached_to=?1 WHERE id=?2",
                params![sandbox_id, id],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("volume {id}")));
            }
            Ok(())
        })
    }

    pub fn delete_volume(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM volumes WHERE id=?1", params![id])?;
            Ok(())
        })
    }

    pub fn upsert_image(&self, img: &Image) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO images (id, name, source, size_mb, created_at)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(id) DO UPDATE SET
                   name=excluded.name, source=excluded.source,
                   size_mb=excluded.size_mb",
                params![img.id, img.name, img.source, img.size_mb, img.created_at],
            )?;
            Ok(())
        })
    }

    pub fn list_images(&self) -> Result<Vec<Image>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, source, size_mb, created_at FROM images ORDER BY name ASC",
            )?;
            let rows = stmt.query_map([], Image::from_row)?;
            rows.collect::<std::result::Result<Vec<_>, _>>().map_err(Error::Sqlite)
        })
    }

    pub fn create_task(&self, t: &Task) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO tasks (id, kind, state, progress, created_at, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![t.id, t.kind, t.state, t.progress, t.created_at, t.updated_at],
            )?;
            Ok(())
        })
    }

    pub fn set_task(&self, id: &str, state: &str, progress: f64, now: i64) -> Result<()> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE tasks SET state=?1, progress=?2, updated_at=?3 WHERE id=?4",
                params![state, progress, now, id],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("task {id}")));
            }
            Ok(())
        })
    }

    pub fn get_task(&self, id: &str) -> Result<Task> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT id, kind, state, progress, created_at, updated_at
                 FROM tasks WHERE id = ?1",
                params![id],
                Task::from_row,
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("task {id}")),
                e => Error::Sqlite(e),
            })
        })
    }
}
