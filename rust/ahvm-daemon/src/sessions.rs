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
    state.activity.touch(&id);
    let _flight = state
        .activity
        .begin(&id)
        .ok_or_else(|| crate::ApiError::Conflict(format!("sandbox {id} is stopping")))?;
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
    state.activity.touch(&id);
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
    state.activity.touch(&id);
    let _flight = state
        .activity
        .begin(&id)
        .ok_or_else(|| crate::ApiError::Conflict(format!("sandbox {id} is stopping")))?;
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
    state.activity.touch(&id);
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
    state.activity.touch(&id);
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
    state.activity.touch(&id);
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
    /// Authoritative resume cursor for the next read (frame seq + bytes,
    /// never client-side byte counting).
    pub next_seq: u64,
    /// True when scrollback evicted output before the returned data.
    pub truncated: bool,
}

pub async fn read(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, sid)): Path<(String, String)>,
    Query(q): Query<ReadQuery>,
) -> ApiResult<Json<ReadResponse>> {
    use base64::Engine;
    owned(&state, &user.0, &id).await?;
    state.activity.touch(&id);
    // Guard across the drain: budgets reach 30s, past any small idle window.
    let _flight = state
        .activity
        .begin(&id)
        .ok_or_else(|| crate::ApiError::Conflict(format!("sandbox {id} is stopping")))?;
    let budget = Duration::from_millis(q.budget_ms.clamp(100, 30_000));
    let backend = state.backend.clone();
    let stream_guard = state
        .ops
        .try_stream(&id)
        .ok_or_else(|| ApiError::Conflict("workspace stream limit reached; retry later".into()))?;
    let chunk = blocking(move || {
        let (_stream, _flight) = (stream_guard, _flight);
        backend.session_read(&id, &sid, q.from_seq, budget)
    })
    .await?;
    Ok(Json(ReadResponse {
        data_b64: base64::engine::general_purpose::STANDARD.encode(&chunk.data),
        eof: chunk.eof,
        exit_code: chunk.exit_code,
        next_seq: chunk.next_seq,
        truncated: chunk.truncated,
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
    state.activity.touch(&id);
    let stream_guard = state
        .ops
        .try_stream(&id)
        .ok_or_else(|| ApiError::Conflict("workspace stream limit reached; retry later".into()))?;
    // The session must exist before we upgrade (else the socket dangles).
    {
        let backend = state.backend.clone();
        let (id, sid) = (id.clone(), sid.clone());
        let pending_guard = stream_guard.clone();
        let list = blocking(move || {
            let _stream = pending_guard;
            backend.session_list(&id)
        })
        .await?;
        if !list.iter().any(|s| s.id == sid) {
            return Err(ApiError::NotFound(format!("session {sid}")));
        }
    }
    Ok(ws
        .max_message_size(256 * 1024)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| async move {
            let _ = tokio::time::timeout(
                Duration::from_secs(3600),
                bridge(state, id, sid, q.from_seq, socket, stream_guard),
            )
            .await;
        }))
}

async fn bridge(
    state: AppState,
    id: String,
    sid: String,
    mut seq: u64,
    socket: WebSocket,
    stream_guard: std::sync::Arc<crate::scheduler::StreamGuard>,
) {
    use futures_util::StreamExt;
    let (mut tx, mut rx) = socket.split();
    let transfer = state.ops.transfers.get(&id);
    // Keep exactly one output read in flight while admitting keyboard input.
    // Dropping a blocking-task handle cannot cancel the guest RPC, so never
    // restart it merely because input arrived.
    let budget = Duration::from_millis(1000);
    'outer: loop {
        let backend = state.backend.clone();
        let (read_id, read_sid) = (id.clone(), sid.clone());
        let read_guard = stream_guard.clone();
        let mut output = tokio::task::spawn_blocking(move || {
            let _stream = read_guard;
            backend.session_poll(&read_id, &read_sid, seq, budget)
        });
        let chunk = loop {
            tokio::select! {
                chunk = &mut output => break chunk,
                incoming = rx.next() => {
                    if let (Some(transfer), Some(Ok(message))) = (&transfer, &incoming) {
                        let len = match message {
                            Message::Text(t) => t.len(),
                            Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b.len(),
                            Message::Close(_) => 0,
                        };
                        transfer.input.take(len).await;
                    }
                    let data: Option<Vec<u8>> = match incoming {
                        Some(Ok(Message::Text(t))) => serde_json::from_str::<serde_json::Value>(&t)
                            .ok()
                            .and_then(|v| v["data_b64"].as_str().map(str::to_string))
                            .and_then(|b| {
                                use base64::Engine;
                                base64::engine::general_purpose::STANDARD.decode(&b).ok()
                            }),
                        Some(Ok(Message::Binary(b))) => Some(b.to_vec()),
                        Some(Ok(Message::Ping(p))) => {
                            if !send_frame(&mut tx, Message::Pong(p)).await { break 'outer; }
                            None
                        }
                        Some(Ok(Message::Pong(_))) => None,
                        _ => break 'outer,
                    };
                    if let Some(data) = data {
                        let Some(guard) = state.activity.begin(&id) else {
                            let frame = serde_json::json!({"error": "sandbox is stopping; input rejected"});
                            let frame = frame.to_string();
                            if let Some(transfer) = &transfer { transfer.output.take(frame.len()).await; }
                            if !send_frame(&mut tx, Message::Text(frame.into())).await { break 'outer; }
                            continue;
                        };
                        let backend = state.backend.clone();
                        let (id, sid) = (id.clone(), sid.clone());
                        let input_guard = stream_guard.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            let _stream = input_guard;
                            // Admission lasts until the RPC actually completes,
                            // even if this WebSocket task is cancelled.
                            let _guard = guard;
                            backend.session_input(&id, &sid, &data)
                        }).await;
                        if !matches!(result, Ok(Ok(_))) {
                            let frame = serde_json::json!({"error": "session input failed"});
                            let _ = send_frame(&mut tx, Message::Text(frame.to_string().into())).await;
                            break 'outer;
                        }
                    }
                }
            }
        };
        let chunk = match chunk {
            Ok(Ok(c)) => c,
            _ => break,
        };
        seq = chunk.next_seq;
        if !chunk.data.is_empty() || chunk.eof {
            // An idle terminal must not keep its VM awake.
            state.activity.touch(&id);
            use base64::Engine;
            let frame = serde_json::json!({
                "data_b64": base64::engine::general_purpose::STANDARD.encode(&chunk.data),
                "eof": chunk.eof,
                "exit_code": chunk.exit_code,
                "next_seq": chunk.next_seq,
                "truncated": chunk.truncated,
            });
            let frame = frame.to_string();
            if let Some(transfer) = &transfer {
                transfer.output.take(frame.len()).await;
            }
            if !send_frame(&mut tx, Message::Text(frame.into())).await {
                break;
            }
        }
        if chunk.eof {
            break;
        }
    }
}

async fn send_frame(
    tx: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
) -> bool {
    use futures_util::SinkExt;
    matches!(
        tokio::time::timeout(Duration::from_secs(10), tx.send(message)).await,
        Ok(Ok(()))
    )
}
