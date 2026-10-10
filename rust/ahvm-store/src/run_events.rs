//! Ordered host outbox. Acknowledgement releases payload storage, never identity.
use crate::{runs, Error, ManagedRun, Result, Store};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const MAX_MANAGED_EVENT_BYTES: usize = 300 * 1024;
const MAX_RUN_PENDING: i64 = 8 * 1024 * 1024;
const MAX_TOTAL_PENDING: i64 = 128 * 1024 * 1024;
const MAX_PENDING_STREAMS: i64 = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedRunEventStream {
    pub run_id: String,
    pub source_offset: u64,
    pub guest_seq: u64,
    pub next_event_seq: u64,
    pub acked_seq: u64,
    pub checkpoint_json: Option<String>,
    pub pending_bytes: u64,
    pub source_failed: bool,
    pub sealed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedRunEvent {
    pub run_id: String,
    pub seq: u64,
    pub payload_json: String,
}

fn stream(conn: &Connection, id: &str) -> Result<ManagedRunEventStream> {
    conn.query_row(
        "SELECT run_id,source_offset,guest_seq,next_event_seq,acked_seq,checkpoint_json,pending_bytes,source_failed,sealed
         FROM managed_run_event_streams WHERE run_id=?1",
        [id],
        |r| Ok(ManagedRunEventStream {
            run_id:r.get(0)?, source_offset:r.get::<_, i64>(1)? as u64, guest_seq:r.get::<_, i64>(2)? as u64,
            next_event_seq:r.get::<_, i64>(3)? as u64, acked_seq:r.get::<_, i64>(4)? as u64, checkpoint_json:r.get(5)?,
            pending_bytes:r.get::<_, i64>(6)? as u64, source_failed:r.get(7)?, sealed:r.get(8)?,
        }),
    ).optional()?.ok_or_else(|| Error::NotFound(format!("managed run event stream {id}")))
}

fn totals(conn: &Connection) -> Result<(i64, i64, i64)> {
    Ok(conn.query_row(
        "SELECT coalesce(sum(pending_bytes),0),coalesce(sum(CASE WHEN sealed=0 THEN 1 ELSE 0 END),0),
         coalesce(sum(CASE WHEN sealed=0 OR acked_seq<next_event_seq-1 THEN 1 ELSE 0 END),0)
         FROM managed_run_event_streams", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?)
}

pub(crate) fn cleanup_retired_streams(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM managed_run_event_streams WHERE sealed=1 AND acked_seq=next_event_seq-1
        AND NOT EXISTS(SELECT 1 FROM managed_runs WHERE managed_runs.id=run_id)",
        [],
    )?;
    Ok(())
}

pub(crate) fn admit(conn: &Connection, id: &str, request: &str) -> Result<()> {
    cleanup_retired_streams(conn)?;
    let retained: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM managed_run_event_streams WHERE run_id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if retained {
        return Err(Error::Conflict(
            "managed event run identity is retained".into(),
        ));
    }
    let value: Value = serde_json::from_str(request).unwrap_or(Value::Null);
    if value.get("event_delivery").is_none() {
        return Ok(());
    }
    let run = runs::get(conn, id)?;
    let retry_until = run
        .deadline_at
        .checked_add(7 * 86400)
        .ok_or_else(|| Error::Conflict("managed event retry deadline overflow".into()))?;
    conn.execute(
        "INSERT INTO managed_run_event_streams(run_id,delivery_json,retry_until) VALUES(?1,?2,?3)",
        params![id, value["event_delivery"].to_string(), retry_until],
    )?;
    let (pending, open, streams) = totals(conn)?;
    if pending + open * MAX_MANAGED_EVENT_BYTES as i64 > MAX_TOTAL_PENDING
        || streams > MAX_PENDING_STREAMS
    {
        return Err(Error::Conflict(
            "managed event retention capacity exhausted".into(),
        ));
    }
    Ok(())
}

/// Strip the host-only callback configuration from every wire receipt.
pub fn public_managed_run(mut run: ManagedRun) -> ManagedRun {
    if let Ok(mut request) = serde_json::from_str::<Value>(&run.request_json) {
        if let Some(object) = request.as_object_mut() {
            if object.remove("event_delivery").is_some() {
                run.request_json = request.to_string();
            }
        }
    }
    run
}

fn append(
    conn: &Connection,
    run: &ManagedRun,
    kind: &str,
    checkpoint: Option<Value>,
    now: i64,
) -> Result<()> {
    let source = stream(conn, &run.id)?;
    if source.sealed {
        return Err(Error::Conflict("managed event stream is sealed".into()));
    }
    let request: Value = serde_json::from_str(&run.request_json)?;
    let request_hash = request["event_delivery"]["request_hash"]
        .as_str()
        .ok_or_else(|| Error::Conflict("managed event binding missing".into()))?;
    let mut envelope = json!({"schema":1,"run_id":run.id,"sandbox_id":run.sandbox_id,
        "request_hash":request_hash,"event_seq":source.next_event_seq,"kind":kind,
        "receipt":public_managed_run(run.clone())});
    if let Some(value) = checkpoint {
        envelope["checkpoint"] = value;
    }
    let payload = envelope.to_string();
    if payload.len() > MAX_MANAGED_EVENT_BYTES {
        return Err(Error::Conflict("managed event exceeds size limit".into()));
    }
    let size = payload.len() as i64;
    let final_event = kind == "receipt";
    let per_run = MAX_RUN_PENDING
        - if final_event {
            0
        } else {
            MAX_MANAGED_EVENT_BYTES as i64
        };
    let (pending, open, _) = totals(conn)?;
    let remaining_reserve = (open - i64::from(final_event)) * MAX_MANAGED_EVENT_BYTES as i64;
    if source.pending_bytes as i64 + size > per_run
        || pending + size + remaining_reserve > MAX_TOTAL_PENDING
    {
        return Err(Error::Conflict("managed event outbox backpressure".into()));
    }
    let next = source
        .next_event_seq
        .checked_add(1)
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(|| Error::Conflict("managed event sequence exhausted".into()))?;
    conn.execute(
        "INSERT INTO managed_run_events(run_id,seq,payload_json,created_at) VALUES(?1,?2,?3,?4)",
        params![run.id, source.next_event_seq as i64, payload, now],
    )?;
    conn.execute("UPDATE managed_run_event_streams SET next_event_seq=?2,pending_bytes=pending_bytes+?3,sealed=?4 WHERE run_id=?1",
        params![run.id,next as i64,size,final_event])?;
    Ok(())
}

pub(crate) fn terminal(conn: &Connection, run: &ManagedRun, now: i64) -> Result<()> {
    let source = match stream(conn, &run.id) {
        Ok(value) => value,
        Err(Error::NotFound(_)) => return Ok(()),
        Err(error) => return Err(error),
    };
    let checkpoint = if source.source_failed {
        None
    } else {
        source
            .checkpoint_json
            .map(|value| serde_json::from_str(&value))
            .transpose()?
    };
    append(conn, run, "receipt", checkpoint, now)
}

impl Store {
    pub fn cleanup_managed_run_event_streams(&self) -> Result<()> {
        self.with_conn(cleanup_retired_streams)
    }

    /// Lightweight sender state; waiting workers must not load projections or
    /// payload copies until they own a global delivery permit.
    pub fn managed_run_event_delivery_state(&self, id: &str) -> Result<(bool, bool)> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT sealed=0 OR acked_seq<next_event_seq-1,pending_bytes>0
            FROM managed_run_event_streams WHERE run_id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or((false, false)))
        })
    }

    /// Private relay configuration is retained independently of VM deletion.
    /// This method is for the host sender, never an API response or log.
    pub fn managed_run_event_delivery(&self, id: &str) -> Result<(String, i64)> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT delivery_json,retry_until FROM managed_run_event_streams WHERE run_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }
    pub fn managed_run_event_stream(&self, id: &str) -> Result<ManagedRunEventStream> {
        self.with_conn(|conn| stream(conn, id))
    }

    pub fn list_managed_run_event_streams(&self) -> Result<Vec<String>> {
        self.with_conn(|conn|{
            let mut stmt=conn.prepare("SELECT run_id FROM managed_run_event_streams WHERE sealed=0 OR acked_seq<next_event_seq-1 ORDER BY run_id")?;
            let rows=stmt.query_map([],|r|r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// The guest cursor and full reconstructed projection commit with its event.
    /// A full outbox rolls back all three, so retry reads the same guest record.
    pub fn append_managed_run_checkpoint(
        &self,
        id: &str,
        epoch: i64,
        offset: u64,
        guest_seq: u64,
        checkpoint: &str,
        now: i64,
    ) -> Result<()> {
        if offset > i64::MAX as u64 || guest_seq > i64::MAX as u64 || checkpoint.len() > 262144 {
            return Err(Error::Conflict(
                "invalid managed checkpoint cursor or size".into(),
            ));
        }
        let value: Value = serde_json::from_str(checkpoint)?;
        self.transaction(|tx|{
            let conn=&tx.tx;
            let run=runs::get(conn,id)?;
            let source=stream(conn,id)?;
            if run.epoch!=epoch||run.finished_at.is_some()||source.source_failed||
                guest_seq!=source.guest_seq+1||offset<=source.source_offset {
                return Err(Error::Conflict("managed checkpoint cursor or epoch stale".into()));
            }
            append(conn,&run,"checkpoint",Some(value),now)?;
            conn.execute("UPDATE managed_run_event_streams SET source_offset=?2,guest_seq=?3,checkpoint_json=?4 WHERE run_id=?1",
                params![id,offset as i64,guest_seq as i64,checkpoint])?;
            Ok(())
        })
    }

    /// Corrupt/missing guest proof must not turn a successful process receipt
    /// into an apparently successful chat. Keep the host receipt and omit proof.
    pub fn fail_managed_run_event_source(&self, id: &str, epoch: i64) -> Result<()> {
        self.transaction(|tx|{
            let run=runs::get(&tx.tx,id)?;
            if run.epoch!=epoch||run.finished_at.is_some(){return Err(Error::Conflict("managed stream epoch stale".into()));}
            tx.tx.execute("UPDATE managed_run_event_streams SET source_failed=1,checkpoint_json=NULL WHERE run_id=?1",[id])?;
            Ok(())
        })
    }

    pub fn first_managed_run_event(&self, id: &str) -> Result<Option<ManagedRunEvent>> {
        self.with_conn(|conn|Ok(conn.query_row("SELECT run_id,seq,payload_json FROM managed_run_events WHERE run_id=?1 ORDER BY seq LIMIT 1",
            [id],|r|Ok(ManagedRunEvent{run_id:r.get(0)?,seq:r.get::<_, i64>(1)? as u64,payload_json:r.get(2)?})).optional()?))
    }

    /// Only acknowledgement of the first unacknowledged event advances the
    /// cursor. Lost ACKs replay identical persisted bytes; no cumulative guesses.
    pub fn acknowledge_managed_run_event(&self, id: &str, seq: u64) -> Result<()> {
        if seq > i64::MAX as u64 {
            return Err(Error::Conflict(
                "managed event acknowledgement out of range".into(),
            ));
        }
        self.transaction(|tx|{
            let conn=&tx.tx;let source=stream(conn,id)?;
            if seq<=source.acked_seq{return Ok(());}
            if seq!=source.acked_seq+1{return Err(Error::Conflict("managed event acknowledgement skipped sequence".into()));}
            let size:i64=conn.query_row("SELECT length(CAST(payload_json AS BLOB)) FROM managed_run_events WHERE run_id=?1 AND seq=?2",
                params![id,seq as i64],|r|r.get(0)).optional()?.ok_or_else(||Error::Conflict("managed event acknowledgement missing".into()))?;
            conn.execute("DELETE FROM managed_run_events WHERE run_id=?1 AND seq=?2",params![id,seq as i64])?;
            conn.execute("UPDATE managed_run_event_streams SET acked_seq=?2,pending_bytes=pending_bytes-?3 WHERE run_id=?1",params![id,seq as i64,size])?;
            if source.sealed&&seq==source.next_event_seq-1 {
                conn.execute("UPDATE managed_run_event_streams SET delivery_json='{}',checkpoint_json=NULL WHERE run_id=?1",[id])?;
                // Explicit VM deletion retires receipt identity. Once its final
                // delivery is acknowledged, no retained payload needs this row.
                conn.execute("DELETE FROM managed_run_event_streams WHERE run_id=?1 AND NOT EXISTS(SELECT 1 FROM managed_runs WHERE id=?1)",[id])?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests;
