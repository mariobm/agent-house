use super::*;
use ahvm_engine::{Backend, BackendKind, MockBackend, SandboxSpec};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

fn http_fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../../docs/fixtures/project-images-v1.json"
    ))
    .unwrap()
}
fn state() -> (AppState, Arc<MockBackend>, std::path::PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "ahvm-project-node-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(ahvm_store::Store::open_in_memory().unwrap());
    crate::auth::ensure_admin(&store, "project-node-test").unwrap();
    let backend = Arc::new(MockBackend::new(&root));
    backend.enable_project_images(ahvm_engine::ProjectImageCapacity {
        physical_available_bytes: 128 * 1024 * 1024 * 1024,
        physical_reserved_bytes: 0,
    });
    backend.set_project_image_base_content("e".repeat(64));
    (
        AppState {
            private_owners: Default::default(),
            store,
            backend: backend.clone(),
            quotas: crate::quotas::Registry::new(),
            activity: crate::thermal::ActivityTracker::new(),
            ops: crate::scheduler::OpsLimiter::new(4),
            lifecycle: crate::scheduler::LifecycleLocks::new(),
        },
        backend,
        root,
    )
}
fn candidate() -> (
    AppState,
    Arc<MockBackend>,
    ProjectEnvironmentImage,
    CaptureRequest,
    ProjectImageInfo,
) {
    let (state, backend, root) = state();
    let fixture = http_fixture();
    let metadata: ProjectImageMetadata =
        serde_json::from_value(fixture["builderRequest"]["metadata"].clone()).unwrap();
    let body: BuilderRequest = serde_json::from_value(fixture["builderRequest"].clone()).unwrap();
    let sandbox = builder_name(&metadata.build_id);
    state
        .store
        .admit_project_builder(&ProjectEnvironmentBuilder {
            id: metadata.build_id.clone(),
            sandbox_id: sandbox.clone(),
            owner_user_id: "admin".into(),
            request_json: serde_json::to_string(&body).unwrap(),
            metadata_json: String::from_utf8(metadata.canonical_bytes().unwrap()).unwrap(),
            phase: "creating".into(),
            created_at: 1,
        })
        .unwrap();
    let base = root.join(format!("{}.ext4", metadata.base_image_digest));
    std::fs::File::create(&base)
        .unwrap()
        .set_len(65536)
        .unwrap();
    let spec = SandboxSpec {
        name: sandbox.clone(),
        storage_mode: Some(StorageMode::Replicated),
        desktop: false,
        desktop_gpu: false,
        cpus: 1,
        memory_mb: 1024,
        backend: BackendKind::Krucible,
        root_image: Some(base.to_string_lossy().into_owned()),
        kernel_image: None,
        extra_env: Default::default(),
        network_bytes_per_sec: None,
    };
    let mut volume = String::new();
    let info = backend
        .create_with_storage_admission(&spec, &mut |id, bytes| {
            volume = id.to_owned();
            state
                .store
                .reserve_replicated_volume("admin", &sandbox, id, bytes as i64, 1)
                .unwrap();
            Ok(())
        })
        .unwrap();
    state
        .store
        .create_sandbox(&ahvm_store::Sandbox {
            id: sandbox.clone(),
            owner_user_id: "admin".into(),
            name: sandbox.clone(),
            backend: ahvm_store::Backend::Krucible,
            state: "running".into(),
            thermal: "hot".into(),
            cpus: 1,
            memory_mb: 1024,
            ip: info.ip,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    let capture: CaptureRequest =
        serde_json::from_value(fixture["captureRequest"].clone()).unwrap();
    let row = ProjectEnvironmentImage {
        id: metadata.build_id.clone(),
        owner_user_id: "admin".into(),
        tenant_id: metadata.tenant_id.clone(),
        project_id: metadata.project_id.clone(),
        metadata_json: String::from_utf8(metadata.canonical_bytes().unwrap()).unwrap(),
        request_json: serde_json::to_string(&capture).unwrap(),
        source_volume_id: volume.clone(),
        source_sandbox_id: sandbox,
        source_logical_bytes: 65536,
        logical_bytes: 131072,
        phase: "preparing".into(),
        digest: None,
        image_path: None,
        detail: None,
        created_at: 1,
        updated_at: 1,
    };
    state.store.admit_project_image(&row).unwrap();
    let image = ProjectImageInfo {
        image_id: row.id.clone(),
        digest: "d".repeat(64),
        image_path: root.join(format!("{}.ext4", "d".repeat(64))),
        logical_bytes: 131072,
        source_volume_id: volume,
        metadata,
    };
    std::fs::File::create(&image.image_path)
        .unwrap()
        .set_len(image.logical_bytes)
        .unwrap();
    (state, backend, row, capture, image)
}

#[test]
fn shared_http_fixture_matches_image_view_and_request_codecs() {
    let fixture = http_fixture();
    let _: BuilderRequest = serde_json::from_value(fixture["builderRequest"].clone()).unwrap();
    let _: CaptureRequest = serde_json::from_value(fixture["captureRequest"].clone()).unwrap();
    let _: RetentionRequest = serde_json::from_value(fixture["retentionRequest"].clone()).unwrap();
    let _: operations::Request = serde_json::from_value(fixture["createRequest"].clone()).unwrap();
    let value = &fixture["readyImage"];
    let row = ProjectEnvironmentImage {
        id: value["image_id"].as_str().unwrap().into(),
        owner_user_id: "admin".into(),
        tenant_id: value["metadata"]["tenant_id"].as_str().unwrap().into(),
        project_id: value["metadata"]["project_id"].as_str().unwrap().into(),
        metadata_json: value["metadata"].to_string(),
        request_json: "private".into(),
        source_volume_id: value["source_volume_id"].as_str().unwrap().into(),
        source_sandbox_id: value["source_sandbox_id"].as_str().unwrap().into(),
        source_logical_bytes: value["capture_source_logical_bytes"].as_i64().unwrap(),
        logical_bytes: value["logical_bytes"].as_i64().unwrap(),
        phase: "ready".into(),
        digest: Some(value["digest"].as_str().unwrap().into()),
        image_path: Some("/private/host-path".into()),
        detail: None,
        created_at: value["created_at"].as_i64().unwrap(),
        updated_at: value["updated_at"].as_i64().unwrap(),
    };
    assert_eq!(image_view(&row).unwrap(), *value);
    assert!(image_view(&row).unwrap().get("image_path").is_none());
}
#[test]
fn first_absent_capture_runs_once_and_lost_reply_reconciles_ready_result() {
    let (state, backend, row, _body, info) = candidate();
    state
        .store
        .set_project_image_phase(&row.id, "capturing", None, 2)
        .unwrap();
    backend.plan_project_image_capture(info, true);
    assert!(capture_step(&state, &row.id).is_err());
    assert_eq!(
        state.store.project_image(&row.id).unwrap().phase,
        "capturing"
    );
    assert!(capture_step(&state, &row.id).unwrap());
    assert_eq!(state.store.project_image(&row.id).unwrap().phase, "ready");
    assert_eq!(backend.project_test_counts().0, 1);
}
#[test]
fn sealed_restart_finishes_stop_sync_without_guest_exec_or_hook_replay() {
    let (state, backend, row, body, _info) = candidate();
    state
        .store
        .set_project_builder_phase(&row.id, "sealed")
        .unwrap();
    backend.fail_sync_calls(1);
    assert!(prepare_capture(&state, &row.id, &body).is_err());
    assert_eq!(
        backend.status(&row.source_sandbox_id).unwrap().state,
        VmState::Stopped
    );
    assert_eq!(
        state.store.project_image(&row.id).unwrap().phase,
        "preparing"
    );
    prepare_capture(&state, &row.id, &body).unwrap();
    assert_eq!(
        state.store.project_image(&row.id).unwrap().phase,
        "capturing"
    );
    assert_eq!(backend.project_test_counts().1, 0);
}
#[test]
fn quiescent_failed_intent_is_terminal_but_keeps_charge_and_pin() {
    let (state, backend, row, _body, _info) = candidate();
    state
        .store
        .set_project_image_phase(&row.id, "capturing", None, 2)
        .unwrap();
    backend.fail_project_image(&row.id, "capture_source_rejected");
    assert!(capture_step(&state, &row.id).unwrap());
    let failed = state.store.project_image(&row.id).unwrap();
    assert_eq!(failed.phase, "failed");
    assert_eq!(failed.detail.as_deref(), Some("capture_source_rejected"));
    assert_eq!(state.store.project_image_usage("admin").unwrap(), 131072);
    assert!(state
        .store
        .begin_project_image_delete(&row.id, "admin", 3)
        .is_err());
    assert_eq!(backend.project_test_counts().0, 0);
}
#[tokio::test]
async fn exact_builder_replay_admits_missing_lifecycle_receipt() {
    let (state, _backend, _root) = state();
    let body: BuilderRequest =
        serde_json::from_value(http_fixture()["builderRequest"].clone()).unwrap();
    let id = body.metadata.build_id.clone();
    let row = ProjectEnvironmentBuilder {
        id: id.clone(),
        sandbox_id: builder_name(&id),
        owner_user_id: "admin".into(),
        request_json: serde_json::to_string(&body).unwrap(),
        metadata_json: String::from_utf8(body.metadata.canonical_bytes().unwrap()).unwrap(),
        phase: "creating".into(),
        created_at: 1,
    };
    state.store.admit_project_builder(&row).unwrap();
    assert!(state
        .store
        .lifecycle_operation(&builder_operation(&id), "admin")
        .is_err());
    let _ = create_builder(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(id.clone()),
        Json(body),
    )
    .await
    .unwrap();
    assert!(state
        .store
        .lifecycle_operation(&builder_operation(&id), "admin")
        .is_ok());
}
#[tokio::test]
async fn sealed_builder_rejects_start_new_run_and_guest_mutation() {
    let (state, _backend, row, _body, _info) = candidate();
    state
        .store
        .set_project_builder_phase(&row.id, "sealed")
        .unwrap();
    assert!(crate::sandboxes::start_operation(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(row.source_sandbox_id.clone()),
        None,
        None
    )
    .await
    .is_err());
    assert!(
        crate::routes::guest(&state, "admin", &row.source_sandbox_id)
            .await
            .is_err()
    );
    let request = crate::runs::RunRequest {
        sandbox_id: row.source_sandbox_id.clone(),
        argv: vec!["true".into()],
        max_runtime_secs: 10,
        fence_on_failure: false,
        session_isolated: true,
        event_delivery: None,
    };
    assert!(crate::runs::submit(
        State(state),
        Extension(UserId("admin".into())),
        Path("new-job".into()),
        Json(request)
    )
    .await
    .is_err());
}

#[tokio::test]
async fn failed_preflight_before_volume_intent_releases_only_derived_image_charge() {
    let (state, backend, row, body, _info) = candidate();
    assert!(prepare_capture(&state, &row.id, &body).is_err());
    assert_eq!(backend.project_test_counts().0, 0);
    state
        .store
        .set_project_image_phase(&row.id, "failed", Some("capture_preflight_failed"), 2)
        .unwrap();
    let _ = retain(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path((row.tenant_id.clone(), row.project_id.clone())),
        Json(RetentionRequest {
            generation: 1,
            current_image_id: None,
            previous_image_id: None,
            active_build_ids: vec![],
            release_build_ids: vec![row.id.clone()],
        }),
    )
    .await
    .unwrap();
    let (status, Json(value)) = delete(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(row.id.clone()),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["phase"], "retired");
    assert_eq!(state.store.project_image_usage("admin").unwrap(), 0);
    assert!(state.store.get_sandbox(&row.source_sandbox_id).is_ok());
    assert_eq!(
        state.store.replicated_usage("admin").unwrap().logical_bytes,
        65536
    );
}

#[tokio::test]
async fn unadmitted_delete_fences_delayed_capture_without_charge_or_source_mutation() {
    let (state, backend, _) = state();
    let fixture = http_fixture();
    let body: BuilderRequest = serde_json::from_value(fixture["builderRequest"].clone()).unwrap();
    let id = body.metadata.build_id.clone();
    state
        .store
        .admit_project_builder(&ProjectEnvironmentBuilder {
            id: id.clone(),
            sandbox_id: builder_name(&id),
            owner_user_id: "admin".into(),
            request_json: serde_json::to_string(&body).unwrap(),
            metadata_json: String::from_utf8(body.metadata.canonical_bytes().unwrap()).unwrap(),
            phase: "creating".into(),
            created_at: 1,
        })
        .unwrap();
    let guard = state.lifecycle.lock(&format!("project-image:{id}")).await;
    let delayed = tokio::spawn({
        let state = state.clone();
        let id = id.clone();
        async move {
            capture(
                State(state),
                Extension(UserId("admin".into())),
                Path(id),
                Json(serde_json::from_value(http_fixture()["captureRequest"].clone()).unwrap()),
            )
            .await
        }
    });
    let tombstone = state
        .store
        .retire_unadmitted_project_image(&id, "admin", 1)
        .unwrap();
    assert_eq!(tombstone.logical_bytes, 0);
    drop(guard);
    assert!(delayed.await.unwrap().is_err());
    let (status, Json(receipt)) = delete(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(id.clone()),
    )
    .await
    .unwrap();
    assert_eq!(status, StatusCode::OK);
    let expected = &fixture["retiredUnadmittedImage"];
    for field in [
        "phase",
        "metadata",
        "source_volume_id",
        "capture_source_logical_bytes",
        "logical_bytes",
        "digest",
        "error_code",
        "logical_released",
        "reclamation_complete",
    ] {
        assert_eq!(receipt[field], expected[field], "{field}");
    }
    assert_eq!(state.store.project_image_usage("admin").unwrap(), 0);
    assert_eq!(backend.project_test_counts().0, 0);
}

#[tokio::test]
async fn delete_before_first_capture_records_retired_intent() {
    let (state, _backend, _) = state();
    let body: BuilderRequest =
        serde_json::from_value(http_fixture()["builderRequest"].clone()).unwrap();
    let id = body.metadata.build_id.clone();
    state
        .store
        .admit_project_builder(&ProjectEnvironmentBuilder {
            id: id.clone(),
            sandbox_id: builder_name(&id),
            owner_user_id: "admin".into(),
            request_json: serde_json::to_string(&body).unwrap(),
            metadata_json: String::from_utf8(body.metadata.canonical_bytes().unwrap()).unwrap(),
            phase: "creating".into(),
            created_at: 1,
        })
        .unwrap();
    assert!(state.store.project_image(&id).is_err());
    let (_, Json(receipt)) = delete(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(id.clone()),
    )
    .await
    .unwrap();
    assert_eq!(receipt["phase"], "retired");
    assert!(state.store.project_image(&id).is_ok());
    assert_eq!(state.store.project_image_usage("admin").unwrap(), 0);
}

#[test]
fn shared_admission_errors_have_fixed_capacity_codes() {
    for entry in http_fixture()["admissionErrors"].as_array().unwrap() {
        let message = entry["message"].as_str().unwrap().to_owned();
        let error = if entry["status"] == 422 {
            ApiError::Invalid(message)
        } else {
            ApiError::Conflict(message)
        };
        assert_eq!(error.code(), entry["code"].as_str().unwrap());
        assert_eq!(
            error.status().as_u16(),
            entry["status"].as_u64().unwrap() as u16
        );
    }
}

#[tokio::test]
async fn raw_change_between_profile_and_post_is_terminal_after_catalog_repair() {
    let (state, backend, _) = state();
    let fixture = http_fixture();
    let body: BuilderRequest = serde_json::from_value(fixture["builderRequest"].clone()).unwrap();
    assert_ne!(
        body.metadata.base_image_digest,
        body.metadata.base_content_digest
    );
    let id = body.metadata.build_id.clone();
    // Frozen profile/raw provenance was e; operator replacement derives f.
    backend.set_project_image_base_content("f".repeat(64));
    let rejected = create_builder(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(id.clone()),
        Json(body.clone()),
    )
    .await
    .unwrap_err();
    assert_eq!(rejected.code(), "project_image_base_invalid");
    let Json(value) = get_builder(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(id.clone()),
    )
    .await
    .unwrap();
    for key in [
        "phase",
        "metadata",
        "lifecycle_operation",
        "capture_source_logical_bytes",
        "candidate_logical_bytes",
    ] {
        assert_eq!(value[key], fixture["invalidBuilder"][key], "{key}");
    }
    assert_eq!(
        state.store.replicated_usage("admin").unwrap().logical_bytes,
        0
    );
    backend.set_project_image_base_content(body.metadata.base_content_digest.clone());
    assert_eq!(
        create_builder(
            State(state.clone()),
            Extension(UserId("admin".into())),
            Path(id),
            Json(body)
        )
        .await
        .unwrap_err()
        .code(),
        "project_image_base_invalid"
    );
}

#[tokio::test]
async fn verifying_delete_fences_late_post_and_actual_create_after_verification() {
    let (state, backend, _) = state();
    let fixture = http_fixture();
    let body: BuilderRequest = serde_json::from_value(fixture["builderRequest"].clone()).unwrap();
    let id = body.metadata.build_id.clone();
    backend.set_project_image_base_status(ProjectImageBaseStatus::Verifying);
    assert_eq!(
        create_builder(
            State(state.clone()),
            Extension(UserId("admin".into())),
            Path(id.clone()),
            Json(body.clone())
        )
        .await
        .unwrap_err()
        .code(),
        "project_image_base_verifying"
    );
    let Json(value) = get_builder(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(id.clone()),
    )
    .await
    .unwrap();
    for key in [
        "phase",
        "metadata",
        "lifecycle_operation",
        "capture_source_logical_bytes",
        "candidate_logical_bytes",
    ] {
        assert_eq!(value[key], fixture["verifyingBuilder"][key], "{key}");
    }
    let _ = operations::submit(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path("cancel-unverified-builder".into()),
        Json(operations::Request::Delete {
            sandbox_id: builder_name(&id),
        }),
    )
    .await
    .unwrap();
    assert_eq!(state.store.project_builder(&id).unwrap().phase, "retired");
    backend.set_project_image_base_status(ProjectImageBaseStatus::Verified);
    let (_, Json(value)) = create_builder(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path(id.clone()),
        Json(body.clone()),
    )
    .await
    .unwrap();
    for key in [
        "phase",
        "metadata",
        "lifecycle_operation",
        "capture_source_logical_bytes",
        "candidate_logical_bytes",
    ] {
        assert_eq!(value[key], fixture["retiredBuilder"][key], "{key}");
    }
    let rejected = crate::sandboxes::create_operation(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Json(crate::sandboxes::CreateBody {
            name: builder_name(&id),
            storage_mode: Some(StorageMode::Replicated),
            cpus: 1,
            memory_mb: 1024,
            desktop: false,
            image: Some("ubuntu-dev".into()),
        }),
        Some(&builder_operation(&id)),
        None,
        Some(&body.metadata.base_image_digest),
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(rejected, ApiError::Conflict(_)));
    assert_eq!(
        state.store.replicated_usage("admin").unwrap().logical_bytes,
        0
    );
}
