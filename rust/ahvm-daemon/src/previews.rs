//! Authenticated HTTP previews on distinct per-sandbox/port browser origins.
use crate::{auth::UserId, blocking, routes::owned, ApiError, ApiResult, AppState};
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    Extension, Json, Router,
};
use http_body_util::BodyExt;
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::sync::Semaphore;
static CONNECTIONS: Semaphore = Semaphore::const_new(64);
const LIFETIME: Duration = Duration::from_secs(300);

pub async fn list(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    owned(&state, &user.0, &id).await?;
    Ok(Json(
        state
            .store
            .preview_ports(&id)?
            .into_iter()
            .map(|port| serde_json::json!({"port":port,"host_label":host_label(&id,port)}))
            .collect(),
    ))
}
pub async fn enable(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, port)): Path<(String, u16)>,
) -> ApiResult<StatusCode> {
    set(&state, &user.0, &id, port, true).await
}
pub async fn disable(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, port)): Path<(String, u16)>,
) -> ApiResult<StatusCode> {
    set(&state, &user.0, &id, port, false).await
}
async fn set(
    state: &AppState,
    user: &str,
    id: &str,
    port: u16,
    enabled: bool,
) -> ApiResult<StatusCode> {
    if port == 0 {
        return Err(ApiError::Invalid("port must be nonzero".into()));
    }
    let _lock = state.lifecycle.lock(id).await;
    owned(state, user, id).await?;
    state.store.set_preview_port(id, port, enabled)?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Clone)]
struct Domain(String, bool);
impl Domain {
    fn cookie_name(&self) -> &'static str {
        if self.1 {
            "__Host-ahvm_preview"
        } else {
            "ahvm_preview"
        }
    }
}
fn preview_cookie(pair: &str) -> bool {
    pair.split_once('=')
        .is_some_and(|(name, _)| matches!(name.trim(), "ahvm_preview" | "__Host-ahvm_preview"))
}
pub fn router(state: AppState, domain: &str) -> ApiResult<Router> {
    if domain.is_empty()
        || domain.len() > 100
        || !domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
    {
        return Err(ApiError::Invalid("invalid preview domain".into()));
    }
    let domain = domain.to_ascii_lowercase();
    Ok(Router::new()
        .fallback(proxy)
        .layer(Extension(Domain(
            domain.to_ascii_lowercase(),
            !domain.ends_with(".localhost") && domain != "localhost",
        )))
        .layer(axum::middleware::from_fn_with_state(
            (
                state.clone(),
                Domain(
                    domain.to_ascii_lowercase(),
                    !domain.ends_with(".localhost") && domain != "localhost",
                ),
            ),
            authorize,
        ))
        .with_state(state))
}
fn target(headers: &HeaderMap, domain: &str) -> ApiResult<(String, u16)> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::Invalid("missing preview Host".into()))?;
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    let label = host
        .strip_suffix(&format!(".{domain}"))
        .ok_or_else(|| ApiError::NotFound("preview host".into()))?;
    let (encoded_id, port) = label
        .rsplit_once("--")
        .ok_or_else(|| ApiError::NotFound("preview host".into()))?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| ApiError::Invalid("invalid preview port".into()))?;
    let id = hex::decode(encoded_id.replace('.', ""))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| ApiError::Invalid("invalid preview sandbox label".into()))?;
    if id.is_empty() || id.len() > 64 || id.contains('/') || host_label(&id, port) != label {
        return Err(ApiError::Invalid("invalid preview sandbox label".into()));
    }
    Ok((id, port))
}
fn strip_hop(headers: &mut HeaderMap) {
    let named: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|v| v.trim().parse::<header::HeaderName>().ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}
