//! Optional native viewer. Long-lived API credentials stay outside the WebView.
mod keyboard_capture;

#[derive(Debug)]
enum UserEvent {
    Capture(bool),
}

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{io::Read, sync::Arc, time::Duration};
use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as RemoteMessage};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connection {
    url: String,
    authorization: String,
    title: String,
}
struct Local {
    connection: Connection,
    nonce: String,
    origin: String,
    connections: Arc<tokio::sync::Semaphore>,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Private stdin pipe from ahvm, never argv/environment or browser JS.
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 {
        return Err("connection settings too large".into());
    }
    let connection: Connection = serde_json::from_slice(&bytes)?;
    let endpoint = url::Url::parse(&connection.url)?;
    if !matches!(endpoint.scheme(), "ws" | "wss")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err("invalid desktop endpoint".into());
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let listener = runtime.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
    let origin = format!("http://{}", listener.local_addr()?);
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let page = format!("{origin}/{nonce}/");
    let title = format!("AHVM · {}", connection.title);
    let state = Arc::new(Local {
        connection,
        nonce,
        origin,
        connections: Arc::new(tokio::sync::Semaphore::new(1)),
    });
    let router = Router::new()
        .route("/{key}/", get(index))
        .route("/{key}/client.js", get(script))
        .route("/{key}/stream", get(stream))
        .with_state(state);
    runtime.spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title(title)
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 760.0))
        .build(&event_loop)?;
    let allowed = page.clone();
    let builder = wry::WebViewBuilder::new()
        .with_url(&page)
        .with_navigation_handler(move |url| url == allowed)
        .with_new_window_req_handler(|_, _| wry::NewWindowResponse::Deny)
        .with_ipc_handler(move |request| match request.body().as_str() {
            "capture:on" => {
                let _ = proxy.send_event(UserEvent::Capture(true));
            }
            "capture:off" | "disconnected" => {
                let _ = proxy.send_event(UserEvent::Capture(false));
            }
            "connected" => eprintln!("desktop: connected"),
            _ => {}
        })
        .with_devtools(false);
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window)?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().ok_or("missing GTK container")?)?
    };
    let mut capture = keyboard_capture::KeyboardCapture::default();
    event_loop.run(move |event, _, flow| {
        let _keep_alive = (&webview, &runtime);
        *flow = ControlFlow::Wait;
        let mode = match event {
            Event::UserEvent(UserEvent::Capture(true)) if window.is_focused() => {
                Some(if capture.enable() {
                    "native"
                } else {
                    "limited"
                })
            }
            Event::UserEvent(UserEvent::Capture(_))
            | Event::WindowEvent {
                event: WindowEvent::Focused(false),
                ..
            } => {
                capture.release();
                Some("off")
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            }
            | Event::LoopDestroyed => {
                capture.release();
                *flow = ControlFlow::Exit;
                None
            }
            _ => None,
        };
        if let Some(mode) = mode {
            // Fixed enum strings only. Never interpolate guest or credential data.
            let _ = webview.evaluate_script(&format!("window.ahvmCaptureState?.('{mode}')"));
        }
    });
}

