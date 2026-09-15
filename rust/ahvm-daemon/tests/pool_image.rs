//! Pinned pool image protocol and canonical retry compatibility, using MockBackend.

use ahvm_daemon::{build_router, AppState};
use ahvm_engine::MockBackend;
use ahvm_store::{Store, User};
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN_A: &str = "test-token-A";

fn hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn user(id: &str, token: &str) -> User {
    User {
        id: id.to_string(),
        name: id.to_string(),
        api_key_hash: hash(token),
        max_sandboxes: 8,
        max_cpus: 32,
        max_memory_mb: 65536,
        max_volumes_mb: 102400,
        max_snapshots: 64,
        created_at: 1,
        updated_at: 1,
    }
}

fn test_state() -> AppState {
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.upsert_user(&user("alice", TOKEN_A)).unwrap();
    AppState {
        private_owners: Arc::new(Default::default()),
        store,
        backend: Arc::new(MockBackend::new(
            std::env::temp_dir().join("pool-mock-snapshots"),
        )),
        quotas: ahvm_daemon::quotas::Registry::new(),
        activity: ahvm_daemon::thermal::ActivityTracker::new(),
        ops: ahvm_daemon::scheduler::OpsLimiter::new(4),
        lifecycle: ahvm_daemon::scheduler::LifecycleLocks::new(),
    }
}

async fn call(
    app: axum::Router,
    token: Option<&str>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = builder
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    let json: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn pool_image_pin_is_admin_only_and_survives_alias_changes_on_retry() {
    let images = std::env::temp_dir().join(format!("ahvm-pool-images-{}", std::process::id()));
    std::fs::create_dir(&images).unwrap();
    std::env::set_var("AHVM_IMAGE_DIR", images.as_path());
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    for digest in [&a, &b] {
        std::fs::write(
            images.as_path().join(format!("{digest}.ext4")),
            b"mock image",
        )
        .unwrap();
    }
    let publish = |digest: &str| {
        std::fs::write(
            images.as_path().join("ubuntu-dev.json"),
            serde_json::json!({"sha256":digest,"guest_abi":1}).to_string(),
        )
        .unwrap();
    };
    publish(&a);
    let state = test_state();
    state
        .store
        .upsert_user(&user("admin", "admin-token"))
        .unwrap();
    let app = build_router(state);
    assert_eq!(
        call(
            app.clone(),
            Some(TOKEN_A),
            "GET",
            "/v1/admin/pool-profile",
            None
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let profile = call(
        app.clone(),
        Some("admin-token"),
        "GET",
        "/v1/admin/pool-profile",
        None,
    )
    .await;
    assert_eq!(profile.0, StatusCode::OK);
    assert_eq!(profile.1["image_digest"], a);
    let request = serde_json::json!({"action":"create","sandbox_id":"spare","cpus":1,"memory_mb":2048,"image_digest":a});
    assert_eq!(
        call(
            app.clone(),
            Some(TOKEN_A),
            "POST",
            "/v1/operations/pin",
            Some(request.clone())
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let created = call(
        app.clone(),
        Some("admin-token"),
        "POST",
        "/v1/operations/pin",
        Some(request.clone()),
    )
    .await;
    assert_eq!(created.1["status"], 201);
    publish(&b);
    let retry = call(
        app.clone(),
        Some("admin-token"),
        "POST",
        "/v1/operations/pin",
        Some(request),
    )
    .await;
    assert_eq!(retry.1["status"], 201);
    let stale = serde_json::json!({"action":"create","sandbox_id":"stale","cpus":1,"memory_mb":2048,"image_digest":a});
    let rejected = call(
        app.clone(),
        Some("admin-token"),
        "POST",
        "/v1/operations/stale",
        Some(stale),
    )
    .await;
    assert_eq!(rejected.1["status"], 409);
    assert_eq!(rejected.1["sandbox_state"], "absent");
    let malformed = serde_json::json!({"action":"create","sandbox_id":"bad","cpus":1,"memory_mb":2048,"image_digest":"../bad"});
    assert_eq!(
        call(
            app,
            Some("admin-token"),
            "POST",
            "/v1/operations/bad",
            Some(malformed)
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    std::env::remove_var("AHVM_IMAGE_DIR");
    std::fs::remove_dir_all(images).unwrap();
}