fn active(state: &AppState, id: &str, port: u16, generation: &[u8]) -> bool {
    state
        .store
        .preview_generation(id, port)
        .is_ok_and(|current| current.as_deref() == Some(generation))
}
async fn proxy(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Extension(domain): Extension<Domain>,
    mut request: Request,
) -> ApiResult<Response> {
    let (id, port) = target(request.headers(), &domain.0)?;
    owned(&state, &user.0, &id).await?;
    let generation = state
        .store
        .preview_generation(&id, port)?
        .ok_or_else(|| ApiError::NotFound("preview port".into()))?;
    if matches!(
        *request.method(),
        axum::http::Method::CONNECT | axum::http::Method::TRACE
    ) {
        return Err(ApiError::Invalid("unsupported preview method".into()));
    }
    let permit = CONNECTIONS
        .try_acquire()
        .map_err(|_| ApiError::Conflict("preview capacity exhausted".into()))?;
    let guard = state
        .activity
        .begin(&id)
        .ok_or_else(|| ApiError::Conflict("sandbox stopping".into()))?;
    let backend = state.backend.clone();
    let connect_id = id.clone();
    let raw = blocking(move || backend.preview_connect(&connect_id, port)).await?;
    raw.set_nonblocking(true)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let stream =
        tokio::net::UnixStream::from_std(raw).map_err(|e| ApiError::Internal(e.to_string()))?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(upstream)?;
    let driver = Driver(tokio::spawn(async move {
        let _ = connection.with_upgrades().await;
    }));
    let websocket = request
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    let client_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));
    strip_hop(request.headers_mut());
    request.headers_mut().remove(header::AUTHORIZATION);
    if let Some(cookies) = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        let clean = cookies
            .split(';')
            .filter(|c| !preview_cookie(c))
            .collect::<Vec<_>>()
            .join(";");
        request.headers_mut().remove(header::COOKIE);
        if !clean.trim().is_empty() {
            request.headers_mut().insert(
                header::COOKIE,
                HeaderValue::from_str(&clean)
                    .map_err(|_| ApiError::Invalid("invalid cookies".into()))?,
            );
        }
    }
    // Never forward proxy-supplied identity or routing hints from the client.
    for name in [
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
    ] {
        request.headers_mut().remove(name);
    }
    if websocket {
        request
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        request
            .headers_mut()
            .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    }
    *request.uri_mut() = request
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("/")
        .parse()
        .map_err(|_| ApiError::Invalid("invalid preview path".into()))?;
    let deadline = tokio::time::Instant::now() + LIFETIME;
    let mut response =
        match tokio::time::timeout(Duration::from_secs(30), sender.send_request(request)).await {
            Ok(Ok(response)) => response,
            result => {
                driver.0.abort();
                return Err(ApiError::Unavailable(format!(
                    "preview response: {result:?}"
                )));
            }
        };
    let server_upgrade = (response.status() == StatusCode::SWITCHING_PROTOCOLS && websocket)
        .then(|| hyper::upgrade::on(&mut response));
    strip_hop(response.headers_mut());
    let cookies: Vec<_> = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter(|v| {
            v.to_str()
                .is_ok_and(|s| !preview_cookie(s.split(';').next().unwrap_or("")))
        })
        .cloned()
        .collect();
    response.headers_mut().remove(header::SET_COOKIE);
    for cookie in cookies {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    if let (Some(client), Some(server)) = (client_upgrade, server_upgrade) {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        response
            .headers_mut()
            .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        tokio::spawn(async move {
            let _held = (permit, guard);
            let relay = async {
                let (client, server) = tokio::try_join!(client, server)?;
                let _ = tokio::io::copy_bidirectional(
                    &mut TokioIo::new(client),
                    &mut TokioIo::new(server),
                )
                .await;
                Ok::<_, hyper::Error>(())
            };
            tokio::pin!(relay);
            loop {
                tokio::select! {
                    _ = &mut relay => break,
                    _ = tokio::time::sleep_until(deadline) => break,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => { if !active(&state,&id,port,&generation) { break; } }
                }
            }
            driver.0.abort();
        });
        return Ok(response.map(|_| Body::empty()));
    }
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        driver.0.abort();
        return Err(ApiError::Unavailable("unexpected guest upgrade".into()));
    }
    let (parts, mut body) = response.into_parts();
    // A separate bounded producer owns the activity guard. Its deadline still
    // fires if a slow client stops polling the response body altogether.
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let _held = (permit, guard, driver);
        let transfer = async {
            while let Some(frame) = body.frame().await {
                match frame {
                    Ok(frame) => {
                        if let Ok(bytes) = frame.into_data() {
                            if tx.send(Ok::<_, std::io::Error>(bytes)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(std::io::Error::other(e))).await;
                        break;
                    }
                }
            }
        };
        tokio::pin!(transfer);
        loop {
            tokio::select! {
                _ = &mut transfer => break,
                _ = tx.closed() => break,
                _ = tokio::time::sleep_until(deadline) => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => if !active(&state,&id,port,&generation) { break; }
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Ok(Response::from_parts(parts, Body::from_stream(stream)))
}
struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn upstream(e: hyper::Error) -> ApiError {
    ApiError::Unavailable(format!("preview upstream: {e}"))
}

fn token_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}
/// Mint a one-hour, preview-only browser credential. Rotation invalidates the
/// previous link/cookie; its hash is the only credential persisted in SQLite.
pub async fn access(
    State(state): State<AppState>,
    Extension(user): Extension<UserId>,
    Path((id, port)): Path<(String, u16)>,
) -> ApiResult<Json<serde_json::Value>> {
    use std::io::Read;
    let _lock = state.lifecycle.lock(&id).await;
    owned(&state, &user.0, &id).await?;
    let mut secret = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut secret))
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let token = hex::encode(secret);
    let expires = crate::unix_now() + 3600;
    state
        .store
        .preview_token(&id, port, &token_hash(&token), expires)?;
    Ok(Json(
        serde_json::json!({"token":token,"expires_at":expires,"host_label":host_label(&id,port)}),
    ))
}
async fn authorize(
    State((state, domain)): State<(AppState, Domain)>,
    mut request: Request,
    next: axum::middleware::Next,
) -> ApiResult<Response> {
    let (id, port) = target(request.headers(), &domain.0)?;
    if request.headers().contains_key(header::AUTHORIZATION)
        && !request
            .uri()
            .query()
            .unwrap_or("")
            .split('&')
            .any(|p| p.starts_with("ahvm_token="))
    {
        return crate::auth::require_user(State(state), request, next).await;
    }
    let query_token = request
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("ahvm_token="))
        .map(str::to_owned);
    let cookie = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(';').find_map(|p| {
                p.trim()
                    .split_once('=')
                    .filter(|(name, _)| *name == domain.cookie_name())
                    .map(|(_, value)| value)
            })
        })
        .map(str::to_owned);
    let token = query_token
        .as_ref()
        .or(cookie.as_ref())
        .filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        .ok_or(ApiError::Unauthorized)?;
    let expires = state
        .store
        .check_preview_token(&id, port, &token_hash(token), crate::unix_now())?
        .ok_or(ApiError::Unauthorized)?;
    if query_token.is_some() {
        if request.method() != axum::http::Method::GET {
            return Err(ApiError::Unauthorized);
        }
        let query = request
            .uri()
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|p| !p.starts_with("ahvm_token="))
            .collect::<Vec<_>>()
            .join("&");
        let mut location = request.uri().path().to_owned();
        // Avoid a scheme-relative redirect from an attacker-controlled path.
        if location.starts_with("//") || location.contains('\\') {
            location = "/".into();
        }
        if !query.is_empty() {
            location.push('?');
            location.push_str(&query);
        }
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::SEE_OTHER;
        response.headers_mut().insert(
            header::LOCATION,
            HeaderValue::from_str(&location)
                .map_err(|_| ApiError::Invalid("invalid preview path".into()))?,
        );
        response.headers_mut().insert(
            header::SET_COOKIE,
            HeaderValue::from_str(&format!(
                "{}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{}",
                domain.cookie_name(),
                expires - crate::unix_now(),
                if domain.1 { "; Secure" } else { "" }
            ))
            .unwrap(),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
            .headers_mut()
            .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        return Ok(response);
    }
    // Preview origins are isolated from each other as well as the API. Reject
    // sibling-origin scripted requests even though browsers call them same-site.
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let authority = origin
            .to_str()
            .ok()
            .and_then(|s| s.parse::<axum::http::Uri>().ok())
            .and_then(|u| u.authority().map(|a| a.as_str().to_owned()));
        if authority.as_deref() != Some(host) {
            return Err(ApiError::Forbidden("cross-origin preview request".into()));
        }
    }
    if let Some(site) = request.headers().get("sec-fetch-site") {
        let navigation = request
            .headers()
            .get("sec-fetch-dest")
            .is_some_and(|v| v == "document");
        if site != "same-origin" && site != "none" && !navigation {
            return Err(ApiError::Forbidden("cross-origin preview request".into()));
        }
    }
    let owner = state.store.get_sandbox(&id)?.owner_user_id;
    request.extensions_mut().insert(UserId(owner));
    Ok(next.run(request).await)
}

