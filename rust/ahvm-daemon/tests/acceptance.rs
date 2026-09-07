//! Step-4 acceptance gate: the whole product story over HTTP against a
//! real KVM backend, in-process (no sockets to bind).
//!
//! create → exec → files → session marker → stop → start → SIGKILL worker
//! → start (dead-worker restore) → snapshot → diverge disk → restore-as-new
//! (disk rollback + RAM marker) → daemon restart (adoption) → destroy.
//!
//! Gated like the engine KVM tests: `AHVM_KVM_TEST=1` with `/dev/kvm`,
//! `AHVM_VMM_BIN`, `AHVM_GUEST_IMAGE` and `LD_LIBRARY_PATH` set; elsewhere
//! a loud skip. The store is a temp FILE (restart persistence); the backend
//! data dir is shared across the restart.

#![cfg(unix)]

use ahvm_daemon::{build_router, AppState};
use ahvm_engine::{KrucibleBackend, KrucibleConfig};
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "acceptance-token";

fn hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

struct Ctx {
    app: axum::Router,
    backend: Arc<dyn ahvm_engine::Backend>,
    store: Arc<ahvm_store::Store>,
}

fn open(dir: &PathBuf) -> Ctx {
    // Store::open creates the file, not its parents (same rule as main).
    std::fs::create_dir_all(dir).expect("data dir");
    let store = Arc::new(ahvm_store::Store::open(dir.join("daemon.db")).expect("open store"));
    store
        .upsert_user(&ahvm_store::User {
            id: "admin".to_string(),
            name: "admin".to_string(),
            api_key_hash: hash(TOKEN),
            max_sandboxes: 16,
            max_cpus: 32,
            max_memory_mb: 65536,
            max_volumes_mb: 0,
            max_snapshots: 16,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    let backend: Arc<dyn ahvm_engine::Backend> = Arc::new(
        KrucibleBackend::open(KrucibleConfig::new(
            PathBuf::from(std::env::var("AHVM_VMM_BIN").unwrap()),
            PathBuf::from(std::env::var("AHVM_GUEST_IMAGE").unwrap()),
            dir.join("sandboxes"),
            std::env::var("LD_LIBRARY_PATH").unwrap(),
        ))
        .unwrap(),
    );
    let app = build_router(AppState {
        store: store.clone(),
        backend: backend.clone(),
        quotas: ahvm_daemon::quotas::Registry::new(),
        activity: ahvm_daemon::thermal::ActivityTracker::new(),
        ops: ahvm_daemon::scheduler::OpsLimiter::new(4),
        lifecycle: ahvm_daemon::scheduler::LifecycleLocks::new(),
    });
    Ctx {
        app,
        backend,
        store,
    }
}

/// Best-effort cleanup guard: destroying sandboxes and removing the temp
/// dir on BOTH success and failure paths (a panic unwinds past the
/// explicit cleanup section, and dropping the backend deliberately
/// preserves running workers — so do it explicitly here).
struct Cleanup {
    backend: Option<Arc<dyn ahvm_engine::Backend>>,
    store: Option<Arc<ahvm_store::Store>>,
    dir: PathBuf,
    ids: Vec<String>,
}

impl Cleanup {
    fn adopt(&mut self, ctx: &Ctx) {
        self.backend = Some(ctx.backend.clone());
        self.store = Some(ctx.store.clone());
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Test failed: keep workers AND files for post-mortem (probe
            // the corpse via the sockets under <dir>/sandboxes/*/sock,
            // read vmm.log + specs + bundles). Stale runs accumulate;
            // reruns use fresh pid-namespaced dirs, never these.
            eprintln!("acceptance artifacts kept at {}", self.dir.display());
            return;
        }
        if let (Some(be), Some(st)) = (self.backend.take(), self.store.take()) {
            for id in &self.ids {
                let _ = be.destroy(id);
                let _ = st.delete_sandbox(id);
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// (pid, starttime) identity of a live worker from its state.json.
fn worker_ident(dir: &std::path::Path, id: &str) -> (u64, Option<u64>) {
    let raw = std::fs::read(dir.join("sandboxes").join(id).join("state.json")).unwrap();
    let v: Value = serde_json::from_slice(&raw).unwrap();
    (v["pid"].as_u64().unwrap(), v["starttime"].as_u64())
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
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

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn unb64(v: &Value) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(v["data_b64"].as_str().unwrap())
        .unwrap()
}

// SAFETY: SIGKILL to a worker pid read from our own state.json, simulating
// host crash/admin kill. Asserts delivery; death itself is observed via API.
#[allow(unsafe_code)]
fn sigkill(pid: i32) {
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
}

fn worker_pid(dir: &std::path::Path, id: &str) -> i32 {
    let raw = std::fs::read(dir.join("sandboxes").join(id).join("state.json")).unwrap();
    let v: Value = serde_json::from_slice(&raw).unwrap();
    v["pid"].as_u64().unwrap() as i32
}

#[tokio::test]
async fn acceptance_crash_recovery_and_restart() {
    if std::env::var("AHVM_KVM_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP acceptance: set AHVM_KVM_TEST=1 on a KVM host");
        return;
    }
    assert!(
        std::path::Path::new("/dev/kvm").exists(),
        "AHVM_KVM_TEST=1 requires /dev/kvm"
    );
    let dir: PathBuf = std::env::temp_dir().join(format!("ahvm-accept-{}", std::process::id()));
    let mut cleanup = Cleanup {
        backend: None,
        store: None,
        dir: dir.clone(),
        ids: vec!["acc1".to_string(), "acc2".to_string()],
    };
    // Worker identities pre-restart (pid + starttime defeat pid reuse).
    let mut pre_restart = Vec::new();
    // Marker session id, needed again post-restart (RAM continuity proof).
    let marker_sid: String;
    {
        let ctx = open(&dir);
        cleanup.adopt(&ctx);
        let app = || ctx.app.clone();

        // ---- create + exec + files + session marker ----
        let (status, sb1) = call(
            app(),
            "POST",
            "/v1/sandboxes",
            Some(serde_json::json!({ "name": "acc1" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(sb1["state"], "running");
        let (status, out) = call(
            app(),
            "POST",
            "/v1/sandboxes/acc1/exec",
            Some(serde_json::json!({ "argv": ["/bin/sh", "-c", "printf acc-ok"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(out["stdout"], "acc-ok");
        let (status, _) = call(
            app(),
            "PUT",
            "/v1/sandboxes/acc1/files",
            Some(serde_json::json!({ "path": "/workspace/acc-proof", "data_b64": b64(b"V1") })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, sess) = call(
            app(),
            "POST",
            "/v1/sandboxes/acc1/sessions",
            Some(serde_json::json!({ "argv": ["/bin/sh", "-c", "echo ACCMARKER; sleep 600"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        marker_sid = sess["session_id"].as_str().unwrap().to_string();

        // ---- stop + start (bundle restore) ----
        assert_eq!(
            call(app(), "POST", "/v1/sandboxes/acc1/stop", None).await.0,
            StatusCode::OK
        );
        let (status, started) = call(app(), "POST", "/v1/sandboxes/acc1/start", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(started["thermal"], "warm");

        // ---- SIGKILL the live worker, then start (dead-worker restore) ----
        // SIGKILL is asynchronous: wait until the backend observes the death
        // (status reconciles to Failed) before restarting, or a start issued
        // in the kill/reap window correctly reports Ok without rebooting.
        sigkill(worker_pid(&dir, "acc1"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (status, got) = call(app(), "GET", "/v1/sandboxes/acc1", None).await;
            assert_eq!(status, StatusCode::OK);
            if got["state"] == "failed" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker death never observed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let (status, rebooted) = call(app(), "POST", "/v1/sandboxes/acc1/start", None).await;
        assert_eq!(status, StatusCode::OK, "start after SIGKILL must reboot");
        assert_eq!(rebooted["state"], "running");
        let (status, out) = call(
            app(),
            "POST",
            "/v1/sandboxes/acc1/exec",
            Some(serde_json::json!({ "argv": ["/bin/sh", "-c", "cat /workspace/acc-proof"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(out["stdout"], "V1");
        // RAM survived the kill: the pre-stop session re-attaches with output.
        let (status, chunk) = call(
            app(),
            "GET",
            &format!("/v1/sandboxes/acc1/sessions/{marker_sid}/read?from_seq=0&budget_ms=15000"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            unb64(&chunk)
                .windows(b"ACCMARKER\n".len())
                .any(|w| w == b"ACCMARKER\n"),
            "RAM marker lost across kill+restore"
        );

        // ---- snapshot, diverge disk, restore-as-new proves rollback ----
        let (status, snap) = call(
            app(),
            "POST",
            "/v1/sandboxes/acc1/snapshots",
            Some(serde_json::json!({ "name": "acc-snap" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let snap_id = snap["id"].as_str().unwrap().to_string();
        let (status, _) = call(
            app(),
            "PUT",
            "/v1/sandboxes/acc1/files",
            Some(serde_json::json!({ "path": "/workspace/acc-proof", "data_b64": b64(b"V2") })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, sb2) = call(
            app(),
            "POST",
            &format!("/v1/snapshots/{snap_id}/restore"),
            Some(serde_json::json!({ "new_id": "acc2" })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(sb2["thermal"], "warm");
        let (status, out) = call(
            app(),
            "POST",
            "/v1/sandboxes/acc2/exec",
            Some(serde_json::json!({ "argv": ["/bin/sh", "-c", "cat /workspace/acc-proof"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            out["stdout"], "V1",
            "restored disk must show frozen V1, not live V2"
        );
        // The live box must show the divergence (guards against a vacuous
        // rollback proof where V2 never actually landed).
        let (status, out) = call(
            app(),
            "POST",
            "/v1/sandboxes/acc1/exec",
            Some(serde_json::json!({ "argv": ["/bin/sh", "-c", "cat /workspace/acc-proof"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(out["stdout"], "V2", "live disk must show diverged V2");
        pre_restart.push(("acc1".to_string(), worker_ident(&dir, "acc1")));
        pre_restart.push(("acc2".to_string(), worker_ident(&dir, "acc2")));
    } // end phase 1: ctx and its request closure drop here (workers survive)

    // ---- daemon restart: reopen against the same dirs, adopt workers ----
    let ctx = open(&dir);
    cleanup.adopt(&ctx);
    let app = || ctx.app.clone();
    // Same processes, not replacements: pid + starttime identity must be
    // unchanged across the reopen (a reboot-on-open regression would pass
    // listing+exec but fail here).
    for (id, (pid, starttime)) in &pre_restart {
        let (now_pid, now_st) = worker_ident(&dir, id);
        assert_eq!(
            (now_pid, now_st),
            (*pid, *starttime),
            "worker {id} was replaced, not adopted"
        );
    }
    // RAM continuity across the restart: the pre-restart session on acc1
    // re-attaches with its marker output (same guest, same memory).
    let (status, chunk) = call(
        app(),
        "GET",
        &format!("/v1/sandboxes/acc1/sessions/{marker_sid}/read?from_seq=0&budget_ms=15000"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        unb64(&chunk)
            .windows(b"ACCMARKER\n".len())
            .any(|w| w == b"ACCMARKER\n"),
        "RAM marker lost across daemon restart"
    );
    let (status, list) = call(app(), "GET", "/v1/sandboxes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["sandboxes"].as_array().unwrap().len(), 2);
    for id in ["acc1", "acc2"] {
        let (status, out) = call(
            app(),
            "POST",
            &format!("/v1/sandboxes/{id}/exec"),
            Some(serde_json::json!({ "argv": ["/bin/sh", "-c", "printf adopted"] })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "exec on adopted {id} after restart");
        assert_eq!(out["stdout"], "adopted");
    }

    // ---- cleanup ----
    for id in ["acc1", "acc2"] {
        let (status, _) = call(app(), "DELETE", &format!("/v1/sandboxes/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let (status, list) = call(app(), "GET", "/v1/sandboxes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list["sandboxes"].as_array().unwrap().is_empty());
    let _ = dir;
}

/// The cleanup guard must run on failure paths too, not just success:
/// panic-unwind still drops it (destroy best-effort + temp dir removal).
/// No KVM needed (mock backend, memory store).
#[test]
fn cleanup_guard_runs_on_panic() {
    use ahvm_engine::MockBackend;
    let dir: PathBuf = std::env::temp_dir().join(format!("ahvm-cleanup-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let backend: Arc<dyn ahvm_engine::Backend> = Arc::new(MockBackend::new(dir.join("snaps")));
    let store = Arc::new(ahvm_store::Store::open_in_memory().unwrap());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = Cleanup {
            backend: Some(backend),
            store: Some(store),
            dir: dir.clone(),
            ids: vec!["ghost".to_string()],
        };
        panic!("boom");
    }));
    assert!(result.is_err());
    assert!(
        dir.exists(),
        "guard must KEEP artifacts (not clean) on panic"
    );
    // Tidy up after asserting (real failures keep theirs on the KVM host).
    let _ = std::fs::remove_dir_all(&dir);
}
