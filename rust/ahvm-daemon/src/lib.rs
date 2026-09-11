//! ahvm-daemon: control plane (REST/WS API over axum).
//!
//! Dashboard-first HTTP API over the [`Backend`](ahvm_engine::Backend)
//! seam with [`Store`](ahvm_store::Store) persistence:
//!
//! * `POST /v1/sandboxes` (+ list/get/delete/start/stop) — sandbox records
//!   mirror backend lifecycle; the backend owns boot truth, the store owns
//!   ownership and listing.
//! * `POST /v1/sandboxes/{id}/exec`, files under `/files` + `/dir`,
//!   snapshots under `/snapshots`, sessions under `/sessions` (+ a WS
//!   attach stream).
//!
//! Auth is Bearer tokens (`Authorization: Bearer <token>`, SHA-256 hex
//! looked up like Go's `sha256Hex`). Blocking backend calls run on
//! `spawn_blocking`; store calls are fast-local and run inline.

pub mod auth;
pub mod desktop;
pub mod files;
pub mod previews;
pub mod quotas;
pub mod routes;
pub mod sandboxes;
pub mod scheduler;
pub mod sessions;
pub mod snapshots;
pub mod thermal;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ahvm_engine::{Backend, SandboxInfo};
use ahvm_store::Store;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use serde::Serialize;

/// Shared handler state (all halves `Send + Sync`, cheap to clone).
#[derive(Debug, Clone)]
pub struct AppState {
    /// Host-reserved sandbox ids: only this user may create/restore that id.
    pub private_owners: Arc<std::collections::BTreeMap<String, String>>,
    pub store: Arc<Store>,
    pub backend: Arc<dyn Backend>,
    pub quotas: quotas::Registry,
    pub activity: thermal::ActivityTracker,
    pub ops: scheduler::OpsLimiter,
    pub lifecycle: scheduler::LifecycleLocks,
}

/// JSON error body: machine-readable `code`, human `message`.
#[derive(Debug, Serialize)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
}

/// Handler error: HTTP status + JSON body. Engine/store errors map here
/// centrally so routes stay thin.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("backend unavailable: {0}")]
    Unavailable(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Unavailable(_) => StatusCode::BAD_GATEWAY,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            ApiError::Unauthorized => "unauthorized",
            ApiError::Forbidden(_) => "forbidden",
            ApiError::NotFound(_) => "not_found",
            ApiError::Conflict(_) => "conflict",
            ApiError::Invalid(_) => "invalid",
            ApiError::Unavailable(_) => "backend_unavailable",
            ApiError::Internal(_) => "internal",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = Json(ApiErrorBody {
            code: self.code().to_string(),
            message: self.to_string(),
        });
        (status, body).into_response()
    }
}

impl From<ahvm_engine::Error> for ApiError {
    fn from(e: ahvm_engine::Error) -> Self {
        match e {
            ahvm_engine::Error::NotFound(m) => ApiError::NotFound(m),
            ahvm_engine::Error::Conflict(m) => ApiError::Conflict(m),
            ahvm_engine::Error::InvalidState(m) | ahvm_engine::Error::Incompatible(m) => {
                ApiError::Invalid(m)
            }
            ahvm_engine::Error::Control(m) => ApiError::Unavailable(m),
            ahvm_engine::Error::Io(e) => ApiError::Internal(format!("io: {e}")),
            ahvm_engine::Error::Json(e) => ApiError::Internal(format!("json: {e}")),
        }
    }
}