async fn index(State(state): State<Arc<Local>>, Path(key): Path<String>) -> Response {
    asset(
        &state,
        &key,
        "text/html; charset=utf-8",
        include_bytes!("../web/index.html"),
    )
}
async fn script(State(state): State<Arc<Local>>, Path(key): Path<String>) -> Response {
    asset(
        &state,
        &key,
        "text/javascript; charset=utf-8",
        include_bytes!("../web/dist/client.js"),
    )
}
fn asset(state: &Local, key: &str, mime: &'static str, bytes: &'static [u8]) -> Response {
    if key != state.nonce {
        return StatusCode::NOT_FOUND.into_response();
    }
    ([ ("Content-Type", mime), ("Cache-Control", "no-store"), ("X-Content-Type-Options", "nosniff"),
       ("Referrer-Policy", "no-referrer"),
       ("Content-Security-Policy", "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'; img-src data: blob:; frame-ancestors 'none'; base-uri 'none'") ], bytes).into_response()
}
async fn stream(
    State(state): State<Arc<Local>>,
    Path(key): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if key != state.nonce
        || headers.get("origin").and_then(|v| v.to_str().ok()) != Some(state.origin.as_str())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(permit) = state.connections.clone().try_acquire_owned() else {
        return StatusCode::CONFLICT.into_response();
    };
    let Ok(mut request) = state.connection.url.as_str().into_client_request() else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let Ok(authorization) = state.connection.authorization.parse() else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    request.headers_mut().insert("Authorization", authorization);
    let remote = match tokio::time::timeout(
        Duration::from_secs(10),
        tokio_tungstenite::connect_async(request),
    )
    .await
    {
        Ok(Ok((stream, _))) => stream,
        _ => return StatusCode::BAD_GATEWAY.into_response(),
    };
    ws.max_message_size(256 * 1024)
        .max_frame_size(256 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            let _ = tokio::time::timeout(Duration::from_secs(3600), bridge(socket, remote)).await;
        })
}
async fn bridge(
    local: WebSocket,
    remote: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut local_tx, mut local_rx) = local.split();
    let (mut remote_tx, mut remote_rx) = remote.split();
    loop {
        tokio::select! {
            message = local_rx.next() => {
                let reply = match message {
                    Some(Ok(Message::Binary(data))) => RemoteMessage::Binary(data),
                    Some(Ok(Message::Pong(_))) => continue,
                    Some(Ok(Message::Ping(data))) => {
                        if !matches!(tokio::time::timeout(Duration::from_secs(10), local_tx.send(Message::Pong(data))).await, Ok(Ok(()))) { break; }
                        continue;
                    }
                    _ => break,
                };
                if !matches!(tokio::time::timeout(Duration::from_secs(10), remote_tx.send(reply)).await, Ok(Ok(()))) { break; }
            }
            message = remote_rx.next() => {
                let reply = match message {
                    Some(Ok(RemoteMessage::Binary(data))) => Message::Binary(data),
                    Some(Ok(RemoteMessage::Pong(_))) => continue,
                    Some(Ok(RemoteMessage::Ping(data))) => {
                        if !matches!(tokio::time::timeout(Duration::from_secs(10), remote_tx.send(RemoteMessage::Pong(data))).await, Ok(Ok(()))) { break; }
                        continue;
                    }
                    _ => break,
                };
                if !matches!(tokio::time::timeout(Duration::from_secs(10), local_tx.send(reply)).await, Ok(Ok(()))) { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::IntoFuture;

    #[test]
    fn local_stream_rejects_other_origins_and_wrong_capabilities() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let state = Arc::new(Local {
                connection: Connection {
                    url: "ws://127.0.0.1:1/".into(),
                    authorization: "Bearer secret".into(),
                    title: "test".into(),
                },
                nonce: "private-capability".into(),
                origin: format!("http://{address}"),
                connections: Arc::new(tokio::sync::Semaphore::new(1)),
            });
            assert_eq!(
                asset(&state, "wrong", "text/html", b"page").status(),
                StatusCode::NOT_FOUND
            );
            let page = asset(&state, "private-capability", "text/html", b"page");
            assert_eq!(page.headers()["cache-control"], "no-store");
            let server = tokio::spawn(
                axum::serve(
                    listener,
                    Router::new()
                        .route("/{key}/stream", get(stream))
                        .with_state(state),
                )
                .into_future(),
            );
            for (key, origin) in [
                ("wrong", Some(format!("http://{address}"))),
                (
                    "private-capability",
                    Some("https://attacker.invalid".into()),
                ),
                ("private-capability", None),
            ] {
                let mut request = format!("ws://{address}/{key}/stream")
                    .into_client_request()
                    .unwrap();
                if let Some(origin) = origin {
                    request
                        .headers_mut()
                        .insert("Origin", origin.parse().unwrap());
                }
                let error = tokio_tungstenite::connect_async(request).await.unwrap_err();
                match error {
                    tokio_tungstenite::tungstenite::Error::Http(response) => {
                        assert_eq!(response.status(), StatusCode::FORBIDDEN)
                    }
                    other => panic!("unexpected error: {other}"),
                }
            }
            server.abort();
        });
    }
}