fn host_label(id: &str, port: u16) -> String {
    let encoded = hex::encode(id);
    let labels = encoded
        .as_bytes()
        .chunks(32)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect::<Vec<_>>()
        .join(".");
    format!("{labels}--{port}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tower::ServiceExt;
    fn state() -> AppState {
        let store = ahvm_store::Store::open_in_memory().unwrap();
        crate::auth::ensure_admin(&store, "admin-token").unwrap();
        store
            .create_sandbox(&ahvm_store::Sandbox {
                id: "id".into(),
                owner_user_id: "admin".into(),
                name: "id".into(),
                backend: ahvm_store::Backend::Krucible,
                state: "running".into(),
                thermal: "hot".into(),
                cpus: 1,
                memory_mb: 128,
                ip: "".into(),
                created_at: 1,
                updated_at: 1,
            })
            .unwrap();
        store.set_preview_port("id", 8080, true).unwrap();
        AppState {
            private_owners: Default::default(),
            store: Arc::new(store),
            backend: Arc::new(ahvm_engine::MockBackend::new(
                std::env::temp_dir().join("preview-auth-mock"),
            )),
            quotas: crate::quotas::Registry::new(),
            activity: crate::thermal::ActivityTracker::new(),
            ops: crate::scheduler::OpsLimiter::new(4),
            lifecycle: crate::scheduler::LifecycleLocks::new(),
        }
    }
    #[tokio::test]
    async fn browser_grant_is_scoped_expires_and_redirects_without_secret() {
        let state = state();
        let generation = state.store.preview_generation("id", 8080).unwrap().unwrap();
        state.store.set_preview_port("id", 8080, true).unwrap();
        assert!(active(&state, "id", 8080, &generation));
        state.store.set_preview_port("id", 8080, false).unwrap();
        state.store.set_preview_port("id", 8080, true).unwrap();
        assert!(!active(&state, "id", 8080, &generation));
        let token = "a".repeat(64);
        state
            .store
            .preview_token("id", 8080, &token_hash(&token), crate::unix_now() + 60)
            .unwrap();
        let app = router(state.clone(), "preview.example").unwrap();
        let req = Request::builder()
            .uri(format!("/page?x=1&ahvm_token={token}"))
            .header(header::HOST, "6964--8080.preview.example")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/page?x=1");
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(
            cookie.starts_with("__Host-ahvm_preview=")
                && cookie.contains("HttpOnly")
                && cookie.contains("Secure")
        );
        for (host, origin, expected) in [
            ("6964--8081.preview.example", None, StatusCode::UNAUTHORIZED),
            (
                "6964--8080.preview.example",
                Some("http://sibling.preview.example"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            let mut builder = Request::builder()
                .uri("/")
                .header(header::HOST, host)
                .header(header::COOKIE, format!("__Host-ahvm_preview={token}"));
            if let Some(origin) = origin {
                builder = builder.header(header::ORIGIN, origin);
            }
            assert_eq!(
                app.clone()
                    .oneshot(builder.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                expected
            );
        }
        // Browsers normalize a backslash to slash in special-scheme URLs.
        // /\evil.example must not become a scheme-relative redirect.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/\\evil.example/?ahvm_token={token}"))
                    .header(header::HOST, "6964--8080.preview.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/");
        state
            .store
            .preview_token("id", 8080, &token_hash(&token), crate::unix_now() - 1)
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::HOST, "6964--8080.preview.example")
                    .header(header::COOKIE, format!("__Host-ahvm_preview={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    #[test]
    fn host_labels_roundtrip_long_ids_without_origin_aliases() {
        let id = "a".repeat(64);
        let label = host_label(&id, 65535);
        assert!(label.split('.').all(|l| l.len() <= 63));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::HOST,
            format!("{label}.preview.localhost:8081").parse().unwrap(),
        );
        assert_eq!(target(&headers, "preview.localhost").unwrap(), (id, 65535));
        headers.insert(
            header::HOST,
            "69.64--8080.preview.localhost".parse().unwrap(),
        );
        assert!(target(&headers, "preview.localhost").is_err());
    }
    #[test]
    fn removes_connection_nominated_headers() {
        let mut headers = HeaderMap::new();
        headers.append(header::CONNECTION, "x-secret, keep-alive".parse().unwrap());
        headers.append(header::CONNECTION, "x-other".parse().unwrap());
        headers.insert("x-secret", "secret".parse().unwrap());
        headers.insert("x-other", "other".parse().unwrap());
        strip_hop(&mut headers);
        assert!(headers.is_empty());
    }
}
