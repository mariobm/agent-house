//! Durable, fenced receipts for finite generic node work.
//!
//! IDs are globally unique while retained. Sandbox deletion cascades receipts;
//! this is deliberately not a tombstone/idempotency guarantee across deletion.
//! Callers authorize ownership and must never reuse deleted sandbox/run IDs.
use crate::{Error, Result, Store};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

const MAX_ACTIVE: i64 = 64;
const MAX_RETAINED: i64 = 4096;
const COLUMNS: &str = "id,sandbox_id,owner_id,request_json,phase,epoch,session_id,boot_id,\
                      created_at,updated_at,deadline_at,finished_at,exit_code,detail";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedRun {
    pub id: String,
    pub sandbox_id: String,
    pub owner_id: String,
    pub request_json: String,
    pub phase: String,
    pub epoch: i64,
    pub session_id: Option<String>,
    pub boot_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deadline_at: i64,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i32>,
    pub detail: Option<String>,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ManagedRun> {
    Ok(ManagedRun {
        id: r.get(0)?,
        sandbox_id: r.get(1)?,
        owner_id: r.get(2)?,
        request_json: r.get(3)?,
        phase: r.get(4)?,
        epoch: r.get(5)?,
        session_id: r.get(6)?,
        boot_id: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
        deadline_at: r.get(10)?,
        finished_at: r.get(11)?,
        exit_code: r.get(12)?,
        detail: r.get(13)?,
    })
}

fn find(conn: &Connection, id: &str) -> Result<Option<ManagedRun>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM managed_runs WHERE id=?1"),
            [id],
            row,
        )
        .optional()?)
}

fn get(conn: &Connection, id: &str) -> Result<ManagedRun> {
    find(conn, id)?.ok_or_else(|| Error::NotFound(format!("managed run {id}")))
}

