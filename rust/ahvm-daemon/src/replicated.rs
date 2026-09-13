//! Deferred storage deletion. No quota release on an RPC acknowledgement.
use crate::{unix_now, AppState};
use ahvm_store::{Error, ReplicatedReservation, Store};

/// Caller holds the lifecycle lock and operation permit through the commit,
/// including when the requesting HTTP client disconnects.
pub(crate) fn create(
    state: &AppState,
    owner: &str,
    spec: &ahvm_engine::SandboxSpec,
) -> crate::ApiResult<(ahvm_store::Sandbox, ahvm_engine::SandboxInfo)> {
    let me = state.store.get_user(owner)?;
    let _hold = state.quotas.reserve_sandbox(
        &state.store,
        &me,
        &spec.name,
        spec.cpus as i64,
        spec.memory_mb as i64,
    )?;
    let mut reserved = None;
    let mut admission_error = None;
    let result = state
        .backend
        .create_with_storage_admission(spec, &mut |volume, bytes| {
            if reserved.is_some() {
                return Err(ahvm_engine::Error::Conflict(
                    "duplicate storage admission".into(),
                ));
            }
            match state.store.reserve_replicated_volume(
                owner,
                &spec.name,
                volume,
                bytes as i64,
                unix_now(),
            ) {
                Ok(row) => {
                    reserved = Some(row);
                    Ok(())
                }
                Err(e) => {
                    admission_error = Some(match e {
                        Error::Conflict(ref message)
                            if message == "replicated storage quota exceeded" =>
                        {
                            crate::ApiError::Forbidden(message.clone())
                        }
                        other => other.into(),
                    });
                    Err(ahvm_engine::Error::Conflict(
                        "storage admission refused".into(),
                    ))
                }
            }
        });
    let info = match result {
        Ok(info) => info,
        Err(e) => {
            cleanup_create(state, reserved.as_ref());
            return Err(admission_error.unwrap_or_else(|| e.into()));
        }
    };
    let now = unix_now();
    let row = ahvm_store::Sandbox {
        id: info.id.clone(),
        owner_user_id: owner.into(),
        name: info.name.clone(),
        backend: ahvm_store::Backend::Krucible,
        state: crate::state_str(&info.state),
        thermal: crate::thermal_str(&info.thermal),
        cpus: spec.cpus as i64,
        memory_mb: spec.memory_mb as i64,
        ip: info.ip.clone(),
        created_at: now,
        updated_at: now,
    };
    if let Err(e) = state.store.create_sandbox(&row) {
        if reserved.is_some() {
            cleanup_create(state, reserved.as_ref());
        } else {
            let _ = state.backend.destroy(&info.id);
        }
        return Err(e.into());
    }
    state.activity.touch(&info.id);
    Ok((row, info))
}

fn cleanup_create(state: &AppState, reserved: Option<&ReplicatedReservation>) {
    if let Some(row) = reserved {
        // Never free capacity on failure or delete acknowledgement. If this
        // store write fails, orphan recovery still sees the reserved row.
        if let Err(e) = state.store.delete_replicated_reservation(
            &row.owner_user_id,
            &row.volume_id,
            unix_now(),
        ) {
            eprintln!("failed-create storage intent {}: {e}", row.volume_id);
            return;
        }
        let _ = destroy_matching(state.backend.as_ref(), row);
    }
}

fn destroy_matching(
    backend: &dyn ahvm_engine::Backend,
    row: &ReplicatedReservation,
) -> ahvm_engine::Result<()> {
    match backend.status(&row.sandbox_id) {
        Ok(info) if info.storage.volume_id.as_deref() == Some(&row.volume_id) => {
            backend.destroy(&row.sandbox_id)
        }
        Ok(_) => Err(ahvm_engine::Error::Conflict(
            "storage cleanup identity mismatch".into(),
        )),
        Err(ahvm_engine::Error::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// A crash between reservation and sandbox commit leaves a charged orphan.
/// Lifecycle serialization prevents confusing an active create with one.
fn mark_orphan(store: &Store, row: &ReplicatedReservation) -> ahvm_store::Result<bool> {
    if row.state == "deleting" {
        return Ok(true);
    }
    match store.get_sandbox(&row.sandbox_id) {
        Ok(_) => Ok(false),
        Err(Error::NotFound(_)) => {
            store.delete_replicated_reservation(&row.owner_user_id, &row.volume_id, unix_now())?;
            Ok(true)
        }
        Err(e) => Err(e),
    }
}

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
                for row in rows {
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
                    match mark_orphan(&state.store, &row) {
                        Ok(true) => (),
                        Ok(false) => continue,
                        Err(e) => {
                            eprintln!("storage orphan {}: {e}", row.volume_id);
                            continue;
                        }
                    }
                    let backend = state.backend.clone();
                    let request = row.clone();
                    let result = crate::blocking(move || {
                        // A failed early create may still have a retained engine
                        // record. Only its exact immutable volume may be destroyed.
                        // Even if destroy fails, retirement may repair a never-
                        // registered volume after destroy saved its delete intent.
                        let _ = destroy_matching(backend.as_ref(), &request);
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
    fn orphan_reservation_remains_charged_and_blocks_name_until_cleanup() {
        let (store, row) = fixture();
        assert!(store.check_replicated_name_available("vm").is_err());
        assert!(mark_orphan(&store, &row).unwrap());
        assert_eq!(
            store
                .replicated_reservation("u", &row.volume_id)
                .unwrap()
                .state,
            "deleting"
        );
        assert_eq!(store.replicated_usage("u").unwrap().logical_bytes, 65536);
        assert!(!finish(&store, &row, false).unwrap());
        assert!(store.check_replicated_name_available("vm").is_err());
        finish(&store, &row, true).unwrap();
        store.check_replicated_name_available("vm").unwrap();
    }

    #[test]
    fn committed_sandbox_is_not_an_orphan() {
        let (store, row) = fixture();
        store
            .create_sandbox(&ahvm_store::Sandbox {
                id: "vm".into(),
                owner_user_id: "u".into(),
                name: "vm".into(),
                backend: ahvm_store::Backend::Krucible,
                state: "stopped".into(),
                thermal: "cold".into(),
                cpus: 1,
                memory_mb: 512,
                ip: "".into(),
                created_at: 0,
                updated_at: 0,
            })
            .unwrap();
        assert!(!mark_orphan(&store, &row).unwrap());
        assert_eq!(
            store
                .replicated_reservation("u", &row.volume_id)
                .unwrap()
                .state,
            "reserved"
        );
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