impl From<ahvm_store::Error> for ApiError {
    fn from(e: ahvm_store::Error) -> Self {
        match e {
            ahvm_store::Error::NotFound(m) => ApiError::NotFound(m),
            ahvm_store::Error::Conflict(m) => ApiError::Conflict(m),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// API sandbox view: store identity + live backend state.
#[derive(Debug, Serialize)]
pub struct SandboxView {
    pub id: String,
    pub name: String,
    pub state: String,
    pub thermal: String,
    pub cpus: i64,
    pub memory_mb: i64,
    pub ip: String,
}

impl SandboxView {
    pub fn new(row: &ahvm_store::Sandbox, live: &SandboxInfo) -> Self {
        Self {
            id: row.id.clone(),
            name: row.name.clone(),
            state: state_str(&live.state),
            thermal: thermal_str(&live.thermal),
            cpus: row.cpus,
            memory_mb: row.memory_mb,
            ip: live.ip.clone(),
        }
    }
}

pub fn state_str(s: &ahvm_engine::State) -> String {
    format!("{s:?}").to_lowercase()
}

pub fn thermal_str(t: &ahvm_engine::Thermal) -> String {
    format!("{t:?}").to_lowercase()
}

/// Blocking backend calls must not run on async workers: run `f` on the
/// blocking pool and map panics/cancellation to 500.
pub async fn blocking<F, T>(f: F) -> ApiResult<T>
where
    F: FnOnce() -> std::result::Result<T, ahvm_engine::Error> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::Internal(format!("backend task: {e}")))?
        .map_err(ApiError::from)
}

pub fn build_router(state: AppState) -> Router {
    use crate::{files, sandboxes, sessions, snapshots};
    use axum::middleware;

    let authed = Router::new()
        .route("/v1/sandboxes/{id}/previews", get(previews::list))
        .route(
            "/v1/sandboxes/{id}/previews/{port}/access",
            axum::routing::post(previews::access),
        )
        .route(
            "/v1/sandboxes/{id}/previews/{port}",
            axum::routing::put(previews::enable).delete(previews::disable),
        )
        .route(
            "/v1/sandboxes",
            get(sandboxes::list).post(sandboxes::create),
        )
        .route(
            "/v1/sandboxes/{id}",
            get(sandboxes::get).delete(sandboxes::destroy),
        )
        .route(
            "/v1/sandboxes/{id}/start",
            axum::routing::post(sandboxes::start),
        )
        .route(
            "/v1/sandboxes/{id}/stop",
            axum::routing::post(sandboxes::stop),
        )
        .route("/v1/sandboxes/{id}/desktop/stream", get(desktop::stream))
        .route(
            "/v1/sandboxes/{id}/exec",
            axum::routing::post(sandboxes::exec),
        )
        .route(
            "/v1/sandboxes/{id}/files",
            get(files::read).put(files::write),
        )
        .route(
            "/v1/sandboxes/{id}/files/upload",
            axum::routing::put(files::upload),
        )
        .route("/v1/sandboxes/{id}/dir", get(files::list))
        .route(
            "/v1/sandboxes/{id}/snapshots",
            axum::routing::post(snapshots::create),
        )
        .route(
            "/v1/snapshots/{snapshot_id}",
            get(snapshots::get).delete(snapshots::delete),
        )
        .route(
            "/v1/snapshots/{snapshot_id}/restore",
            axum::routing::post(snapshots::restore),
        )
        .route(
            "/v1/sandboxes/{id}/sessions",
            get(sessions::list).post(sessions::create),
        )
        .route(
            "/v1/sandboxes/{id}/sessions/{sid}/input",
            axum::routing::post(sessions::input),
        )
        .route(
            "/v1/sandboxes/{id}/sessions/{sid}/kill",
            axum::routing::post(sessions::kill),
        )
        .route(
            "/v1/sandboxes/{id}/sessions/{sid}",
            axum::routing::delete(sessions::delete),
        )
        .route(
            "/v1/sandboxes/{id}/sessions/{sid}/read",
            get(sessions::read),
        )
        .route(
            "/v1/sandboxes/{id}/sessions/{sid}/resize",
            axum::routing::post(sessions::resize),
        )
        .route(
            "/v1/sandboxes/{id}/sessions/{sid}/stream",
            get(sessions::stream),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_user,
        ));
    Router::new()
        .route("/v1/healthz", get(healthz))
        .merge(authed)
        .with_state(state)
}

async fn healthz() -> Json<serde_json::Value> {
    let mut features = vec!["named-images-v1"];
    if std::env::var_os("AHVM_DESKTOP_IMAGE").is_some() {
        features.push("desktop-v1");
    }
    Json(
        serde_json::json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION"), "features": features }),
    )
}
