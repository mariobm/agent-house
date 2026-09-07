//! Typed event bus table: every state change appends a row; the dashboard
//! tails it via cursor (`seq`) for both polling and the SSE stream.

use super::{Result, Store};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub r#type: String,
    pub user_id: String,
    pub sandbox_id: String,
    pub payload: serde_json::Value,
    pub created_at: i64,
}

#[derive(Debug, Default)]
pub struct EventFilter {
    pub type_prefix: Option<String>,
    pub sandbox_id: Option<String>,
    /// Exclusive lower bound: return rows with `seq > after_seq`.
    pub after_seq: i64,
    pub limit: usize,
}

impl Store {
    /// Append an event; returns its `seq` cursor.
    pub fn record_event(
        &self,
        r#type: &str,
        user_id: &str,
        sandbox_id: &str,
        payload: &serde_json::Value,
        now: i64,
    ) -> Result<i64> {
        self.with_conn(|c| super::record_event_inner(c, r#type, user_id, sandbox_id, payload, now))
    }

    /// Oldest-first page after a cursor. `limit == 0` means a sane default.
    pub fn query_events(&self, f: &EventFilter) -> Result<Vec<Event>> {
        let limit = if f.limit == 0 {
            100
        } else {
            f.limit.min(500) as i64
        };
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT seq, type, user_id, sandbox_id, payload, created_at
                 FROM events
                 WHERE seq > ?1
                   AND (?2 = '' OR type LIKE ?2 || '%')
                   AND (?3 = '' OR sandbox_id = ?3)
                 ORDER BY seq ASC
                 LIMIT ?4",
            )?;
            let prefix = f.type_prefix.clone().unwrap_or_default();
            let sandbox = f.sandbox_id.clone().unwrap_or_default();
            let rows = stmt.query_map(params![f.after_seq, prefix, sandbox, limit], |r| {
                let payload_str: String = r.get(4)?;
                let payload = serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
                Ok(Event {
                    seq: r.get(0)?,
                    r#type: r.get(1)?,
                    user_id: r.get(2)?,
                    sandbox_id: r.get(3)?,
                    payload,
                    created_at: r.get(5)?,
                })
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()
                .map_err(super::Error::Sqlite)
        })
    }
}
