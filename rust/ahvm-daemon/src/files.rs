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

// EOF is explicit: losing the HTTP task/channel is an abort, not a valid end.
enum UploadPart {
    Data(Vec<u8>),
    Eof,
}
struct UploadReader {
    rx: tokio::sync::mpsc::Receiver<UploadPart>,
    pending: std::io::Cursor<Vec<u8>>,
    eof: bool,
}
impl std::io::Read for UploadReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.pending.read(out)?;
            if n > 0 || self.eof {
                return Ok(n);
            }
            match self.rx.blocking_recv() {
                Some(UploadPart::Data(bytes)) => self.pending = std::io::Cursor::new(bytes),
                Some(UploadPart::Eof) => self.eof = true,
                None => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "HTTP upload interrupted",
                    ))
                }
            }
        }
    }
}

/// Raw streaming endpoint, separate from the legacy small JSON write route.
pub async fn upload(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Query(q): Query<ReadQuery>,
    body: axum::body::Body,
) -> ApiResult<Json<WriteResponse>> {
    use futures_util::StreamExt;
    use std::time::Duration;
    owned(&state, &user.0, &id).await?;
    if q.path.is_empty() {
        return Err(ApiError::Invalid("path must not be empty".into()));
    }
    let stream_guard = state
        .ops
        .try_stream(&id)
        .ok_or_else(|| ApiError::Conflict("workspace stream limit reached; retry later".into()))?;
    // Fail fast rather than retaining unbounded waiting HTTP uploads.
    let permit = state
        .ops
        .try_acquire()
        .ok_or_else(|| ApiError::Conflict("upload capacity busy; retry later".into()))?;
    let flight = state
        .activity
        .begin(&id)
        .ok_or_else(|| ApiError::Conflict(format!("sandbox {id} is stopping")))?;
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    let backend = state.backend.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        // A cancelled HTTP handler must not release admission/activity while
        // the blocking backend is still unwinding its guest transaction.
        let (_permit, _flight, _stream) = (permit, flight, stream_guard);
        let mut reader = UploadReader {
            rx,
            pending: std::io::Cursor::new(Vec::new()),
            eof: false,
        };
        backend.file_upload(&id, &q.path, &mut reader)
    });
    let feeding = async move {
        let mut stream = body.into_data_stream();
        while let Some(part) = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .map_err(|_| ApiError::Invalid("upload idle timeout".into()))?
        {
            let bytes = part.map_err(|e| ApiError::Invalid(format!("upload body: {e}")))?;
            for chunk in bytes.chunks(65536) {
                tokio::time::timeout(
                    Duration::from_secs(30),
                    tx.send(UploadPart::Data(chunk.to_vec())),
                )
                .await
                .map_err(|_| ApiError::Invalid("upload stalled".into()))?
                .map_err(|_| ApiError::Invalid("guest upload closed".into()))?;
            }
        }
        tx.send(UploadPart::Eof)
            .await
            .map_err(|_| ApiError::Invalid("guest upload closed".into()))?;
        Ok::<_, ApiError>(())
    };
    tokio::pin!(feeding);
    let result = tokio::select! {
        result=&mut worker=>result,
        result=&mut feeding=>{result?;worker.await},
    }
    .map_err(|e| ApiError::Invalid(format!("upload worker: {e}")))??;
    Ok(Json(WriteResponse { bytes: result }))
}
