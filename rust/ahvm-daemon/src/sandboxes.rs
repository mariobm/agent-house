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
    #[serde(default)]
    pub storage_mode: Option<ahvm_engine::StorageMode>,
    #[serde(default)]
    pub desktop: bool,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default = "default_cpus")]
    pub cpus: u8,
    #[serde(default = "default_mem")]
    pub memory_mb: u32,
}

fn default_cpus() -> u8 {
    1
}
fn default_mem() -> u32 {
    2048
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
    create_operation(State(state), Extension(user), Json(body), None, None, None).await
}

pub(crate) async fn create_operation(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Json(body): Json<CreateBody>,
    operation: Option<&str>,
    network_bytes_per_sec: Option<u64>,
    image_digest: Option<&str>,
) -> ApiResult<impl IntoResponse> {
    if body.name.is_empty() || body.name.len() > 64 {
        return Err(ApiError::Invalid("name must be 1..=64 chars".to_string()));
    }
    // Lifecycle serialization first (see scheduler::LifecycleLocks): the
    // whole create (quota → boot → record) is one critical section per id.
    let _lc = state.lifecycle.lock(&body.name).await;
    state.store.check_lifecycle_fence(&body.name, operation)?;
    // A retained disk owns the name even after its sandbox row is gone.
    crate::routes::reserved_owner(&state, &user.0, &body.name)?;
    state.store.check_replicated_name_available(&body.name)?;
    let desktop = body.desktop
        || matches!(
            body.image.as_deref(),
            Some("ubuntu-desktop" | "omarchy-desktop")
        );
    let gpu = body.image.as_deref() == Some("omarchy-desktop");
    if desktop
        && body
            .image
            .as_deref()
            .is_some_and(|name| !matches!(name, "ubuntu-desktop" | "omarchy-desktop"))
    {
        return Err(ApiError::Invalid(
            "desktop requires ubuntu-desktop or omarchy-desktop".into(),
        ));
    }
    let root_image = if let Some(expected) = image_digest {
        let path = resolve_image(Some("ubuntu-dev"))?
            .ok_or_else(|| ApiError::Invalid("Ubuntu image unavailable".into()))?;
        if std::path::Path::new(&path)
            .file_stem()
            .and_then(|s| s.to_str())
            != Some(expected)
        {
            return Err(ApiError::Conflict("Ubuntu image generation changed".into()));
        }
        // Retain the immutable digest path, not the mutable alias/default symlink.
        Some(path)
    } else if desktop {
        // Explicit named images always win over legacy preview overrides.
        match body.image.as_deref() {
            Some(name) => resolve_image(Some(name))?,
            None => match std::env::var("AHVM_DESKTOP_IMAGE") {
                Ok(path) => Some(path),
                Err(_) => resolve_image(Some("ubuntu-desktop"))?,
            },
        }
    } else {
        resolve_image(body.image.as_deref())?
    };
    let spec = ahvm_engine::SandboxSpec {
        storage_mode: body.storage_mode,
        name: body.name.clone(),
        cpus: body.cpus,
        memory_mb: body.memory_mb,
        backend: ahvm_engine::BackendKind::Krucible,
        root_image,
        kernel_image: None,
        desktop,
        desktop_gpu: gpu
            || (desktop
                && body.image.is_none()
                && std::env::var("AHVM_DESKTOP_GPU").as_deref() == Ok("1")),
        network_bytes_per_sec,
        extra_env: Default::default(),
    };
    let permit = state.ops.acquire().await;
    // Keep lifecycle admission and the store commit in the blocking task.
    // Disconnecting the HTTP client must not expose an in-flight reservation
    // to orphan recovery while import is still running.
    let (row, info) = tokio::task::spawn_blocking(move || {
        let _lifecycle = _lc;
        let _permit = permit;
        crate::replicated::create(&state, &user.0, &spec)
    })
    .await
    .map_err(|e| ApiError::Internal(format!("create task: {e}")))??;
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
            storage: None,
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
    let _lc = state.lifecycle.lock(&id).await;
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
    destroy_operation(State(state), Extension(user), Path(id), None).await
}

