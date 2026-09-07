//! Daemon API gate: full HTTP flow against `MockBackend` + in-memory
//! store, in-process via `oneshot` (no sockets, no KVM).
//!
//! Covers auth, ownership isolation, CRUD, exec/files/sessions,
//! snapshots/restore, pagination, validation, and quotas.

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
const TOKEN_B: &str = "test-token-B";

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

fn app() -> axum::Router {
    let dir = std::env::temp_dir().join(format!("ahvm-daemon-test-{}", std::process::id()));
    let store = Arc::new(Store::open_in_memory().unwrap());
    store.upsert_user(&user("alice", TOKEN_A)).unwrap();
    store.upsert_user(&user("bob", TOKEN_B)).unwrap();
    store
        .upsert_user(&ahvm_store::User {
            id: "carol".to_string(),
            name: "carol".to_string(),
            api_key_hash: hash("carol-token"),
            max_sandboxes: 1,
            max_cpus: 32,
            max_memory_mb: 65536,
            max_volumes_mb: 0,
            max_snapshots: 0,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    store
        .upsert_user(&ahvm_store::User {
            id: "dave".to_string(),
            name: "dave".to_string(),
            api_key_hash: hash("dave-token"),
            max_sandboxes: 8,
            max_cpus: 8,
            max_memory_mb: 768,
            max_volumes_mb: 0,
            max_snapshots: 0,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    let backend: Arc<dyn ahvm_engine::Backend> = Arc::new(MockBackend::new(dir.join("snapshots")));
    build_router(AppState {
        store,
        backend,
        quotas: ahvm_daemon::quotas::Registry::new(),
        activity: ahvm_daemon::thermal::ActivityTracker::new(),
        ops: ahvm_daemon::scheduler::OpsLimiter::new(4),
        lifecycle: ahvm_daemon::scheduler::LifecycleLocks::new(),
    })
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
async fn healthz_needs_no_auth() {
    let (status, body) = call(app(), None, "GET", "/v1/healthz", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn auth_rejects_missing_and_bad_tokens() {
    let (status, body) = call(app(), None, "GET", "/v1/sandboxes", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "unauthorized");
    let (status, _) = call(app(), Some("wrong"), "GET", "/v1/sandboxes", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sandbox_crud_exec_and_delete() {
    let app = app();
    let (status, created) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "web", "cpus": 1, "memory_mb": 512 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["state"], "running");

    let (status, got) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        &format!("/v1/sandboxes/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["id"], id.as_str());

    let (status, out) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/sandboxes/{id}/exec"),
        Some(serde_json::json!({ "argv": ["echo", "hi"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["exit_code"], 0);
    assert!(out["stdout"].as_str().unwrap().contains("echo hi"));

    // Validation: empty argv is 422, unknown sandbox is 404.
    let (status, _) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/sandboxes/{id}/exec"),
        Some(serde_json::json!({ "argv": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        "/v1/sandboxes/nope",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = call(
        app.clone(),
        Some(TOKEN_A),
        "DELETE",
        &format!("/v1/sandboxes/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        app,
        Some(TOKEN_A),
        "GET",
        &format!("/v1/sandboxes/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ownership_isolation_is_404_both_ways() {
    let app = app();
    let (_, created) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "private" })),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    // Bob sees nothing: get, exec, and delete are all 404 (never 403).
    for (method, uri) in [
        ("GET", format!("/v1/sandboxes/{id}")),
        ("POST", format!("/v1/sandboxes/{id}/exec")),
        ("DELETE", format!("/v1/sandboxes/{id}")),
    ] {
        let body = (method == "POST").then(|| serde_json::json!({ "argv": ["true"] }));
        let (status, _) = call(app.clone(), Some(TOKEN_B), method, &uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
}

#[tokio::test]
async fn list_paginates_with_cursors() {
    let app = app();
    for name in ["p1", "p2", "p3"] {
        let (status, _) = call(
            app.clone(),
            Some(TOKEN_A),
            "POST",
            "/v1/sandboxes",
            Some(serde_json::json!({ "name": name })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, page1) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        "/v1/sandboxes?limit=2",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page1["sandboxes"].as_array().unwrap().len(), 2);
    let cursor = page1["next_cursor"].as_str().unwrap().to_string();
    let (status, page2) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        &format!("/v1/sandboxes?limit=2&after={cursor}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page2["sandboxes"].as_array().unwrap().len(), 1);
    assert!(page2["next_cursor"].is_null());
    // Bob's list is empty: ownership scopes listing, not just reads.
    let (status, bob) = call(app, Some(TOKEN_B), "GET", "/v1/sandboxes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(bob["sandboxes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn quotas_reject_over_limit_creates() {
    let app = app();
    // Carol holds exactly one sandbox, generously sized; snapshots barred.
    let (status, only) = call(
        app.clone(),
        Some("carol-token"),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "only", "cpus": 1, "memory_mb": 512 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Backend-assigned ids are canonical in URLs (the mock generates them;
    // krucible honors the name — the daemon never assumes either).
    let only_id = only["id"].as_str().unwrap().to_string();
    // Second sandbox: count quota.
    let (status, body) = call(
        app.clone(),
        Some("carol-token"),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "extra" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "forbidden");
    // Snapshots: zero allowed.
    let (status, _) = call(
        app.clone(),
        Some("carol-token"),
        "POST",
        &format!("/v1/sandboxes/{only_id}/snapshots"),
        Some(serde_json::json!({ "name": "s" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Dave is CPU/RAM bound instead: one small box fits, a bigger one does not.
    let (status, _) = call(
        app.clone(),
        Some("dave-token"),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "small", "cpus": 1, "memory_mb": 512 })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    for (name, body) in [
        (
            "cpu",
            serde_json::json!({ "name": "big-cpu", "cpus": 8, "memory_mb": 128 }),
        ),
        (
            "mem",
            serde_json::json!({ "name": "big-mem", "cpus": 1, "memory_mb": 1024 }),
        ),
    ] {
        let (status, _) = call(
            app.clone(),
            Some("dave-token"),
            "POST",
            "/v1/sandboxes",
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name} quota");
    }
}

#[tokio::test]
async fn concurrent_creates_enforce_quota() {
    // The reviewer's repro: two concurrent creates with max_sandboxes=1.
    // Exactly one succeeds regardless of interleaving — serialized, the
    // second sees the committed row; overlapped, it sees the hold.
    use std::sync::Arc;
    use tokio::sync::Barrier;
    let store = ahvm_store::Store::open_in_memory().unwrap();
    store
        .upsert_user(&ahvm_store::User {
            id: "racer".to_string(),
            name: "racer".to_string(),
            api_key_hash: hash("racer-token"),
            max_sandboxes: 1,
            max_cpus: 32,
            max_memory_mb: 65536,
            max_volumes_mb: 0,
            max_snapshots: 0,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    // Rebuild the app on a store containing the racer (app() seeds fixed users).
    let dir = std::env::temp_dir().join(format!("ahvm-daemon-race-{}", std::process::id()));
    let backend: Arc<dyn ahvm_engine::Backend> = Arc::new(MockBackend::new(dir.join("snapshots")));
    let app = build_router(AppState {
        store: Arc::new(store),
        backend,
        quotas: ahvm_daemon::quotas::Registry::new(),
        activity: ahvm_daemon::thermal::ActivityTracker::new(),
        ops: ahvm_daemon::scheduler::OpsLimiter::new(4),
        lifecycle: ahvm_daemon::scheduler::LifecycleLocks::new(),
    });
    let barrier = Arc::new(Barrier::new(2));
    let mk = |name: &'static str| {
        let app = app.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            call(
                app,
                Some("racer-token"),
                "POST",
                "/v1/sandboxes",
                Some(serde_json::json!({ "name": name })),
            )
            .await
            .0
        })
    };
    let (a, b) = tokio::join!(mk("race-a"), mk("race-b"));
    let mut codes = [a.unwrap(), b.unwrap()];
    codes.sort();
    assert_eq!(
        codes,
        [StatusCode::CREATED, StatusCode::FORBIDDEN],
        "exactly one concurrent create must win"
    );
}

#[tokio::test]
async fn stop_start_cycle_gates_exec() {
    let app = app();
    let (_, created) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "cycler" })),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    let (status, stopped) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/sandboxes/{id}/stop"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stopped["state"], "stopped");
    let (status, _) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/sandboxes/{id}/exec"),
        Some(serde_json::json!({ "argv": ["true"] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, running) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/sandboxes/{id}/start"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(running["state"], "running");
}

#[tokio::test]
async fn files_roundtrip() {
    let app = app();
    let (_, created) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "filer" })),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    use base64::Engine;
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(b"file-bytes");
    let (status, wrote) = call(
        app.clone(),
        Some(TOKEN_A),
        "PUT",
        &format!("/v1/sandboxes/{id}/files"),
        Some(serde_json::json!({ "path": "/w.txt", "data_b64": data_b64 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(wrote["bytes"], 10);
    let (status, read) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        &format!("/v1/sandboxes/{id}/files?path=/w.txt&offset=5&limit=100"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["eof"], true);
    let raw = base64::engine::general_purpose::STANDARD
        .decode(read["data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(raw, b"bytes");
    let (status, missing) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        &format!("/v1/sandboxes/{id}/files?path=/nope"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = missing;
    let (status, listed) = call(
        app,
        Some(TOKEN_A),
        "GET",
        &format!("/v1/sandboxes/{id}/dir?path=/"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "w.txt"));
}

#[tokio::test]
async fn sessions_flow_over_rest() {
    let app = app();
    let (_, created) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "sess" })),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    let (status, started) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/sandboxes/{id}/sessions"),
        Some(serde_json::json!({ "argv": ["echo", "yo"] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let sid = started["session_id"].as_str().unwrap().to_string();
    let (status, chunk) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        &format!("/v1/sandboxes/{id}/sessions/{sid}/read?from_seq=0"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(chunk["eof"], true);
    let (status, listed) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        &format!("/v1/sandboxes/{id}/sessions"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    // Kill on the already-exited echo session is a 422 (guest error
    // semantics: kill targets running sessions; delete removes records).
    let (status, _) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/sandboxes/{id}/sessions/{sid}/kill"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = call(
        app,
        Some(TOKEN_A),
        "DELETE",
        &format!("/v1/sandboxes/{id}/sessions/{sid}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn snapshots_restore_as_new() {
    let app = app();
    let (_, created) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        "/v1/sandboxes",
        Some(serde_json::json!({ "name": "snapper" })),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    let (status, snap) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/sandboxes/{id}/snapshots"),
        Some(serde_json::json!({ "name": "s1" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let snap_id = snap["id"].as_str().unwrap().to_string();
    let (status, got) = call(
        app.clone(),
        Some(TOKEN_A),
        "GET",
        &format!("/v1/snapshots/{snap_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["name"], "s1");
    let (status, restored) = call(
        app.clone(),
        Some(TOKEN_A),
        "POST",
        &format!("/v1/snapshots/{snap_id}/restore"),
        Some(serde_json::json!({ "new_id": "snapper-2" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(restored["id"], "snapper-2");
    // Bob cannot see or restore Alice's snapshot.
    let (status, _) = call(
        app.clone(),
        Some(TOKEN_B),
        "GET",
        &format!("/v1/snapshots/{snap_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        app,
        Some(TOKEN_A),
        "DELETE",
        &format!("/v1/snapshots/{snap_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn start_enforces_resources_without_double_counting_sandbox() {
    // Check CPU and memory independently, then starting at the sandbox-count cap.
    for (max_sb, max_cpu, max_mem) in [(2, 1, 4096), (2, 8, 512), (1, 1, 512)] {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut u = user("alice", TOKEN_A);
        u.max_sandboxes = max_sb;
        u.max_cpus = max_cpu;
        u.max_memory_mb = max_mem;
        store.upsert_user(&u).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "start-quota-{}-{max_sb}-{max_cpu}",
            std::process::id()
        ));
        let app = build_router(AppState {
            store,
            backend: Arc::new(MockBackend::new(dir)),
            quotas: ahvm_daemon::quotas::Registry::new(),
            activity: ahvm_daemon::thermal::ActivityTracker::new(),
            ops: ahvm_daemon::scheduler::OpsLimiter::new(4),
            lifecycle: ahvm_daemon::scheduler::LifecycleLocks::new(),
        });
        let (status, first) = call(
            app.clone(),
            Some(TOKEN_A),
            "POST",
            "/v1/sandboxes",
            Some(serde_json::json!({"name":"first"})),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let id = first["id"].as_str().unwrap();
        let start = format!("/v1/sandboxes/{id}/start");
        let stop = format!("/v1/sandboxes/{id}/stop");
        assert_eq!(
            call(app.clone(), Some(TOKEN_A), "POST", &start, None)
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(app.clone(), Some(TOKEN_A), "POST", &stop, None)
                .await
                .0,
            StatusCode::OK
        );
        if max_sb == 2 {
            let (status, second) = call(
                app.clone(),
                Some(TOKEN_A),
                "POST",
                "/v1/sandboxes",
                Some(serde_json::json!({"name":"second"})),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED);
            assert_eq!(
                call(app.clone(), Some(TOKEN_A), "POST", &start, None)
                    .await
                    .0,
                StatusCode::FORBIDDEN
            );
            let get = format!("/v1/sandboxes/{id}");
            assert_eq!(
                call(app.clone(), Some(TOKEN_A), "GET", &get, None).await.1["state"],
                "stopped"
            );
            let second_id = second["id"].as_str().unwrap();
            assert_eq!(
                call(
                    app.clone(),
                    Some(TOKEN_A),
                    "POST",
                    &format!("/v1/sandboxes/{second_id}/stop"),
                    None
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        // Rejected starts retain no hold; stopping the other VM makes room.
        assert_eq!(
            call(app.clone(), Some(TOKEN_A), "POST", &start, None)
                .await
                .0,
            StatusCode::OK
        );
    }
}
