//! Durable tenant admission before replicated-disk import. No per-I/O accounting.
//! Only verified remote/local reclamation releases a reservation; failed creates,
//! stopped/evicted disks and missing sandbox rows still consume logical capacity.
use crate::{Error, Result, Store};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplicatedReservation {
    pub volume_id: String,
    pub owner_user_id: String,
    pub sandbox_id: String,
    pub logical_bytes: i64,
    pub state: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReplicatedUsage {
    pub retained_volumes: i64,
    pub logical_bytes: i64,
}
fn find(conn: &Connection, id: &str) -> Result<Option<ReplicatedReservation>> {
    Ok(conn.query_row(
        "SELECT volume_id,owner_user_id,sandbox_id,logical_bytes,state FROM replicated_reservations WHERE volume_id=?1",
        [id], |r| Ok(ReplicatedReservation {volume_id:r.get(0)?,owner_user_id:r.get(1)?,sandbox_id:r.get(2)?,logical_bytes:r.get(3)?,state:r.get(4)?})
    ).optional()?)
}
fn usage(conn: &Connection, owner: &str) -> Result<ReplicatedUsage> {
    Ok(conn.query_row(
        "SELECT COUNT(*),COALESCE(SUM(logical_bytes),0) FROM replicated_reservations WHERE owner_user_id=?1 AND state!='reclaimed'",
        [owner], |r| Ok(ReplicatedUsage {retained_volumes:r.get(0)?,logical_bytes:r.get(1)?})
    )?)
}
impl Store {
    /// Host-global name fence, including disks whose sandbox row is gone.
    pub fn check_replicated_name_available(&self, sandbox: &str) -> Result<()> {
        let occupied: bool = self.with_conn(|conn| Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM replicated_reservations WHERE sandbox_id=?1 AND state!='reclaimed')",
            [sandbox], |r| r.get(0),
        )?))?;
        if occupied {
            return Err(Error::Conflict("sandbox storage cleanup is pending".into()));
        }
        Ok(())
    }

    /// Host-internal admission. Obtain owner from authenticated context and size
    /// from the validated image, not from a user-supplied accounting claim. Mint
    /// the immutable volume ID before import and persist this reservation first.
    /// IMMEDIATE serializes even independently opened store connections.
    pub fn reserve_replicated_volume(
        &self,
        owner: &str,
        sandbox: &str,
        volume: &str,
        bytes: i64,
        now: i64,
    ) -> Result<ReplicatedReservation> {
        if volume.len() != 64
            || !volume
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || owner.is_empty()
            || sandbox.is_empty()
            || sandbox.len() > 64
            || bytes <= 0
            || bytes > 64 * 1024 * 1024 * 1024
            || bytes % 65536 != 0
        {
            return Err(Error::Conflict("invalid replicated reservation".into()));
        }
        self.transaction(|tx| {
            if let Some(old) = find(&tx.tx, volume)? {
                if old.owner_user_id == owner
                    && old.sandbox_id == sandbox
                    && old.logical_bytes == bytes
                    && old.state == "reserved"
                {
                    // Existing disks survive lowered quotas without a new charge.
                    return Ok(old);
                }
                return Err(Error::Conflict(
                    "replicated volume identity already reserved or retired".into(),
                ));
            }
            let quota: Option<i64> = tx.tx.query_row(
                "SELECT max_volumes_mb FROM users WHERE id=?1",
                [owner], |r| r.get(0),
            ).optional()?;
            let maximum = quota.ok_or_else(|| Error::NotFound("storage owner".into()))?
                .checked_mul(1024 * 1024).filter(|n| *n >= 0)
                .ok_or_else(|| Error::Conflict("invalid storage quota".into()))?;
            // Invalid legacy sizes cannot subtract from the shared quota.
            let (legacy_mb, smallest): (i64, i64) = tx.tx.query_row(
                "SELECT COALESCE(SUM(size_mb),0),COALESCE(MIN(size_mb),0) FROM volumes WHERE owner_user_id=?1",
                [owner], |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let legacy_bytes = legacy_mb.checked_mul(1024 * 1024)
                .filter(|_| smallest >= 0)
                .ok_or_else(|| Error::Conflict("invalid existing volume usage".into()))?;
            let total = usage(&tx.tx, owner)?.logical_bytes.checked_add(legacy_bytes)
                .and_then(|n| n.checked_add(bytes))
                .ok_or_else(|| Error::Conflict("storage accounting overflow".into()))?;
            if total > maximum {
                return Err(Error::Conflict("replicated storage quota exceeded".into()));
            }
            let occupied: bool = tx.tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM replicated_reservations WHERE sandbox_id=?1 AND state!='reclaimed')",
                [sandbox], |r| r.get(0),
            )?;
            if occupied {
                return Err(Error::Conflict("sandbox already has a retained replicated disk".into()));
            }
            tx.tx.execute(
                "INSERT INTO replicated_reservations(volume_id,owner_user_id,sandbox_id,logical_bytes,state,created_at,updated_at) VALUES(?1,?2,?3,?4,'reserved',?5,?5)",
                params![volume, owner, sandbox, bytes, now],
            )?;
            Ok(find(&tx.tx, volume)?.expect("inserted reservation"))
        })
    }

    pub fn replicated_usage(&self, owner: &str) -> Result<ReplicatedUsage> {
        self.with_conn(|conn| usage(conn, owner))
    }
    /// Host-internal recovery scan. Bounded cursor pagination includes failed
    /// imports and pending deletions even when no sandbox row exists.
    pub fn retained_replicated_reservations(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ReplicatedReservation>> {
        if !(1..=1000).contains(&limit) {
            return Err(Error::Conflict("invalid reservation page size".into()));
        }
        self.with_conn(|conn| {
            let mut query = conn.prepare(
                "SELECT volume_id,owner_user_id,sandbox_id,logical_bytes,state FROM replicated_reservations WHERE state!='reclaimed' AND (?1 IS NULL OR volume_id>?1) ORDER BY volume_id LIMIT ?2",
            )?;
            let rows = query.query_map(params![after, limit as i64], |r| Ok(ReplicatedReservation {
                volume_id: r.get(0)?, owner_user_id: r.get(1)?, sandbox_id: r.get(2)?, logical_bytes: r.get(3)?, state: r.get(4)?,
            }))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
    pub fn replicated_for_sandbox(
        &self,
        owner: &str,
        sandbox: &str,
    ) -> Result<Option<ReplicatedReservation>> {
        self.with_conn(|conn| {
            let id: Option<String> = conn.query_row(
                "SELECT volume_id FROM replicated_reservations WHERE owner_user_id=?1 AND sandbox_id=?2 AND state!='reclaimed'",
                params![owner, sandbox], |r| r.get(0),
            ).optional()?;
            match id { Some(id) => find(conn, &id), None => Ok(None) }
        })
    }
    /// Owner-scoped lookup: foreign IDs are indistinguishable from missing IDs.
    pub fn replicated_reservation(
        &self,
        owner: &str,
        volume: &str,
    ) -> Result<ReplicatedReservation> {
        self.with_conn(|conn| {
            find(conn, volume)?
                .filter(|r| r.owner_user_id == owner)
                .ok_or_else(|| Error::NotFound("replicated volume".into()))
        })
    }
    /// Record deletion intent before asking the supervisor to delete. This does
    /// not release quota, even if the sandbox row is already gone.
    pub fn delete_replicated_reservation(&self, owner: &str, volume: &str, now: i64) -> Result<()> {
        self.transition_replicated(owner, volume, now, false)
    }
    /// Trusted reconciler only: call AFTER the supervisor confirms both remote
    /// and local reclamation. Never call on a delete acknowledgement or timeout.
    pub fn confirm_replicated_reclamation(
        &self,
        owner: &str,
        volume: &str,
        now: i64,
    ) -> Result<()> {
        self.transition_replicated(owner, volume, now, true)
    }
    fn transition_replicated(
        &self,
        owner: &str,
        volume: &str,
        now: i64,
        reclaimed: bool,
    ) -> Result<()> {
        self.transaction(|tx| {
            let row = find(&tx.tx, volume)?
                .filter(|r| r.owner_user_id == owner)
                .ok_or_else(|| Error::NotFound("replicated volume".into()))?;
            if row.state == "reclaimed" {
                return Ok(());
            }
            if reclaimed && row.state != "deleting" {
                return Err(Error::Conflict("replicated deletion not requested".into()));
            }
            tx.tx.execute(
                "UPDATE replicated_reservations SET state=?2,updated_at=?3 WHERE volume_id=?1",
                params![
                    volume,
                    if reclaimed { "reclaimed" } else { "deleting" },
                    now
                ],
            )?;
            Ok(())
        })
    }
}
