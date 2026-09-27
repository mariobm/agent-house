//! Durable, owner-charged snapshots with cancellation-safe publication and cleanup.

use crate::{auth::UserId, routes::owned, unix_now, ApiError, ApiResult, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct SnapshotView {
    pub id: String,
    pub name: String,
    pub sandbox_id: Option<String>,
    pub state: String,
    pub local_bytes: i64,
    pub created_at: i64,
}

fn view(row: &ahvm_store::Snapshot) -> SnapshotView {
    SnapshotView {
        id: row.id.clone(),
        name: row.name.clone(),
        sandbox_id: row.sandbox_id.clone(),
        state: row.state.clone(),
        local_bytes: row.local_bytes,
        created_at: row.created_at,
    }
}

fn owned_snapshot(
    state: &AppState,
    user: &str,
    snapshot_id: &str,
) -> ApiResult<ahvm_store::Snapshot> {
    match state.store.get_snapshot(snapshot_id) {
        Ok(row) if row.owner_user_id == user => Ok(row),
        Ok(_) => Err(crate::ApiError::NotFound(format!("snapshot {snapshot_id}"))),
        Err(ahvm_store::Error::NotFound(_)) => {
            Err(crate::ApiError::NotFound(format!("snapshot {snapshot_id}")))
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(body): Json<CreateBody>,
) -> ApiResult<impl IntoResponse> {
    if body.name.is_empty()
        || body.name.len() > 64
        || !body
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::Invalid(
            "name must be 1..=64 letters, digits, hyphens or underscores".into(),
        ));
    }
    owned(&state, &user.0, &id).await?;
    let flight = crate::routes::guest(&state, &user.0, &id).await?;
    let snapshot_lock = state
        .lifecycle
        .lock(&format!("snapshot:{}", body.name))
        .await;
    let permit = state.ops.acquire().await;
    let row = tokio::task::spawn_blocking(move || -> ApiResult<_> {
        let (snapshot_lock, _flight, _permit) = (snapshot_lock, flight, permit);
        let me = state.store.get_user(&user.0)?;
        let hold = state
            .quotas
            .reserve_snapshot(&state.store, &me, &body.name)?;
        let mut row = ahvm_store::Snapshot {
            id: body.name.clone(),
            owner_user_id: user.0,
            sandbox_id: Some(id.clone()),
            name: body.name,
            kind: "manual".into(),
            state: "creating".into(),
            local_bytes: 0,
            remote_state: "local".into(),
            remote_manifest_key: None,
            created_at: unix_now(),
            expires_at: None,
        };
        // Persist ownership and quota before producing files. The blocking task
        // owns publication/cleanup even if its HTTP request disappears.
        state.store.create_snapshot(&row)?;
        drop(hold);
        if let Err(error) = state.backend.create_snapshot(&id, &row.id) {
            // A partial backend publication still belongs to this charged row.
            // Cleanup errors retain it for explicit retry or startup recovery.
            match cleanup(&state, &row.id) {
                Ok(()) => snapshot_lock.invalidate_identity(),
                Err(cleanup) => eprintln!("snapshot {} cleanup deferred: {cleanup}", row.id),
            }
            return Err(error.into());
        }
        row.local_bytes = retained_bytes(&state, &row.id)?;
        row.state = "ready".into();
        state
            .store
            .set_snapshot_state(&row.id, &row.state, row.local_bytes)?;
        Ok(row)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("snapshot task: {e}")))??;
    Ok((StatusCode::CREATED, Json(view(&row))))
}

fn retained_bytes(state: &AppState, id: &str) -> ApiResult<i64> {
    i64::try_from(state.backend.snapshot_local_bytes(id)?)
        .map_err(|_| ApiError::Internal("snapshot size exceeds accounting range".into()))
}

fn cleanup(state: &AppState, id: &str) -> ApiResult<()> {
    let row = state.store.get_snapshot(id)?;
    let bytes = retained_bytes(state, id).unwrap_or(row.local_bytes);
    state.store.set_snapshot_state(id, "deleting", bytes)?;
    state.backend.delete_snapshot(id)?;
    state.store.delete_snapshot(id)?;
    Ok(())
}

/// Called once before accepting requests. Pending rows retain quota across a
/// crash; old record-only deletions leave unowned bundles which are retired.
pub fn recover(state: &AppState) -> ApiResult<()> {
    for row in state.store.list_snapshots()? {
        match row.state.as_str() {
            "creating" => {
                if state.backend.snapshot_manifest(&row.id).is_ok() {
                    state.store.set_snapshot_state(
                        &row.id,
                        "ready",
                        retained_bytes(state, &row.id)?,
                    )?;
                } else {
                    cleanup(state, &row.id)?;
                }
            }
            "deleting" => cleanup(state, &row.id)?,
            "ready" => {
                state
                    .store
                    .set_snapshot_state(&row.id, "ready", retained_bytes(state, &row.id)?)?
            }
            _ => {}
        }
    }
    for id in state.backend.snapshot_ids()? {
        match state.store.get_snapshot(&id) {
            Ok(_) => {}
            Err(ahvm_store::Error::NotFound(_)) => state.backend.delete_snapshot(&id)?,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub async fn get(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(snapshot_id): Path<String>,
) -> ApiResult<Json<SnapshotView>> {
    owned_snapshot(&state, &user.0, &snapshot_id).map(|row| Json(view(&row)))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(snapshot_id): Path<String>,
) -> ApiResult<StatusCode> {
    owned_snapshot(&state, &user.0, &snapshot_id)?;
    let snapshot_lock = state
        .lifecycle
        .lock(&format!("snapshot:{snapshot_id}"))
        .await;
    crate::routes::unchanged_identity(&snapshot_lock)?;
    owned_snapshot(&state, &user.0, &snapshot_id)?;
    let permit = state.ops.acquire().await;
    tokio::task::spawn_blocking(move || -> ApiResult<()> {
        let _permit = permit;
        cleanup(&state, &snapshot_id)?;
        snapshot_lock.invalidate_identity();
        Ok(())
    })
    .await
    .map_err(|e| ApiError::Internal(format!("snapshot cleanup task: {e}")))??;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct RestoreBody {
    pub new_id: String,
}

pub async fn restore(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(snapshot_id): Path<String>,
    Json(body): Json<RestoreBody>,
) -> ApiResult<impl IntoResponse> {
    if body.new_id.is_empty()
        || body.new_id.len() > 64
        || body.new_id.starts_with('.')
        || body.new_id == "snapshots"
        || !body
            .new_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(ApiError::Invalid("invalid sandbox name".into()));
    }
    owned_snapshot(&state, &user.0, &snapshot_id)?;
    crate::routes::reserved_owner(&state, &user.0, &body.new_id)?;
    let snapshot_lock = state
        .lifecycle
        .lock(&format!("snapshot:{snapshot_id}"))
        .await;
    crate::routes::unchanged_identity(&snapshot_lock)?;
    let snapshot = owned_snapshot(&state, &user.0, &snapshot_id)?;
    if snapshot.state != "ready" {
        return Err(ApiError::Conflict("snapshot is not ready".into()));
    }
    let lifecycle = state.lifecycle.lock(&body.new_id).await;
    crate::routes::unchanged_identity(&lifecycle)?;
    crate::routes::identity_available(&state, &body.new_id)?;
    state.store.check_replicated_name_available(&body.new_id)?;
    let permit = state.ops.acquire().await;
    let result = tokio::task::spawn_blocking(move || -> ApiResult<_> {
        let (_snapshot_lock, _lifecycle, _permit) = (snapshot_lock, lifecycle, permit);
        let manifest = state.backend.snapshot_manifest(&snapshot_id)?;
        let me = state.store.get_user(&user.0)?;
        let _hold = state.quotas.reserve_sandbox(
            &state.store,
            &me,
            &body.new_id,
            manifest.compat.vcpus as i64,
            manifest.compat.mem_mib as i64,
        )?;
        let info = state.backend.restore(&manifest, &body.new_id)?;
        let now = unix_now();
        let row = ahvm_store::Sandbox {
            id: info.id.clone(),
            owner_user_id: user.0,
            name: info.name.clone(),
            backend: ahvm_store::Backend::Krucible,
            state: crate::state_str(&info.state),
            thermal: crate::thermal_str(&info.thermal),
            cpus: manifest.compat.vcpus as i64,
            memory_mb: manifest.compat.mem_mib as i64,
            ip: info.ip.clone(),
            created_at: now,
            updated_at: now,
        };
        if let Err(error) = state.store.create_sandbox(&row) {
            let _ = state.backend.destroy(&info.id);
            return Err(error.into());
        }
        state.activity.touch(&info.id);
        Ok(crate::SandboxView::new(&row, &info))
    })
    .await
    .map_err(|e| ApiError::Internal(format!("snapshot restore task: {e}")))??;
    Ok((StatusCode::CREATED, Json(result)))
}

#[cfg(test)]
#[path = "snapshot_tests.rs"]
mod tests;
