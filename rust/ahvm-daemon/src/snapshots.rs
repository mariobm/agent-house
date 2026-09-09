//! Typed snapshots: create (online), get/delete (records), restore-as-new.
//!
//! Record deletes are record-only: backend registry bundles are immutable
//! and content-addressed by snapshot id, so dropping the row cannot strand
//! a live sandbox (nothing references bundles but restores).

use crate::{auth::UserId, blocking, routes::owned, unix_now, ApiResult, AppState};
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
    if body.name.is_empty() || body.name.len() > 64 {
        return Err(crate::ApiError::Invalid(
            "name must be 1..=64 chars".to_string(),
        ));
    }
    owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    let owned_id = id.clone();
    let snap_name = body.name.clone();
    // Quota first: same atomic gate as sandboxes (count + in-flight).
    let me = state.store.get_user(&user.0)?;
    let _hold = state
        .quotas
        .reserve_snapshot(&state.store, &me, &snap_name)?;
    let _permit = state.ops.acquire().await;
    state.activity.touch(&id);
    // Guard across the guest-paused snapshot (minutes on big RAM).
    let _flight = state
        .activity
        .begin(&id)
        .ok_or_else(|| crate::ApiError::Conflict(format!("sandbox {id} is stopping")))?;
    let manifest = blocking(move || backend.create_snapshot(&owned_id, &snap_name)).await?;
    let now = unix_now();
    let row = ahvm_store::Snapshot {
        id: manifest.snapshot_id.clone(),
        owner_user_id: user.0.clone(),
        sandbox_id: Some(id.clone()),
        name: body.name.clone(),
        kind: "manual".to_string(),
        state: "ready".to_string(),
        local_bytes: manifest.artifacts.memory_bytes as i64,
        remote_state: "local".to_string(),
        remote_manifest_key: None,
        created_at: now,
        expires_at: None,
    };
    state.store.create_snapshot(&row)?;
    Ok((StatusCode::CREATED, Json(view(&row))))
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
    match state.store.delete_snapshot(&snapshot_id) {
        Ok(()) | Err(ahvm_store::Error::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
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
    owned_snapshot(&state, &user.0, &snapshot_id)?;
    // Lifecycle first (see scheduler::LifecycleLocks): the restore (quota
    // → boot → record) is one critical section for the new id.
    crate::routes::reserved_owner(&state, &user.0, &body.new_id)?;
    let _lc = state.lifecycle.lock(&body.new_id).await;
    let backend = state.backend.clone();
    let manifest = blocking({
        let backend = backend.clone();
        let snapshot_id = snapshot_id.clone();
        move || backend.snapshot_manifest(&snapshot_id)
    })
    .await?;
    // Restore consumes sandbox quota like a create (it boots a new VM):
    // reserve with the stored manifest's sizing before touching the backend.
    let me = state.store.get_user(&user.0)?;
    let _hold = state.quotas.reserve_sandbox(
        &state.store,
        &me,
        &body.new_id,
        manifest.compat.vcpus as i64,
        manifest.compat.mem_mib as i64,
    )?;
    let _permit = state.ops.acquire().await;
    let new_id = body.new_id.clone();
    let snap_cpus = manifest.compat.vcpus as i64;
    let snap_mem = manifest.compat.mem_mib as i64;
    let info = blocking(move || backend.restore(&manifest, &new_id)).await?;
    let now = unix_now();
    // Sizing lives in the manifest (the compat gate already vetted it).
    let sandbox_row = ahvm_store::Sandbox {
        id: info.id.clone(),
        owner_user_id: user.0.clone(),
        name: info.name.clone(),
        backend: ahvm_store::Backend::Krucible,
        state: crate::state_str(&info.state),
        thermal: crate::thermal_str(&info.thermal),
        cpus: snap_cpus,
        memory_mb: snap_mem,
        ip: info.ip.clone(),
        created_at: now,
        updated_at: now,
    };
    if let Err(e) = state.store.create_sandbox(&sandbox_row) {
        let backend = state.backend.clone();
        let id = info.id.clone();
        let _ = blocking(move || backend.destroy(&id)).await;
        return Err(e.into());
    }
    state.activity.touch(&info.id);
    Ok((
        StatusCode::CREATED,
        Json(crate::SandboxView::new(&sandbox_row, &info)),
    ))
}