pub(crate) async fn destroy_operation(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    operation: Option<&str>,
) -> ApiResult<StatusCode> {
    let _lc = state.lifecycle.lock(&id).await;
    state.store.check_lifecycle_fence(&id, operation)?;
    owned(&state, &user.0, &id).await?;
    if let Some(reservation) = state.store.replicated_for_sandbox(&user.0, &id)? {
        state
            .store
            .delete_replicated_reservation(&user.0, &reservation.volume_id, unix_now())?;
    }
    let _permit = state.ops.acquire().await;
    let backend = state.backend.clone();
    let owned_id = id.clone();
    match blocking(move || backend.destroy(&owned_id)).await {
        // Backend already lost it (data dir wiped): converge by dropping
        // the record rather than stranding it.
        Ok(()) | Err(ApiError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    state.activity.remove(&id);
    match state.store.delete_sandbox(&id) {
        Ok(()) | Err(ahvm_store::Error::NotFound(_)) => {}
        Err(e) => return Err(e.into()),
    }
    state.ops.transfers.forget(&id);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn start(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<SandboxView>> {
    start_operation(State(state), Extension(user), Path(id), None, None).await
}

pub(crate) async fn start_operation(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    operation: Option<&str>,
    network_bytes_per_sec: Option<u64>,
) -> ApiResult<Json<SandboxView>> {
    owned(&state, &user.0, &id).await?;
    let me = state.store.get_user(&user.0)?;
    // Keep admission through the backend transition and its store mirror.
    // Stopped/failed boxes regain resource usage; running boxes are counted once.
    let _hold = state.quotas.reserve_start(&state.store, &me, &id)?;
    set_running(&state, &id, operation, move |backend, owned_id| {
        backend.start_with_network_bandwidth(&owned_id, network_bytes_per_sec)
    })
    .await
}

pub async fn stop(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<SandboxView>> {
    stop_operation(State(state), Extension(user), Path(id), None).await
}

pub(crate) async fn stop_operation(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    operation: Option<&str>,
) -> ApiResult<Json<SandboxView>> {
    owned(&state, &user.0, &id).await?;
    set_running(&state, &id, operation, |backend, owned_id| {
        backend.stop(&owned_id)
    })
    .await
}

async fn set_running(
    state: &AppState,
    id: &str,
    operation: Option<&str>,
    op: impl FnOnce(std::sync::Arc<dyn ahvm_engine::Backend>, String) -> Result<(), ahvm_engine::Error>
        + Send
        + 'static,
) -> ApiResult<Json<SandboxView>> {
    // Lifecycle first (see scheduler::LifecycleLocks), then the op permit:
    // same order as the sweep, so neither can deadlock the other.
    let _lc = state.lifecycle.lock(id).await;
    state.store.check_lifecycle_fence(id, operation)?;
    let _permit = state.ops.acquire().await;
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
    if live.state == ahvm_engine::State::Running {
        // A (re)started VM is active now; without this a start followed by
        // silence would be reaped on its stale pre-stop timestamp.
        state.activity.touch(&live.id);
    }
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
    // Keep the activity guard inside the backend task, even if the HTTP
    // caller disconnects before the guest command finishes.
    state.activity.touch(&id);
    let _flight = crate::routes::guest(&state, &id).await?;
    let backend = state.backend.clone();
    let out = blocking(move || {
        let _flight = _flight;
        backend.exec(&id, &body.argv)
    })
    .await?;
    Ok(Json(out))
}

// Image aliases resolve only inside the administrator-managed cache. API users
// cannot supply arbitrary host filesystem paths.
pub(crate) fn resolve_image(name: Option<&str>) -> ApiResult<Option<String>> {
    let Some(name) = name else {
        return Ok(None);
    };
    if name.is_empty()
        || name.len() >= 100
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(ApiError::Invalid("invalid image name".into()));
    }
    let root = std::env::var_os("AHVM_IMAGE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/var/lib/ahvm-images".into());
    let bytes = std::fs::read(root.join(format!("{name}.json")))
        .map_err(|_| ApiError::Invalid("image is not installed; use ahvm image pull".into()))?;
    let record: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::Invalid("invalid image record".into()))?;
    let digest = record["sha256"].as_str().unwrap_or_default();
    if digest.len() != 64
        || !digest.bytes().all(|b| b.is_ascii_hexdigit())
        || record["guest_abi"].as_u64() != Some(1)
    {
        return Err(ApiError::Invalid("incompatible image record".into()));
    }
    let image = root.join(format!("{digest}.ext4"));
    if !image.is_file() {
        return Err(ApiError::Invalid("image file is missing".into()));
    }
    Ok(Some(image.to_string_lossy().into_owned()))
}
