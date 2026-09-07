//! Guest sessions: REST lifecycle + a WS attach stream.
//!
//! The WS stream bridges one guest session: server→client carries drained
//! output as JSON text frames, client→server text frames carry
//! `{"data_b64": ...}` input. Either side closing ends the session attach
//! (the guest session itself survives; attach is just a view).

use crate::{auth::UserId, blocking, routes::owned, ApiError, ApiResult, AppState};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct CreateBody {
    pub argv: Vec<String>,
    #[serde(default)]
    pub pty: bool,
}

#[derive(Debug, Serialize)]
pub struct CreateResponse {
    pub session_id: String,
}

pub async fn create(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    Json(body): Json<CreateBody>,
) -> ApiResult<Json<CreateResponse>> {
    if body.argv.is_empty() {
        return Err(ApiError::Invalid("argv must not be empty".to_string()));
    }
    owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    let session_id = blocking(move || backend.session_create(&id, &body.argv, body.pty)).await?;
    Ok(Json(CreateResponse { session_id }))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ahvm_engine::SessionInfo>>> {
    owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    Ok(Json(blocking(move || backend.session_list(&id)).await?))
}

#[derive(Debug, Deserialize)]
pub struct InputBody {
    pub data_b64: String,
}

#[derive(Debug, Serialize)]
pub struct InputResponse {
    pub bytes: u64,
}

pub async fn input(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, sid)): Path<(String, String)>,
    Json(body): Json<InputBody>,
) -> ApiResult<Json<InputResponse>> {
    use base64::Engine;
    owned(&state, &user.0, &id).await?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(&body.data_b64)
        .map_err(|e| ApiError::Invalid(format!("data_b64 is not base64: {e}")))?;
    let backend = state.backend.clone();
    let bytes = blocking(move || backend.session_input(&id, &sid, &data)).await?;
    Ok(Json(InputResponse { bytes }))
}

pub async fn kill(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, sid)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    let sid_reply = sid.clone();
    blocking(move || backend.session_kill(&id, &sid)).await?;
    Ok(Json(serde_json::json!({ "killed": sid_reply })))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, sid)): Path<(String, String)>,
) -> ApiResult<axum::http::StatusCode> {
    owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    blocking(move || backend.session_delete(&id, &sid)).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct ResizeBody {
    pub rows: u16,
    pub cols: u16,
}

pub async fn resize(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, sid)): Path<(String, String)>,
    Json(body): Json<ResizeBody>,
) -> ApiResult<Json<serde_json::Value>> {
    owned(&state, &user.0, &id).await?;
    let backend = state.backend.clone();
    let sid_reply = sid.clone();
    blocking(move || backend.session_resize(&id, &sid, body.rows, body.cols)).await?;
    Ok(Json(serde_json::json!({ "resized": sid_reply })))
}

#[derive(Debug, Deserialize)]
pub struct ReadQuery {
    #[serde(default)]
    pub from_seq: u64,
    #[serde(default = "default_budget_ms")]
    pub budget_ms: u64,
}

fn default_budget_ms() -> u64 {
    5000
}

#[derive(Debug, Serialize)]
pub struct ReadResponse {
    pub data_b64: String,
    pub eof: bool,
    pub exit_code: Option<i32>,
}

pub async fn read(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, sid)): Path<(String, String)>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<ReadResponse>> {
    use base64::Engine;
    owned(&state, &user.0, &id).await?;
    let budget = Duration::from_millis(q.budget_ms.clamp(100, 30_000));
    let backend = state.backend.clone();
    let chunk = blocking(move || backend.session_read(&id, &sid, q.from_seq, budget)).await?;
    Ok(Json(ReadResponse {
        data_b64: base64::engine::general_purpose::STANDARD.encode(&chunk.data),
        eof: chunk.eof,
        exit_code: chunk.exit_code,
    }))
}

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    pub from_seq: u64,
}

pub async fn stream(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, sid)): Path<(String, String)>,
    Query(q): Query<StreamQuery>,
    ws: WebSocketUpgrade,
) -> ApiResult<axum::response::Response> {
    owned(&state, &user.0, &id).await?;
    // The session must exist before we upgrade (else the socket dangles).
    {
        let backend = state.backend.clone();
        let (id, sid) = (id.clone(), sid.clone());
        let list = blocking(move || backend.session_list(&id)).await?;
        if !list.iter().any(|s| s.id == sid) {
            return Err(ApiError::NotFound(format!("session {sid}")));
        }
    }
    Ok(ws.on_upgrade(move |socket| bridge(state, id, sid, q.from_seq, socket)))
}

async fn bridge(state: AppState, id: String, sid: String, mut seq: u64, socket: WebSocket) {
    use futures_util::{SinkExt, StreamExt};
    let (mut tx, mut rx) = socket.split();
    // Upper bound per drain; idle guests just long-poll at this cadence.
    let budget = Duration::from_millis(1000);
    'outer: loop {
        // Pump pending input without blocking the output drain.
        while let Ok(incoming) = tokio::time::timeout(Duration::from_millis(0), rx.next()).await {
            let data: Option<Vec<u8>> = match incoming {
                Some(Ok(Message::Text(t))) => serde_json::from_str::<serde_json::Value>(&t)
                    .ok()
                    .and_then(|v| v["data_b64"].as_str().map(str::to_string))
                    .and_then(|b| {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.decode(&b).ok()
                    }),
                Some(Ok(Message::Binary(b))) => Some(b.to_vec()),
                // Client close, error, or end of stream: detach (the guest
                // session itself survives; attach is just a view).
                _ => break 'outer,
            };
            if let Some(data) = data {
                let backend = state.backend.clone();
                let (id, sid) = (id.clone(), sid.clone());
                let _ =
                    tokio::task::spawn_blocking(move || backend.session_input(&id, &sid, &data))
                        .await;
            }
        }
        // Blocking output drain (up to the budget), then forward.
        let backend = state.backend.clone();
        let (id, sid) = (id.clone(), sid.clone());
        let chunk =
            tokio::task::spawn_blocking(move || backend.session_read(&id, &sid, seq, budget)).await;
        let chunk = match chunk {
            Ok(Ok(c)) => c,
            _ => break,
        };
        seq += chunk.data.len() as u64;
        if !chunk.data.is_empty() || chunk.eof {
            use base64::Engine;
            let frame = serde_json::json!({
                "data_b64": base64::engine::general_purpose::STANDARD.encode(&chunk.data),
                "eof": chunk.eof,
                "exit_code": chunk.exit_code,
            });
            if tx
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        if chunk.eof {
            break;
        }
    }
}
