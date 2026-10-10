use super::*;
use ahvm_engine::{Backend, BackendKind, MockBackend, SandboxSpec};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const BOOT: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";

#[test]
fn health_capability_requires_configured_https_origin() {
    assert!(!crate::health_features(None).contains(&"managed-agent-events-v1"));
    for origin in [
        "",
        "not-a-url",
        "http://cloud.example",
        "https://cloud.example/path",
        "https://cloud.example/?q=x",
        "https://user@cloud.example",
    ] {
        assert!(!crate::health_features(Some(origin)).contains(&"managed-agent-events-v1"));
    }
    assert!(
        crate::health_features(Some("https://cloud.example")).contains(&"managed-agent-events-v1")
    );
    assert!(
        crate::health_features(Some("https://cloud.example/")).contains(&"managed-agent-events-v1")
    );
}
fn grant() -> RunEventDelivery {
    RunEventDelivery {
        url: "https://cloud.example/internal/agent-events/job".into(),
        token: "host-only-secret-01234567890123456789".into(),
        request_hash: "a".repeat(64),
    }
}
fn fixture() -> (AppState, ManagedRun, RunEventDelivery) {
    let store = Arc::new(ahvm_store::Store::open_in_memory().unwrap());
    crate::auth::ensure_admin(&store, "test-event-admin").unwrap();
    let backend = Arc::new(MockBackend::new(
        std::env::temp_dir().join("ahvm-event-tests"),
    ));
    backend
        .create(&SandboxSpec {
            name: "vm".into(),
            storage_mode: None,
            desktop: false,
            desktop_gpu: false,
            cpus: 1,
            memory_mb: 2048,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            extra_env: Default::default(),
            network_bytes_per_sec: None,
        })
        .unwrap();
    backend
        .file_write("vm", "/proc/sys/kernel/random/boot_id", BOOT.as_bytes())
        .unwrap();
    store
        .create_sandbox(&ahvm_store::Sandbox {
            id: "vm".into(),
            owner_user_id: "admin".into(),
            name: "vm".into(),
            backend: ahvm_store::Backend::Krucible,
            state: "running".into(),
            thermal: "hot".into(),
            cpus: 1,
            memory_mb: 2048,
            ip: "".into(),
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    let grant = grant();
    let request = RunRequest {
        sandbox_id: "vm".into(),
        argv: vec![
            "/usr/local/bin/ahvm-dev".into(),
            "/usr/bin/env".into(),
            "node".into(),
            "/home/ahvm/.local/state/ahvm-agent/runs/job/controller.mjs".into(),
            "/home/ahvm/.local/state/ahvm-agent/runs/job/input.json".into(),
        ],
        max_runtime_secs: 3600,
        fence_on_failure: false,
        session_isolated: true,
        event_delivery: Some(grant.clone()),
    };
    store
        .admit_managed_run(
            "job",
            "vm",
            "admin",
            &serde_json::to_string(&request).unwrap(),
            1,
            3601,
        )
        .unwrap();
    let run = store
        .update_managed_run(
            "job",
            1,
            "running",
            Some("session"),
            Some(BOOT),
            None,
            None,
            2,
            false,
        )
        .unwrap();
    let state = AppState {
        private_owners: Default::default(),
        store,
        backend,
        quotas: crate::quotas::Registry::new(),
        activity: crate::thermal::ActivityTracker::new(),
        ops: crate::scheduler::OpsLimiter::new(4),
        lifecycle: crate::scheduler::LifecycleLocks::new(),
    };
    (state, run, grant)
}
fn checkpoint() -> Value {
    json!({"schema":1,"harness":"pi","runId":"job","requestHash":"a".repeat(64),"sessionId":"ses_a","messageId":"msg_a","bootId":BOOT,"phase":"running","text":"Živjo 🦀","tools":[{"id":"tool_a","name":"bash","status":"pending","input":"echo one"}]})
}
fn line(seq: u64, key: &str, value: Value) -> Vec<u8> {
    let mut out = json!({"schema":1,"seq":seq,key:value})
        .to_string()
        .into_bytes();
    out.push(b'\n');
    out
}
fn write_journal(state: &AppState, bytes: &[u8]) {
    state
        .backend
        .file_write(
            "vm",
            "/home/ahvm/.local/state/ahvm-agent/runs/job/events.ndjson",
            bytes,
        )
        .unwrap();
}

#[test]
fn host_origin_and_callback_authority_are_narrow_and_redacted() {
    let expected = grant();
    assert!(validate_origin("job", &expected, "https://cloud.example").is_ok());
    assert!(!format!("{expected:?}").contains(&expected.token));
    for url in [
        "http://cloud.example/internal/agent-events/job",
        "https://evil.example/internal/agent-events/job",
        "https://cloud.example/internal/agent-events/other",
        "https://cloud.example/internal/agent-events/job?secret=x",
        "https://user:password@cloud.example/internal/agent-events/job",
        "https://cloud.example:444/internal/agent-events/job",
        "https://cloud.example/internal/agent-events/%6aob",
    ] {
        let mut changed = expected.clone();
        changed.url = url.into();
        assert!(
            validate_origin("job", &changed, "https://cloud.example").is_err(),
            "{url}"
        );
    }
    let mut changed = expected.clone();
    changed.token = "bad\r\nheader".into();
    assert!(validate_origin("job", &changed, "https://cloud.example").is_err());
}

#[test]
fn typescript_generated_fixture_matches_rust_codec_and_rejects_bad_patches() {
    let fixture_doc: Value = serde_json::from_str(include_str!("guest-journal-v1.json")).unwrap();
    let (state, mut run, grant) = fixture();
    let mut source = state.store.managed_run_event_stream("job").unwrap();
    run.id = fixture_doc["checkpoints"][0]["runId"]
        .as_str()
        .unwrap()
        .into();
    run.boot_id = Some(
        fixture_doc["checkpoints"][0]["bootId"]
            .as_str()
            .unwrap()
            .into(),
    );
    for (record, expected) in fixture_doc["records"]
        .as_array()
        .unwrap()
        .iter()
        .zip(fixture_doc["checkpoints"].as_array().unwrap())
    {
        let (seq, actual) =
            guest_record(record.to_string().as_bytes(), &source, &run, &grant).unwrap();
        assert_eq!(&actual, expected);
        source.guest_seq = seq;
        source.checkpoint_json = Some(actual.to_string());
    }
    for record in fixture_doc["rejected"].as_array().unwrap() {
        assert!(guest_record(record.to_string().as_bytes(), &source, &run, &grant).is_err());
    }
}

#[test]
fn persisted_host_envelope_fixture_for_cloud_decoder() {
    let (state, _, _) = fixture();
    let guest: Value = serde_json::from_str(include_str!("guest-journal-v1.json")).unwrap();
    let run_id = guest["checkpoints"][0]["runId"].as_str().unwrap();
    let boot = guest["checkpoints"][0]["bootId"].as_str().unwrap();
    let request_hash = "a20f02d01c399cce0cc2995549e202392e023a175e56231df696033605616265";
    state
        .backend
        .create(&SandboxSpec {
            name: "guest".into(),
            storage_mode: None,
            desktop: false,
            desktop_gpu: false,
            cpus: 1,
            memory_mb: 2048,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            extra_env: Default::default(),
            network_bytes_per_sec: None,
        })
        .unwrap();
    state
        .backend
        .file_write("guest", "/proc/sys/kernel/random/boot_id", boot.as_bytes())
        .unwrap();
    state
        .store
        .create_sandbox(&ahvm_store::Sandbox {
            id: "guest".into(),
            owner_user_id: "admin".into(),
            name: "guest".into(),
            backend: ahvm_store::Backend::Krucible,
            state: "running".into(),
            thermal: "hot".into(),
            cpus: 1,
            memory_mb: 2048,
            ip: "".into(),
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    let delivery = RunEventDelivery {
        url: format!("https://cloud.example/internal/agent-events/{run_id}"),
        token: "A".repeat(43),
        request_hash: request_hash.into(),
    };
    let request = RunRequest {
        sandbox_id: "guest".into(),
        argv: vec![
            "/usr/local/bin/ahvm-dev".into(),
            "/usr/bin/env".into(),
            "node".into(),
            format!("/home/ahvm/.local/state/ahvm-agent/runs/{run_id}/controller.mjs"),
            format!("/home/ahvm/.local/state/ahvm-agent/runs/{run_id}/input.json"),
        ],
        max_runtime_secs: 120,
        fence_on_failure: false,
        session_isolated: true,
        event_delivery: Some(delivery.clone()),
    };
    state
        .store
        .admit_managed_run(
            run_id,
            "guest",
            "admin",
            &serde_json::to_string(&request).unwrap(),
            1,
            121,
        )
        .unwrap();
    let run = state
        .store
        .update_managed_run(
            run_id,
            1,
            "running",
            Some("host_session_fixture"),
            Some(boot),
            None,
            None,
            2,
            false,
        )
        .unwrap();
    let mut journal = Vec::new();
    for record in guest["records"].as_array().unwrap() {
        let mut record = record.clone();
        if let Some(value) = record.get_mut("checkpoint") {
            value["requestHash"] = request_hash.into();
        }
        journal.extend(record.to_string().as_bytes());
        journal.push(b'\n');
    }
    state
        .backend
        .file_write(
            "guest",
            &format!("/home/ahvm/.local/state/ahvm-agent/runs/{run_id}/events.ndjson"),
            &journal,
        )
        .unwrap();
    capture(&state, &run, &delivery, true).unwrap();
    state
        .store
        .update_managed_run(run_id, 1, "succeeded", None, None, Some(0), None, 6, true)
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = state.store.first_managed_run_event(run_id).unwrap() {
        events.push(serde_json::from_str::<Value>(&event.payload_json).unwrap());
        state
            .store
            .acknowledge_managed_run_event(run_id, event.seq)
            .unwrap();
    }
    assert_eq!(events.len(), 5);
    assert_eq!(events[4]["kind"], "receipt");
    assert_eq!(events[4]["checkpoint"]["text"], "Final 🚀");
    let exported =
        json!({"run_id":run_id,"sandbox_id":"guest","request_hash":request_hash,"events":events});
    assert!(!exported.to_string().contains(&delivery.token));
    assert!(!exported.to_string().contains("event_delivery"));
    // Explicit opt-in only: exports real persisted bytes for cross-repo tests.
    if let Ok(file) = std::env::var("AHVM_EVENT_FIXTURE_OUT") {
        std::fs::write(file, serde_json::to_vec_pretty(&exported).unwrap()).unwrap();
    }
}

#[test]
fn long_unicode_journal_captures_6000_ordered_revisions_with_bounded_spool() {
    let (state, run, grant) = fixture();
    let started = std::time::Instant::now();
    let mut initial = checkpoint();
    initial["text"] = "".into();
    initial["tools"]=json!((0..10).map(|i|json!({"id":format!("tool_{i}"),"name":"bash","status":"running","input":"x".repeat(1024),"output":""})).collect::<Vec<_>>());
    let mut journal = line(1, "checkpoint", initial);
    let mut text = String::new();
    for revision in 1..=6000 {
        let prefix = text.len();
        text.push_str("🌍x");
        let patch = json!([{"op":"text","path":["text"],"prefix":prefix,"append":"🌍x"}]);
        journal.extend(line(revision + 1, "patch", patch));
        if revision % 32 == 0 || revision == 6000 {
            write_journal(&state, &journal);
            capture(&state, &run, &grant, false).unwrap();
            while let Some(event) = state.store.first_managed_run_event("job").unwrap() {
                state
                    .store
                    .acknowledge_managed_run_event("job", event.seq)
                    .unwrap();
            }
        }
    }
    let source = state.store.managed_run_event_stream("job").unwrap();
    let projection: Value =
        serde_json::from_str(source.checkpoint_json.as_deref().unwrap()).unwrap();
    assert_eq!(projection["text"], text);
    assert_eq!(source.guest_seq, 6001);
    assert_eq!(source.source_offset, journal.len() as u64);
    assert!(journal.len() < 1024 * 1024);
    assert_eq!(source.pending_bytes, 0);
    println!("managed event synthetic: 6000 Unicode revisions, {} guest bytes, {}ms capture+SQLite+ACK (mock guest RPC)",journal.len(),started.elapsed().as_millis());
}

#[test]
fn changed_guest_boot_omits_old_proof_from_final_receipt() {
    let (state, run, grant) = fixture();
    write_journal(&state, &line(1, "checkpoint", checkpoint()));
    capture(&state, &run, &grant, false).unwrap();
    state
        .backend
        .file_write(
            "vm",
            "/proc/sys/kernel/random/boot_id",
            b"bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        )
        .unwrap();
    prepare_terminal(&state, &run).unwrap();
    state
        .store
        .update_managed_run("job", 1, "interrupted", None, None, None, None, 4, true)
        .unwrap();
    state.store.acknowledge_managed_run_event("job", 1).unwrap();
    let event = state.store.first_managed_run_event("job").unwrap().unwrap();
    let payload: Value = serde_json::from_str(&event.payload_json).unwrap();
    assert_eq!(payload["receipt"]["phase"], "interrupted");
    assert!(payload.get("checkpoint").is_none());
}

#[test]
fn patches_reconstruct_unicode_tools_and_question_lifecycle() {
    let mut value = checkpoint();
    let question = json!({"id":"frm_a","toolId":"tool_a","fields":[{"key":"q0","title":"Choice","type":"string"}]});
    apply_patch(
        &mut value,
        &json!([
            {"op":"text","path":["text"],"prefix":7,"append":"W🌍"},
            {"op":"set","path":["tools","0","status"],"value":"running"},
            {"op":"set","path":["question"],"value":question}
        ]),
    )
    .unwrap();
    assert_eq!(value["text"], "Živjo W🌍");
    assert_eq!(value["tools"][0]["status"], "running");
    apply_patch(
        &mut value,
        &json!([{"op":"remove","path":["question"]},{"op":"set","path":["tools"],"value":[]}]),
    )
    .unwrap();
    assert!(value.get("question").is_none());
    assert_eq!(value["tools"], json!([]));
    for patch in [
        json!([{"op":"text","path":["text"],"prefix":1,"append":"x"}]),
        json!([{"op":"set","path":["bootId"],"value":"other"}]),
        json!([{"op":"set","path":["question","__proto__"],"value":{}}]),
        json!([{"op":"remove","path":["tools","0"]}]),
        json!([{"op":"set","path":["tools","00"],"value":{}}]),
    ] {
        assert!(apply_patch(&mut value.clone(), &patch).is_err());
    }
    let (_, run, grant) = fixture();
    let mut oversized = checkpoint();
    oversized["text"] = "x".repeat(32769).into();
    assert!(validate_checkpoint(&oversized, &run, &grant).is_err());
}

#[test]
fn capture_restarts_from_durable_patch_base_without_replaying_output() {
    let (state, run, grant) = fixture();
    let mut journal = line(1, "checkpoint", checkpoint());
    write_journal(&state, &journal);
    capture(&state, &run, &grant, false).unwrap();
    let first = state.store.first_managed_run_event("job").unwrap().unwrap();
    assert_eq!(
        state
            .store
            .managed_run_event_stream("job")
            .unwrap()
            .guest_seq,
        1
    );
    let claimed = state.store.claim_managed_run("job", 3).unwrap();
    journal.extend(line(2,"patch",json!([{"op":"text","path":["text"],"prefix":7,"append":"again 🌍"},{"op":"set","path":["tools","0","status"],"value":"completed"}])));
    write_journal(&state, &journal);
    capture(&state, &claimed, &grant, false).unwrap();
    assert_eq!(
        state.store.first_managed_run_event("job").unwrap().unwrap(),
        first
    );
    state.store.acknowledge_managed_run_event("job", 1).unwrap();
    let second = state.store.first_managed_run_event("job").unwrap().unwrap();
    let payload: Value = serde_json::from_str(&second.payload_json).unwrap();
    assert_eq!(second.seq, 2);
    assert_eq!(payload["checkpoint"]["text"], "Živjo again 🌍");
    assert_eq!(payload["receipt"]["epoch"], 2);
    assert_eq!(
        state
            .store
            .managed_run_event_stream("job")
            .unwrap()
            .source_offset,
        journal.len() as u64
    );
    capture(&state, &claimed, &grant, false).unwrap();
    assert_eq!(
        state
            .store
            .managed_run_event_stream("job")
            .unwrap()
            .next_event_seq,
        3
    );
}

#[test]
fn terminal_drains_last_patch_before_freezing_receipt_and_detects_partial_tail() {
    let (state, run, grant) = fixture();
    let mut journal = line(1, "checkpoint", checkpoint());
    journal.extend(line(2,"patch",json!([{"op":"set","path":["phase"],"value":"succeeded"},{"op":"text","path":["text"],"prefix":11,"append":" done"}])));
    write_journal(&state, &journal);
    prepare_terminal(&state, &run).unwrap();
    state
        .store
        .update_managed_run("job", 1, "succeeded", None, None, Some(0), None, 4, true)
        .unwrap();
    for seq in [1, 2] {
        state
            .store
            .acknowledge_managed_run_event("job", seq)
            .unwrap();
    }
    let terminal = state.store.first_managed_run_event("job").unwrap().unwrap();
    let payload: Value = serde_json::from_str(&terminal.payload_json).unwrap();
    assert_eq!(terminal.seq, 3);
    assert_eq!(payload["kind"], "receipt");
    assert_eq!(payload["checkpoint"]["phase"], "succeeded");
    assert_eq!(payload["checkpoint"]["text"], "Živjo 🦀 done");
    assert!(!terminal.payload_json.contains(&grant.token));
    let (state, run, grant) = fixture();
    let mut journal = line(1, "checkpoint", checkpoint());
    journal.extend_from_slice(b"{\"schema\":1,\"seq\":2");
    write_journal(&state, &journal);
    capture(&state, &run, &grant, false).unwrap();
    assert!(
        !state
            .store
            .managed_run_event_stream("job")
            .unwrap()
            .source_failed
    );
    prepare_terminal(&state, &run).unwrap();
    assert!(
        state
            .store
            .managed_run_event_stream("job")
            .unwrap()
            .source_failed
    );
    state
        .store
        .update_managed_run("job", 1, "succeeded", None, None, Some(0), None, 4, true)
        .unwrap();
    state.store.acknowledge_managed_run_event("job", 1).unwrap();
    assert!(serde_json::from_str::<Value>(
        &state
            .store
            .first_managed_run_event("job")
            .unwrap()
            .unwrap()
            .payload_json
    )
    .unwrap()
    .get("checkpoint")
    .is_none());
}

#[test]
fn missing_or_corrupt_guest_proof_never_creates_successful_chat_evidence() {
    for corrupt in [None, Some(b"{not-json}\n".as_slice())] {
        let (state, run, _) = fixture();
        if let Some(bytes) = corrupt {
            write_journal(&state, bytes);
        }
        prepare_terminal(&state, &run).unwrap();
        state
            .store
            .update_managed_run("job", 1, "succeeded", None, None, Some(0), None, 4, true)
            .unwrap();
        let terminal = state.store.first_managed_run_event("job").unwrap().unwrap();
        let value: Value = serde_json::from_str(&terminal.payload_json).unwrap();
        assert_eq!(value["receipt"]["phase"], "succeeded");
        assert!(value.get("checkpoint").is_none());
    }
}

#[tokio::test]
async fn node_receipt_api_never_returns_callback_secret() {
    use tower::ServiceExt;
    let (state, run, grant) = fixture();
    let router = crate::build_router(state);
    for (method, suffix, body) in [
        ("GET", "", String::new()),
        ("POST", "", run.request_json.clone()),
        ("POST", "/cancel", "{}".into()),
        ("POST", "/cancel", "{}".into()),
    ] {
        let response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(format!("/v1/admin/runs/{}{suffix}", run.id))
                    .header("Authorization", "Bearer test-event-admin")
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 300000)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains(&grant.token));
        assert!(!String::from_utf8_lossy(&body).contains("event_delivery"));
    }
}

#[tokio::test]
async fn transport_retries_exact_bytes_and_only_accepts_matching_bounded_ack() {
    use axum::{routing::post, Json, Router};
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let count = calls.clone();
    let observations = observed.clone();
    let router = Router::new().route(
        "/events",
        post(move |headers: axum::http::HeaderMap, body: String| {
            let count = count.clone();
            let observations = observations.clone();
            async move {
                assert_eq!(
                    headers.get("Authorization").unwrap(),
                    "Bearer host-only-secret-01234567890123456789"
                );
                observations.lock().unwrap().push(body);
                let attempt = count.fetch_add(1, Ordering::SeqCst);
                Json(json!({"schema":1,"run_id":if attempt==0{"wrong"}else{"job"},"event_seq":1}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut grant = grant();
    grant.url = format!("http://{address}/events");
    let event = ManagedRunEvent {
        run_id: "job".into(),
        seq: 1,
        payload_json: "{\"frozen\":\"🌍\"}".into(),
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    assert!(!deliver(&client, &grant, &event).await);
    assert!(deliver(&client, &grant, &event).await);
    assert_eq!(
        *observed.lock().unwrap(),
        vec![event.payload_json.clone(), event.payload_json.clone()]
    );
    for bad in [
        json!({"schema":1,"run_id":"job","event_seq":2}),
        json!({"schema":1,"run_id":"job","event_seq":1,"extra":true}),
    ] {
        assert!(!accepted_ack(bad.to_string().as_bytes(), &event, false));
    }
    assert!(accepted_ack(
        json!({"schema":1,"run_id":"job","event_seq":1,"retired":true})
            .to_string()
            .as_bytes(),
        &event,
        true
    ));
    assert!(!accepted_ack(
        json!({"schema":1,"run_id":"job","event_seq":1})
            .to_string()
            .as_bytes(),
        &event,
        true
    ));
    server.abort();
}
