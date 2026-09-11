//! Authenticated raw VNC transport for explicitly enabled desktop VMs.
use crate::{auth::UserId, blocking, routes::owned, ApiError, ApiResult, AppState};
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    Extension,
};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{timeout, Duration},
};

static CONNECTIONS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

pub async fn stream(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<axum::response::Response> {
    owned(&state, &user.0, &id).await?;
    let permit = CONNECTIONS
        .try_acquire()
        .map_err(|_| ApiError::Conflict("desktop connection limit reached".into()))?;
    let guard = state
        .activity
        .begin(&id)
        .ok_or_else(|| ApiError::Conflict("sandbox is stopping".into()))?;
    let backend = state.backend.clone();
    let (stream, guard) =
        blocking(move || backend.desktop_connect(&id).map(|stream| (stream, guard))).await?;
    stream
        .set_nonblocking(true)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let stream =
        tokio::net::UnixStream::from_std(stream).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(ws
        .max_message_size(256 * 1024)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| async move {
            let (_permit, _guard) = (permit, guard);
            // A connected viewer keeps the desktop awake, bounded to one hour.
            let _ = timeout(Duration::from_secs(3600), bridge(socket, stream)).await;
        }))
}

async fn bridge(socket: WebSocket, mut stream: tokio::net::UnixStream) {
    let (mut tx, mut rx) = socket.split();
    let mut bytes = vec![0; 64 * 1024];
    loop {
        tokio::select! {
            result = stream.read(&mut bytes) => match result {
                Ok(0) | Err(_) => break,
                Ok(n) => if !matches!(timeout(Duration::from_secs(10), tx.send(Message::Binary(bytes[..n].to_vec().into()))).await, Ok(Ok(()))) { break; },
            },
            frame = rx.next() => match frame {
                Some(Ok(Message::Binary(data))) => {
                    if !matches!(timeout(Duration::from_secs(10), stream.write_all(&data)).await, Ok(Ok(()))) { break; }
                }
                Some(Ok(Message::Ping(data))) => {
                    if !matches!(timeout(Duration::from_secs(10), tx.send(Message::Pong(data))).await, Ok(Ok(()))) { break; }
                }
                Some(Ok(Message::Pong(_))) => {},
                _ => break,
            }
        }
    }
}