impl Store {
    /// Atomically admit one run per sandbox, at most 64 active and 4096 total.
    /// Exact retries return the original receipt even when terminal or capacity
    /// is exhausted; timestamps/deadline on a retry cannot rewrite the record.
    /// `request_json` equality is byte-for-byte, not semantic JSON equivalence.
    pub fn admit_managed_run(
        &self,
        id: &str,
        sandbox_id: &str,
        owner_id: &str,
        request_json: &str,
        now: i64,
        deadline_at: i64,
    ) -> Result<(ManagedRun, bool)> {
        self.transaction(|tx| {
            let conn = &tx.tx;
            if let Some(existing) = find(conn, id)? {
                return if existing.sandbox_id == sandbox_id
                    && existing.owner_id == owner_id
                    && existing.request_json == request_json
                {
                    Ok((existing, false))
                } else {
                    Err(Error::Conflict("managed run request ID reused".into()))
                };
            }
            if id.is_empty() || sandbox_id.is_empty() || owner_id.is_empty() || deadline_at <= now {
                return Err(Error::Conflict("invalid managed run admission".into()));
            }
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sandboxes WHERE id=?1)",
                [sandbox_id],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(Error::NotFound(format!("sandbox {sandbox_id}")));
            }
            let retained: i64 =
                conn.query_row("SELECT count(*) FROM managed_runs", [], |r| r.get(0))?;
            if retained >= MAX_RETAINED {
                return Err(Error::Conflict("managed run receipt capacity exhausted".into()));
            }
            let active: i64 = conn.query_row(
                "SELECT count(*) FROM managed_runs WHERE finished_at IS NULL",
                [],
                |r| r.get(0),
            )?;
            let busy: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM managed_runs WHERE sandbox_id=?1 AND finished_at IS NULL)",
                [sandbox_id],
                |r| r.get(0),
            )?;
            if active >= MAX_ACTIVE || busy {
                return Err(Error::Conflict("managed run already active or queue full".into()));
            }
            conn.execute(
                "INSERT INTO managed_runs(id,sandbox_id,owner_id,request_json,phase,epoch,created_at,updated_at,deadline_at)
                 VALUES(?1,?2,?3,?4,'starting',1,?5,?5,?6)",
                params![id, sandbox_id, owner_id, request_json, now, deadline_at],
            )?;
            Ok((get(conn, id)?, true))
        })
    }

    pub fn get_managed_run(&self, id: &str) -> Result<ManagedRun> {
        self.with_conn(|conn| get(conn, id))
    }

    pub fn list_active_managed_runs(&self) -> Result<Vec<ManagedRun>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM managed_runs WHERE finished_at IS NULL ORDER BY created_at,id"
            ))?;
            let rows = stmt.query_map([], row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn managed_run_for_sandbox(&self, id: &str) -> Result<Option<ManagedRun>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM managed_runs WHERE sandbox_id=?1 AND finished_at IS NULL"),
                    [id],
                    row,
                )
                .optional()?)
        })
    }

    /// Latest persisted finish per sandbox, without an age filter. The node
    /// restores its own cooldown policy conservatively across restart/skew.
    pub fn list_managed_run_cooldowns(&self) -> Result<Vec<(String, i64)>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT sandbox_id,max(finished_at) FROM managed_runs
                 WHERE finished_at IS NOT NULL GROUP BY sandbox_id ORDER BY sandbox_id",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Fence a previous controller during recovery. This does not dispatch or
    /// reset phase/identity/deadline; ambiguous runs must not be blindly retried.
    pub fn claim_managed_run(&self, id: &str, now: i64) -> Result<ManagedRun> {
        self.transaction(|tx| {
            let current = get(&tx.tx, id)?;
            if current.finished_at.is_some() {
                return Err(Error::Conflict("managed run is terminal".into()));
            }
            let next = current
                .epoch
                .checked_add(1)
                .ok_or_else(|| Error::Conflict("managed run epoch exhausted".into()))?;
            let changed = tx.tx.execute(
                "UPDATE managed_runs SET epoch=?2,updated_at=?3 WHERE id=?1 AND epoch=?4 AND finished_at IS NULL",
                params![id, next, now, current.epoch],
            )?;
            if changed != 1 {
                return Err(Error::Conflict("managed run claim lost".into()));
            }
            get(&tx.tx, id)
        })
    }

    /// Update only the current, unfinished epoch. Native session and boot IDs
    /// bind once; None preserves a known identity, and replacement conflicts.
    #[allow(clippy::too_many_arguments)]
    pub fn update_managed_run(
        &self,
        id: &str,
        epoch: i64,
        phase: &str,
        session_id: Option<&str>,
        boot_id: Option<&str>,
        exit_code: Option<i32>,
        detail: Option<&str>,
        now: i64,
        terminal: bool,
    ) -> Result<ManagedRun> {
        let terminal_phase = matches!(phase, "succeeded" | "failed" | "interrupted");
        if terminal != terminal_phase
            || !matches!(
                phase,
                "starting"
                    | "running"
                    | "uncertain"
                    | "cancelling"
                    | "succeeded"
                    | "failed"
                    | "interrupted"
            )
        {
            return Err(Error::Conflict(
                "invalid managed run phase/terminal flag".into(),
            ));
        }
        self.transaction(|tx| {
            let current = get(&tx.tx, id)?;
            if current.finished_at.is_some() || current.epoch != epoch {
                return Err(Error::Conflict("managed run is terminal or epoch stale".into()));
            }
            if session_id.is_some_and(|new| current.session_id.as_deref().is_some_and(|old| new != old))
                || boot_id.is_some_and(|new| current.boot_id.as_deref().is_some_and(|old| new != old))
            {
                return Err(Error::Conflict("managed run native identity changed".into()));
            }
            let changed = tx.tx.execute(
                "UPDATE managed_runs SET phase=?3,session_id=coalesce(?4,session_id),boot_id=coalesce(?5,boot_id),
                 exit_code=?6,detail=?7,updated_at=?8,finished_at=?9
                 WHERE id=?1 AND epoch=?2 AND finished_at IS NULL",
                params![id, epoch, phase, session_id, boot_id, exit_code, detail, now, terminal.then_some(now)],
            )?;
            if changed != 1 {
                return Err(Error::Conflict("managed run update lost".into()));
            }
            get(&tx.tx, id)
        })
    }
}
