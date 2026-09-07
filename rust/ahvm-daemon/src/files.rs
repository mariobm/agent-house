//! Guest files: read/write/list through the backend.

use crate::{auth::UserId, blocking, routes::owned, ApiError, ApiResult, AppState};
use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_limit")]
    pub limit: u64,
}

fn default_limit() -> u64 {
    65536
}

#[derive(Debug, Serialize)]
pub struct ReadResponse {
    pub data_b64: String,
    pub eof: bool,
}

pub async fn read(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<ReadResponse>> {
    owned(&state, &user.0, &id).await?;
    if q.path.is_empty() {
        return Err(ApiError::Invalid("path must not be empty".to_string()));
    }
    state.activity.touch(&id);
    let _flight = state
        .activity
        .begin(&id)
        .ok_or_else(|| crate::ApiError::Conflict(format!("sandbox {id} is stopping")))?;
    let backend = state.backend.clone();
    let chunk = blocking(move || backend.file_read(&id, &q.path, q.offset, q.limit)).await?;
    Ok(Json(ReadResponse {
        data_b64: base64_body(&chunk.data),
        eof: chunk.eof,
    }))
}

#[derive(Debug, Deserialize)]
pub struct WriteBody {
    pub path: String,
    pub data_b64: String,
}

#[derive(Debug, Serialize)]
pub struct WriteResponse {
    pub bytes: u64,
}

pub async fn write(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(body): Json<WriteBody>,
) -> ApiResult<Json<WriteResponse>> {
    owned(&state, &user.0, &id).await?;
    if body.path.is_empty() {
        return Err(ApiError::Invalid("path must not be empty".to_string()));
    }
    state.activity.touch(&id);
    let _flight = state
        .activity
        .begin(&id)
        .ok_or_else(|| crate::ApiError::Conflict(format!("sandbox {id} is stopping")))?;
    let data = base64_decode(&body.data_b64)?;
    let backend = state.backend.clone();
    let bytes = blocking(move || backend.file_write(&id, &body.path, &data)).await?;
    Ok(Json(WriteResponse { bytes }))
}

#[derive(Debug, Serialize)]
pub struct ListResponse {
    pub entries: Vec<EntryView>,
    pub next_offset: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct EntryView {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<ListResponse>> {
    owned(&state, &user.0, &id).await?;
    if q.path.is_empty() {
        return Err(ApiError::Invalid("path must not be empty".to_string()));
    }
    state.activity.touch(&id);
    let _flight = state
        .activity
        .begin(&id)
        .ok_or_else(|| crate::ApiError::Conflict(format!("sandbox {id} is stopping")))?;
    let backend = state.backend.clone();
    let listing = blocking(move || backend.file_list(&id, &q.path, q.offset, q.limit)).await?;
    Ok(Json(ListResponse {
        entries: listing
            .entries
            .into_iter()
            .map(|e| EntryView {
                name: e.name,
                is_dir: e.is_dir,
                size: e.size,
            })
            .collect(),
        next_offset: listing.next_offset,
    }))
}

fn base64_body(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn base64_decode(s: &str) -> ApiResult<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| ApiError::Invalid(format!("data_b64 is not base64: {e}")))
}
