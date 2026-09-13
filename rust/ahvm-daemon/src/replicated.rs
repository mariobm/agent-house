//! Deferred storage deletion. No quota release on an RPC acknowledgement.
use crate::{unix_now, AppState};
use ahvm_store::{Error, ReplicatedReservation, Store};

// Caller holds the sandbox lifecycle lock. Removing a leftover row first makes
// interruption conservative: a crash can retain quota, never free it early.
fn finish(store: &Store, row: &ReplicatedReservation, complete: bool) -> ahvm_store::Result<bool> {
    if !complete {
        return Ok(false);
    }
    if store
        .replicated_reservation(&row.owner_user_id, &row.volume_id)?
        .state
        != "deleting"
    {
        return Err(Error::Conflict("storage deletion no longer pending".into()));
    }
    match store.get_sandbox(&row.sandbox_id) {
        Ok(sandbox) if sandbox.owner_user_id == row.owner_user_id => {
            store.delete_sandbox(&row.sandbox_id)?
        }
        Ok(_) => return Err(Error::Conflict("storage deletion owner mismatch".into())),
        Err(Error::NotFound(_)) => (),
        Err(e) => return Err(e),
    }
    store.confirm_replicated_reclamation(&row.owner_user_id, &row.volume_id, unix_now())?;
    Ok(true)
}

/// Separate from the thermal sweep so storage failures cannot delay idle stops.
/// One bounded page per pass; per-sandbox lifecycle locks and operation permits
/// are acquired without waiting behind foreground work.
pub async fn run(state: AppState) -> ! {
    let mut after: Option<String> = None;
    loop {
        match state
            .store
            .retained_replicated_reservations(after.as_deref(), 100)
        {
            Ok(rows) => {
                after = if rows.len() == 100 {
                    rows.last().map(|r| r.volume_id.clone())
                } else {
                    None
                };
                for row in rows.into_iter().filter(|r| r.state == "deleting") {
                    let Some(_lifecycle) = state.lifecycle.try_lock(&row.sandbox_id) else {
                        continue;
                    };
                    if state
                        .store
                        .check_lifecycle_fence(&row.sandbox_id, None)
                        .is_err()
                    {
                        continue;
                    }
                    let Some(_permit) = state.ops.try_acquire() else {
                        continue;
                    };
                    let backend = state.backend.clone();
                    let request = row.clone();
                    let result = crate::blocking(move || {
                        backend.reclaim_replicated_volume(
                            &request.sandbox_id,
                            &request.volume_id,
                            request.logical_bytes as u64,
                        )
                    })
                    .await;
                    match result {
                        Ok(complete) => match finish(&state.store, &row, complete) {
                            Ok(true) => state.activity.remove(&row.sandbox_id),
                            Ok(false) => (),
                            Err(e) => eprintln!("storage reconciliation {}: {e}", row.volume_id),
                        },
                        Err(e) => eprintln!("storage retirement {}: {e}", row.volume_id),
                    }
                }
            }
            Err(e) => eprintln!("storage reservation scan: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Store, ReplicatedReservation) {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_user(&ahvm_store::User {
                id: "u".into(),
                name: "u".into(),
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
        let row = store
            .reserve_replicated_volume("u", "vm", &"a".repeat(64), 65536, 0)
            .unwrap();
        (store, row)
    }
    #[test]
    fn acknowledgement_keeps_quota_and_confirmed_cleanup_releases_it() {
        let (store, row) = fixture();
        assert!(finish(&store, &row, true).is_err());
        store
            .delete_replicated_reservation("u", &row.volume_id, 1)
            .unwrap();
        assert!(!finish(&store, &row, false).unwrap());
        assert_eq!(store.replicated_usage("u").unwrap().logical_bytes, 65536);
        assert!(finish(&store, &row, true).unwrap());
        assert_eq!(store.replicated_usage("u").unwrap().logical_bytes, 0);
    }
}
