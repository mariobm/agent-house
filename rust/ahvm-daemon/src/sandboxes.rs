//! Sandbox CRUD + lifecycle + exec.

use crate::{
    auth::UserId, blocking, routes::owned, state_str, thermal_str, unix_now, ApiError, ApiResult,
    AppState, SandboxView,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub name: String,
    #[serde(default = "default_cpus")]
    pub cpus: u8,
    #[serde(default = "default_mem")]
    pub memory_mb: u32,
}

fn default_cpus() -> u8 {
    1
}
fn default_mem() -> u32 {
    512
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub sandboxes: Vec<SandboxView>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub after: Option<String>,
    pub limit: Option<usize>,
}

fn parse_cursor(after: &Option<String>) -> ApiResult<Option<(i64, String)>> {
    let Some(cursor) = after else {
        return Ok(None);
    };
    let (ts, id) = cursor
        .split_once(':')
        .ok_or_else(|| ApiError::Invalid("bad cursor (want <created_at>:<id>)".to_string()))?;
    let ts: i64 = ts
        .parse()
        .map_err(|_| ApiError::Invalid("bad cursor timestamp".to_string()))?;
    Ok(Some((ts, id.to_string())))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Json(body): Json<CreateBody>,
) -> ApiResult<impl IntoResponse> {
    if body.name.is_empty() || body.name.len() > 64 {
        return Err(ApiError::Invalid("name must be 1..=64 chars".to_string()));
    }
    // Atomic quota gate: committed rows plus in-flight holds, checked
    // and reserved under one lock (see quotas.rs). The hold lives until
    // the store record commits below, so concurrent creators serialize.
    let me = state.store.get_user(&user.0)?;
    let _hold = state.quotas.reserve_sandbox(
        &state.store,
        &me,
        &body.name,
        body.cpus as i64,
        body.memory_mb as i64,
    )?;
    let spec = ahvm_engine::SandboxSpec {
        name: body.name.clone(),
        cpus: body.cpus,
        memory_mb: body.memory_mb,
        backend: ahvm_engine::BackendKind::Krucible,
        root_image: None,
        kernel_image: None,
        extra_env: Default::default(),
    };
    let backend = state.backend.clone();
    let info = blocking(move || backend.create(&spec)).await?;
    // Mirror to the store; on failure unwind the boot (never orphan a VM
    // behind a missing record).
    let now = unix_now();
    let row = ahvm_store::Sandbox {
        id: info.id.clone(),
        owner_user_id: user.0.clone(),
        name: info.name.clone(),
        backend: ahvm_store::Backend::Krucible,
        state: state_str(&info.state),
        thermal: thermal_str(&info.thermal),
        cpus: body.cpus as i64,
        memory_mb: body.memory_mb as i64,
        ip: info.ip.clone(),
        created_at: now,
        updated_at: now,
    };
    if let Err(e) = state.store.create_sandbox(&row) {
        let backend = state.backend.clone();
        let id = info.id.clone();
        let _ = blocking(move || backend.destroy(&id)).await;
        return Err(e.into());
    }
    Ok((StatusCode::CREATED, Json(SandboxView::new(&row, &info))))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<ListResponse>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    // One extra row tells us whether a next page exists.
    let rows = state
        .store
        .list_sandboxes(&user.0, parse_cursor(&q.after)?, limit + 1)?;
    let next_cursor = (rows.len() > limit).then(|| {
        let last = &rows[limit - 1];
        format!("{}:{}", last.created_at, last.id)
    });
    let views = rows
        .into_iter()
        .take(limit)
        .map(|row| SandboxView {
            id: row.id.clone(),
            name: row.name.clone(),
            state: row.state.clone(),
            thermal: row.thermal.clone(),
            cpus: row.cpus,
            memory_mb: row.memory_mb,
            ip: row.ip.clone(),
        })
        .collect();
    Ok(Json(ListResponse {
        sandboxes: views,
        next_cursor,
    }))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<SandboxView>> {
    let row = owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    let owned_id = id.clone();
    let live = blocking(move || backend.status(&owned_id)).await?;
    state.store.set_sandbox_state(
        &id,
        &state_str(&live.state),
        &thermal_str(&live.thermal),
        unix_now(),
    )?;
    Ok(Json(SandboxView::new(&row, &live)))
}

pub async fn destroy(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    let owned_id = id.clone();
    match blocking(move || backend.destroy(&owned_id)).await {
        // Backend already lost it (data dir wiped): converge by dropping
        // the record rather than stranding it.
        Ok(()) | Err(ApiError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    match state.store.delete_sandbox(&id) {
        Ok(()) | Err(ahvm_store::Error::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn start(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<SandboxView>> {
    owned(&state, &user.0, &id).await?;
    set_running(&state, &id, |backend, owned_id| backend.start(&owned_id)).await
}

pub async fn stop(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<SandboxView>> {
    owned(&state, &user.0, &id).await?;
    set_running(&state, &id, |backend, owned_id| backend.stop(&owned_id)).await
}

async fn set_running(
    state: &AppState,
    id: &str,
    op: impl FnOnce(std::sync::Arc<dyn ahvm_engine::Backend>, String) -> Result<(), ahvm_engine::Error>
        + Send
        + 'static,
) -> ApiResult<Json<SandboxView>> {
    let backend = state.backend.clone();
    let a = id.to_string();
    blocking(move || op(backend, a)).await?;
    let backend = state.backend.clone();
    let b = id.to_string();
    let live = blocking(move || backend.status(&b)).await?;
    state.store.set_sandbox_state(
        &live.id,
        &state_str(&live.state),
        &thermal_str(&live.thermal),
        unix_now(),
    )?;
    // Re-fetch the row for stable identity fields.
    let row = state.store.get_sandbox(&live.id)?;
    Ok(Json(SandboxView::new(&row, &live)))
}

#[derive(Debug, Deserialize)]
pub struct ExecBody {
    pub argv: Vec<String>,
}

pub async fn exec(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(body): Json<ExecBody>,
) -> ApiResult<Json<ahvm_engine::ExecResult>> {
    if body.argv.is_empty() {
        return Err(ApiError::Invalid("argv must not be empty".to_string()));
    }
    owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    let out = blocking(move || backend.exec(&id, &body.argv)).await?;
    Ok(Json(out))
}
