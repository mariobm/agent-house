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
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LifecycleOperation> {
    Ok(LifecycleOperation {
        id: r.get(0)?,
        owner_user_id: r.get(1)?,
        sandbox_id: r.get(2)?,
        request: r.get(3)?,
        state: r.get(4)?,
        status: r.get(5)?,
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
        self.with_conn(|c| c.query_row("SELECT id,owner_user_id,sandbox_id,request,state,status FROM lifecycle_operations WHERE id=?1 AND owner_user_id=?2",params![id,owner],row).optional()?.ok_or_else(|| Error::NotFound("operation".into())))
    }
    /// Return true only to the caller that durably admitted this exact request.
    pub fn admit_lifecycle_operation(&self, op: &LifecycleOperation, now: i64) -> Result<bool> {
        self.with_conn(|c| {
            let existing=c.query_row("SELECT id,owner_user_id,sandbox_id,request,state,status FROM lifecycle_operations WHERE id=?1",[&op.id],row).optional()?;
            if let Some(old)=existing {
                return if old.owner_user_id==op.owner_user_id && old.request==op.request && old.sandbox_id==op.sandbox_id {Ok(false)} else {Err(Error::Conflict("operation key reused".into()))};
            }
            let inserted=c.execute("INSERT OR IGNORE INTO lifecycle_operations(id,owner_user_id,sandbox_id,request,state,created_at) SELECT ?1,?2,?3,?4,'pending',?5 WHERE (SELECT count(*) FROM lifecycle_operations WHERE state='pending')<64",params![op.id,op.owner_user_id,op.sandbox_id,op.request,now])?;
            if inserted==0 {return Err(Error::Conflict("operation already in progress or queue full".into()));}
            Ok(true)
        })
    }
    pub fn finish_lifecycle_operation(&self, id: &str, status: u16) -> Result<()> {
        self.with_conn(|c| {c.execute("UPDATE lifecycle_operations SET state='done',status=?2 WHERE id=?1 AND state='pending'",params![id,status])?;Ok(())})
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
    fn admission_binds_keys_serializes_and_preserves_restart_tombstones() {
        let store = Store::open_in_memory().unwrap();
        let mut a = LifecycleOperation {
            id: "first".into(),
            owner_user_id: "alice".into(),
            sandbox_id: "vm".into(),
            request: "create".into(),
            state: "pending".into(),
            status: None,
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
