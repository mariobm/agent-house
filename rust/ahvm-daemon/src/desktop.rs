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
    let stream_guard = state
        .ops
        .try_stream(&id)
        .ok_or_else(|| ApiError::Conflict("workspace stream limit reached; retry later".into()))?;
    let permit = CONNECTIONS
        .try_acquire()
        .map_err(|_| ApiError::Conflict("desktop connection limit reached".into()))?;
    let guard = crate::routes::guest(&state, &id).await?;
    let transfer = state.ops.transfers.get(&id);
    let backend = state.backend.clone();
    let (stream, guard) = blocking(move || {
        backend
            .desktop_connect(&id)
            .map(|stream| (stream, (guard, stream_guard)))
    })
    .await?;
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
            let _ = timeout(Duration::from_secs(3600), bridge(socket, stream, transfer)).await;
        }))
}

async fn bridge(
    socket: WebSocket,
    mut stream: tokio::net::UnixStream,
    transfer: Option<std::sync::Arc<crate::bandwidth::Transfer>>,
) {
    let (mut tx, mut rx) = socket.split();
    let mut bytes = vec![0; 64 * 1024];
    loop {
        tokio::select! {
            result = stream.read(&mut bytes) => match result {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(transfer) = &transfer { transfer.output.take(n).await; }
                    if !matches!(timeout(Duration::from_secs(10), tx.send(Message::Binary(bytes[..n].to_vec().into()))).await, Ok(Ok(()))) { break; }
                },
            },
            frame = rx.next() => match frame {
                Some(Ok(Message::Binary(data))) => {
                    if let Some(transfer) = &transfer { transfer.input.take(data.len()).await; }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};

    #[tokio::test]
    async fn desktop_output_uses_the_shared_vm_bandwidth_budget() {
        let registry = crate::bandwidth::Registry::new(Some(65536));
        let transfer = registry.get("desktop").unwrap();
        let (guest, transport) = tokio::net::UnixStream::pair().unwrap();
        let transport = std::sync::Arc::new(std::sync::Mutex::new(Some(transport)));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/",
            get(move |ws: WebSocketUpgrade| {
                let stream = transport.lock().unwrap().take().unwrap();
                let transfer = transfer.clone();
                async move { ws.on_upgrade(move |socket| bridge(socket, stream, Some(transfer))) }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/"))
            .await
            .unwrap();
        // Consume the shared credit as another VM API transfer would.
        registry.get("desktop").unwrap().output.take(65536).await;
        let writer = tokio::spawn(async move {
            let mut guest = guest;
            guest.write_all(&vec![42; 65536]).await.unwrap();
        });
        let started = tokio::time::Instant::now();
        let mut received = 0;
        while received < 65536 {
            let frame = timeout(Duration::from_secs(5), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if let tokio_tungstenite::tungstenite::Message::Binary(bytes) = frame {
                assert!(bytes.iter().all(|b| *b == 42));
                received += bytes.len();
            }
        }
        assert!(started.elapsed() >= Duration::from_millis(750));
        writer.await.unwrap();
        server.abort();
    }
}
