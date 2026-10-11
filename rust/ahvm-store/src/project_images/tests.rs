use super::*;

fn fixture() -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .upsert_user(&crate::User {
            id: "admin".into(),
            name: "admin".into(),
            api_key_hash: "unused".into(),
            max_sandboxes: 100,
            max_cpus: 100,
            max_memory_mb: 100000,
            max_volumes_mb: 1,
            max_snapshots: 10,
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    store
}
fn candidate(store: &Store, id: &str, letter: char, ready: bool) -> ProjectEnvironmentImage {
    let sandbox = format!("builder-{id}");
    let builder = ProjectEnvironmentBuilder {
        id: id.into(),
        sandbox_id: sandbox.clone(),
        owner_user_id: "admin".into(),
        request_json: format!("create-{id}"),
        metadata_json:
            serde_json::json!({"tenant_id":"tenant","project_id":"project","build_id":id})
                .to_string(),
        phase: "creating".into(),
        created_at: 1,
    };
    assert!(store.admit_project_builder(&builder).unwrap());
    let volume = letter.to_string().repeat(64);
    store
        .reserve_replicated_volume("admin", &sandbox, &volume, 65536, 1)
        .unwrap();
    let row = ProjectEnvironmentImage {
        id: id.into(),
        owner_user_id: "admin".into(),
        tenant_id: "tenant".into(),
        project_id: "project".into(),
        metadata_json: builder.metadata_json,
        request_json: format!("capture-{id}"),
        source_volume_id: volume,
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
    assert!(store.admit_project_image(&row).unwrap());
    if ready {
        store
            .set_project_image_phase(id, "capturing", None, 2)
            .unwrap();
        store
            .finish_project_image(
                id,
                &letter.to_string().repeat(64),
                &format!("/images/{id}"),
                3,
            )
            .unwrap();
    }
    store.project_image(id).unwrap()
}
fn retain(
    store: &Store,
    generation: i64,
    current: Option<&str>,
    previous: Option<&str>,
    active: &[&str],
    release: &[&str],
) -> Result<()> {
    let active: Vec<_> = active.iter().map(|s| s.to_string()).collect();
    let release: Vec<_> = release.iter().map(|s| s.to_string()).collect();
    let body=serde_json::json!({"generation":generation,"current":current,"previous":previous,"active":active,"release":release}).to_string();
    store.retain_project_images(
        "admin", "tenant", "project", generation, &body, current, previous, &active, &release,
    )
}

#[test]
fn failed_image_remains_charged_until_verified_retirement() {
    let store = fixture();
    let image = candidate(&store, "failed", 'a', false);
    store
        .set_project_image_phase(&image.id, "failed", Some("capture_failed"), 2)
        .unwrap();
    assert_eq!(store.project_image_usage("admin").unwrap(), 131072);
    assert!(store
        .begin_project_image_delete(&image.id, "admin", 3)
        .is_err());
    retain(&store, 1, None, None, &[], &[&image.id]).unwrap();
    store
        .begin_project_image_delete(&image.id, "admin", 4)
        .unwrap();
    assert_eq!(store.project_image_usage("admin").unwrap(), 131072);
    store.finish_project_image_retirement(&image.id, 5).unwrap();
    assert_eq!(store.project_image_usage("admin").unwrap(), 0);
    assert!(!store.admit_project_image(&image).unwrap());
}

#[test]
fn retention_reordering_never_drops_new_project_or_build_pins() {
    let store = fixture();
    candidate(&store, "a", 'a', true);
    candidate(&store, "b", 'b', true);
    candidate(&store, "c", 'c', true);
    retain(&store, 2, Some("b"), Some("a"), &["c"], &["a", "b"]).unwrap();
    candidate(&store, "d", 'd', false);
    retain(&store, 2, Some("b"), Some("a"), &["c"], &["a", "b"]).unwrap();
    assert!(store.begin_project_image_delete("d", "admin", 3).is_err());
    retain(&store, 3, Some("c"), Some("b"), &["d"], &["c"]).unwrap();
    assert!(retain(&store, 2, Some("b"), Some("a"), &[], &["c"]).is_err());
    assert!(retain(&store, 3, Some("b"), Some("a"), &[], &["c"]).is_err());
    assert!(store.begin_project_image_delete("c", "admin", 4).is_err());
    assert!(store.begin_project_image_delete("b", "admin", 4).is_err());
    assert!(store.begin_project_image_delete("a", "admin", 4).is_ok());
}

#[test]
fn tenant_project_generation_and_vm_pins_are_atomic() {
    let store = fixture();
    candidate(&store, "a", 'a', true);
    let sha = "a".repeat(64);
    for (tenant, project, digest) in [
        ("other", "project", sha.as_str()),
        ("tenant", "other", sha.as_str()),
        ("tenant", "project", "wrong"),
    ] {
        assert!(store
            .pin_project_image_vm("a", "admin", tenant, project, digest, "chat")
            .is_err());
    }
    store
        .pin_project_image_vm("a", "admin", "tenant", "project", &sha, "chat")
        .unwrap();
    store
        .pin_project_image_vm("a", "admin", "tenant", "project", &sha, "chat")
        .unwrap();
    retain(&store, 1, None, None, &[], &["a"]).unwrap();
    assert!(store.begin_project_image_delete("a", "admin", 2).is_err());
    store.release_project_image_vm("chat").unwrap();
    assert!(store.begin_project_image_delete("a", "admin", 3).is_ok());
}

#[test]
fn candidate_images_and_disks_compete_for_one_logical_quota() {
    let store = fixture();
    let row = candidate(&store, "a", 'a', true);
    store
        .reserve_replicated_volume("admin", "other", &"e".repeat(64), 13 * 65536, 1)
        .unwrap();
    assert!(store
        .reserve_replicated_volume("admin", "too-large", &"f".repeat(64), 65536, 1)
        .is_err());
    retain(&store, 1, None, None, &[], &["a"]).unwrap();
    store
        .begin_project_image_delete(&row.id, "admin", 2)
        .unwrap();
    assert!(store
        .reserve_replicated_volume("admin", "too-large", &"f".repeat(64), 65536, 1)
        .is_err());
    store.finish_project_image_retirement(&row.id, 3).unwrap();
    assert!(store
        .reserve_replicated_volume("admin", "fits", &"f".repeat(64), 65536, 1)
        .is_ok());
}

#[test]
fn arbitrary_existing_vm_cannot_become_builder_and_capture_identity_cannot_change() {
    let store = fixture();
    store
        .create_sandbox(&crate::Sandbox {
            id: "personal".into(),
            owner_user_id: "admin".into(),
            name: "personal".into(),
            backend: crate::Backend::Krucible,
            state: "stopped".into(),
            thermal: "cold".into(),
            cpus: 1,
            memory_mb: 1024,
            ip: "".into(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    let builder = ProjectEnvironmentBuilder {
        id: "new".into(),
        sandbox_id: "personal".into(),
        owner_user_id: "admin".into(),
        request_json: "create".into(),
        metadata_json: "meta".into(),
        phase: "creating".into(),
        created_at: 1,
    };
    assert!(store.admit_project_builder(&builder).is_err());
    let mut row = candidate(&store, "a", 'a', true);
    row.request_json = "changed".into();
    assert!(store.admit_project_image(&row).is_err());
    assert_eq!(store.project_image("a").unwrap().phase, "ready");
}

#[test]
fn delayed_capture_cannot_reopen_ready_or_deleting_image() {
    let store = fixture();
    candidate(&store, "a", 'a', true);
    assert!(store
        .set_project_image_phase("a", "capturing", None, 4)
        .is_err());
    assert!(store
        .set_project_image_phase("a", "failed", Some("late_failure"), 4)
        .is_err());
    retain(&store, 1, None, None, &[], &["a"]).unwrap();
    store.begin_project_image_delete("a", "admin", 5).unwrap();
    assert!(store
        .set_project_image_phase("a", "capturing", None, 6)
        .is_err());
    assert!(store
        .set_project_image_phase("a", "failed", Some("late_failure"), 6)
        .is_err());
    assert!(store
        .finish_project_image("a", &"a".repeat(64), "/images/a", 6)
        .is_err());
    assert_eq!(store.project_image("a").unwrap().phase, "deleting");
}

#[test]
fn capture_admission_cannot_change_builder_project_under_shared_daemon_owner() {
    let store = fixture();
    let mut row = candidate(&store, "a", 'a', true);
    row.tenant_id = "another-tenant".into();
    assert!(store.admit_project_image(&row).is_err());
    row.tenant_id = "tenant".into();
    row.project_id = "another-project".into();
    assert!(store.admit_project_image(&row).is_err());
    assert_eq!(store.project_image("a").unwrap().project_id, "project");
}

#[test]
fn pending_create_pin_survives_absent_vm_rows_and_restart_reconciliation() {
    let store = fixture();
    candidate(&store, "a", 'a', true);
    store
        .admit_lifecycle_operation(
            &crate::LifecycleOperation {
                id: "create-chat".into(),
                owner_user_id: "admin".into(),
                sandbox_id: "chat".into(),
                request: "{\"action\":\"create\"}".into(),
                state: "pending".into(),
                status: None,
                error: None,
                sandbox_state: None,
                storage_volume_id: None,
            },
            1,
        )
        .unwrap();
    store
        .pin_project_image_vm("a", "admin", "tenant", "project", &"a".repeat(64), "chat")
        .unwrap();
    retain(&store, 1, None, None, &[], &["a"]).unwrap();
    assert!(store.release_project_image_vm("chat").is_err());
    assert!(store.begin_project_image_delete("a", "admin", 2).is_err());
    store.interrupt_lifecycle_operations().unwrap();
    // The caller additionally proves backend absence; only a terminal operation
    // and absent VM/disk rows allow it to release this interrupted create pin.
    store.release_project_image_vm("chat").unwrap();
    assert!(store.begin_project_image_delete("a", "admin", 3).is_ok());
}
