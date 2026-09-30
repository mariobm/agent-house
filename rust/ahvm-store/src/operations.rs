//! Durable receipts survive sandbox deletion. Never expire a receipt while
//! clients may replay its key; a delayed request must not resurrect a VM.
use crate::{Error, Result, Store};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct LifecycleOperation {
    pub id: String,
    pub owner_user_id: String,
    pub sandbox_id: String,
    pub request: String,
    pub state: String,
    pub status: Option<u16>,
    pub error: Option<serde_json::Value>,
    pub sandbox_state: Option<String>,
    pub storage_volume_id: Option<String>,
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LifecycleOperation> {
    Ok(LifecycleOperation {
        id: r.get(0)?,
        owner_user_id: r.get(1)?,
        sandbox_id: r.get(2)?,
        request: r.get(3)?,
        state: r.get(4)?,
        status: r.get(5)?,
        error: r
            .get::<_, Option<String>>(6)?
            .map(|json| {
                serde_json::from_str(&json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })
            .transpose()?,
        sandbox_state: r.get(7)?,
        storage_volume_id: r.get(8)?,
    })
}
impl Store {
    pub fn check_lifecycle_fence(&self, sandbox: &str, operation: Option<&str>) -> Result<()> {
        self.with_conn(|c| {
            let active: Option<String> = c
                .query_row(
                    "SELECT id FROM lifecycle_operations WHERE sandbox_id=?1 AND state='pending'",
                    [sandbox],
                    |r| r.get(0),
                )
                .optional()?;
            if active.as_deref().is_some_and(|id| Some(id) != operation) {
                return Err(Error::Conflict(
                    "durable lifecycle operation in progress".into(),
                ));
            }
            Ok(())
        })
    }

    pub fn lifecycle_operation(&self, id: &str, owner: &str) -> Result<LifecycleOperation> {
        self.with_conn(|c| c.query_row("SELECT id,owner_user_id,sandbox_id,request,state,status,error_json,sandbox_state,storage_volume_id FROM lifecycle_operations WHERE id=?1 AND owner_user_id=?2",params![id,owner],row).optional()?.ok_or_else(|| Error::NotFound("operation".into())))
    }
    /// Return true only to the caller that durably admitted this exact request.
    pub fn admit_lifecycle_operation(&self, op: &LifecycleOperation, now: i64) -> Result<bool> {
        self.with_conn(|c| {
            let existing=c.query_row("SELECT id,owner_user_id,sandbox_id,request,state,status,error_json,sandbox_state,storage_volume_id FROM lifecycle_operations WHERE id=?1",[&op.id],row).optional()?;
            if let Some(old)=existing {
                return if old.owner_user_id==op.owner_user_id && old.request==op.request && old.sandbox_id==op.sandbox_id {Ok(false)} else {Err(Error::Conflict("operation key reused".into()))};
            }
            let inserted=c.execute("INSERT OR IGNORE INTO lifecycle_operations(id,owner_user_id,sandbox_id,request,state,created_at,storage_volume_id) SELECT ?1,?2,?3,?4,'pending',?5,(SELECT volume_id FROM replicated_reservations WHERE owner_user_id=?2 AND sandbox_id=?3 AND state!='reclaimed' AND logical_released=0) WHERE (SELECT count(*) FROM lifecycle_operations WHERE state='pending')<64",params![op.id,op.owner_user_id,op.sandbox_id,op.request,now])?;
            if inserted==0 {return Err(Error::Conflict("operation already in progress or queue full".into()));}
            Ok(true)
        })
    }
    pub fn finish_lifecycle_operation(&self, id: &str, status: u16) -> Result<()> {
        self.finish_lifecycle_operation_with_error(id, status, None, None, None)
    }
    pub fn finish_lifecycle_operation_with_error(
        &self,
        id: &str,
        status: u16,
        error: Option<&serde_json::Value>,
        sandbox_state: Option<&str>,
        storage_volume_id: Option<&str>,
    ) -> Result<()> {
        let error = error.map(serde_json::to_string).transpose()?;
        self.with_conn(|c| {c.execute("UPDATE lifecycle_operations SET state='done',status=?2,error_json=?3,sandbox_state=?4,storage_volume_id=COALESCE(storage_volume_id,?5) WHERE id=?1 AND state='pending'",params![id,status,error,sandbox_state,storage_volume_id])?;Ok(())})
    }
    /// Call exactly once at daemon startup, before serving requests. The previous
    /// process can no longer execute requests. Keep tombstones, never replay an
    /// interrupted command merely because the outcome was not recorded.
    pub fn interrupt_lifecycle_operations(&self) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE lifecycle_operations SET state='interrupted' WHERE state='pending'",
                [],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipts_persist_across_database_reopen() {
        let path = std::env::temp_dir().join(format!(
            "ahvm-receipt-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let op = LifecycleOperation {
            id: "persisted".into(),
            owner_user_id: "alice".into(),
            sandbox_id: "vm".into(),
            request: "create".into(),
            state: "pending".into(),
            status: None,
            error: None,
            sandbox_state: None,
            storage_volume_id: None,
        };
        {
            let store = Store::open(&path).unwrap();
            assert!(store.admit_lifecycle_operation(&op, 1).unwrap());
        }
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(
                store
                    .lifecycle_operation("persisted", "alice")
                    .unwrap()
                    .state,
                "pending"
            );
            store.interrupt_lifecycle_operations().unwrap();
        }
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(
                store
                    .lifecycle_operation("persisted", "alice")
                    .unwrap()
                    .state,
                "interrupted"
            );
            assert!(!store.admit_lifecycle_operation(&op, 1).unwrap());
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn terminal_error_and_identity_snapshot_survive_restart_and_cannot_be_overwritten() {
        let path = std::env::temp_dir().join(format!(
            "ahvm-terminal-receipt-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let op = LifecycleOperation {
            id: "failed".into(),
            owner_user_id: "alice".into(),
            sandbox_id: "desktop".into(),
            request: "create".into(),
            state: "pending".into(),
            status: None,
            error: None,
            sandbox_state: None,
            storage_volume_id: None,
        };
        let error = serde_json::json!({"code":"disk_quota_reached","message":"replicated storage quota exceeded"});
        let store = Store::open(&path).unwrap();
        store.admit_lifecycle_operation(&op, 1).unwrap();
        store
            .finish_lifecycle_operation_with_error(
                "failed",
                403,
                Some(&error),
                Some("absent"),
                Some(&"a".repeat(64)),
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        store
            .finish_lifecycle_operation_with_error("failed", 201, None, Some("running"), None)
            .unwrap();
        let receipt = store.lifecycle_operation("failed", "alice").unwrap();
        assert_eq!(receipt.status, Some(403));
        assert_eq!(receipt.error, Some(error));
        assert_eq!(receipt.sandbox_state.as_deref(), Some("absent"));
        assert_eq!(
            receipt.storage_volume_id.as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert!(!store.admit_lifecycle_operation(&op, 2).unwrap());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pending_replicated_identity_survives_interruption_before_terminal_receipt() {
        let path = std::env::temp_dir().join(format!(
            "ahvm-pending-volume-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Store::open(&path).unwrap();
        store
            .upsert_user(&crate::User {
                id: "alice".into(),
                name: "alice".into(),
                api_key_hash: "unused".into(),
                max_sandboxes: 1,
                max_cpus: 1,
                max_memory_mb: 512,
                max_volumes_mb: 1,
                max_snapshots: 1,
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        let op = LifecycleOperation {
            id: "pending-create".into(),
            owner_user_id: "alice".into(),
            sandbox_id: "vm".into(),
            request: "create".into(),
            state: "pending".into(),
            status: None,
            error: None,
            sandbox_state: None,
            storage_volume_id: None,
        };
        store.admit_lifecycle_operation(&op, 0).unwrap();
        let volume = "a".repeat(64);
        store
            .reserve_replicated_volume("alice", "vm", &volume, 65536, 0)
            .unwrap();
        assert_eq!(
            store
                .lifecycle_operation(&op.id, "alice")
                .unwrap()
                .storage_volume_id
                .as_deref(),
            Some(volume.as_str())
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        store.interrupt_lifecycle_operations().unwrap();
        let receipt = store.lifecycle_operation(&op.id, "alice").unwrap();
        assert_eq!(receipt.state, "interrupted");
        assert_eq!(receipt.storage_volume_id.as_deref(), Some(volume.as_str()));
        store
            .delete_replicated_reservation("alice", &volume, 1)
            .unwrap();
        store
            .confirm_replicated_retirement("alice", &volume, 2, true, false)
            .unwrap();
        assert!(
            store
                .replicated_reservation("alice", receipt.storage_volume_id.as_ref().unwrap())
                .unwrap()
                .logical_released
        );
        let new = "b".repeat(64);
        store
            .reserve_replicated_volume("alice", "vm", &new, 65536, 3)
            .unwrap();
        let delete = LifecycleOperation {
            id: "pending-delete".into(),
            request: "delete".into(),
            ..op
        };
        store.admit_lifecycle_operation(&delete, 3).unwrap();
        assert_eq!(
            store
                .lifecycle_operation(&delete.id, "alice")
                .unwrap()
                .storage_volume_id
                .as_deref(),
            Some(new.as_str())
        );
        store
            .finish_lifecycle_operation_with_error(
                &delete.id,
                502,
                None,
                Some("failed"),
                Some(&volume),
            )
            .unwrap();
        assert_eq!(
            store
                .lifecycle_operation(&delete.id, "alice")
                .unwrap()
                .storage_volume_id
                .as_deref(),
            Some(new.as_str())
        );
        drop(store);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn legacy_receipts_migrate_without_inventing_generation_evidence() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE lifecycle_operations(id TEXT PRIMARY KEY,owner_user_id TEXT NOT NULL,sandbox_id TEXT NOT NULL,request TEXT NOT NULL,state TEXT NOT NULL,status INTEGER,created_at INTEGER NOT NULL);
            INSERT INTO lifecycle_operations VALUES('legacy','alice','vm','create','done',201,0);").unwrap();
        let store = Store::from_conn(conn).unwrap();
        let receipt = store.lifecycle_operation("legacy", "alice").unwrap();
        assert_eq!(receipt.status, Some(201));
        assert!(receipt.error.is_none());
        assert!(receipt.sandbox_state.is_none());
        assert!(receipt.storage_volume_id.is_none());
    }

    #[test]
    fn admission_binds_keys_serializes_and_preserves_restart_tombstones() {
        let store = Store::open_in_memory().unwrap();
        let mut a = LifecycleOperation {
            id: "first".into(),
            owner_user_id: "alice".into(),
            sandbox_id: "vm".into(),
            request: "create".into(),
            state: "pending".into(),
            status: None,
            error: None,
            sandbox_state: None,
            storage_volume_id: None,
        };
        assert!(store.admit_lifecycle_operation(&a, 1).unwrap());
        assert!(!store.admit_lifecycle_operation(&a, 1).unwrap());
        a.owner_user_id = "bob".into();
        assert!(store.admit_lifecycle_operation(&a, 1).is_err());
        a.owner_user_id = "alice".into();
        a.id = "second".into();
        assert!(store.admit_lifecycle_operation(&a, 1).is_err());
        store.interrupt_lifecycle_operations().unwrap();
        a.id = "first".into();
        assert!(!store.admit_lifecycle_operation(&a, 1).unwrap());
        assert_eq!(
            store.lifecycle_operation("first", "alice").unwrap().state,
            "interrupted"
        );
        a.id = "second".into();
        assert!(store.admit_lifecycle_operation(&a, 1).unwrap());
        store.finish_lifecycle_operation("second", 200).unwrap();
        assert!(!store.admit_lifecycle_operation(&a, 1).unwrap());
    }
}
