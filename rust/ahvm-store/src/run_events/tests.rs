use super::*;
use crate::{Backend, Sandbox, User};

fn setup(store: &Store) {
    store
        .upsert_user(&User {
            id: "u".into(),
            name: "u".into(),
            api_key_hash: "hash".into(),
            max_sandboxes: 5,
            max_cpus: 4,
            max_memory_mb: 4096,
            max_volumes_mb: 20480,
            max_snapshots: 5,
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
    store
        .create_sandbox(&Sandbox {
            id: "vm".into(),
            owner_user_id: "u".into(),
            name: "vm".into(),
            backend: Backend::Krucible,
            state: "running".into(),
            thermal: "hot".into(),
            cpus: 1,
            memory_mb: 2048,
            ip: "".into(),
            created_at: 1,
            updated_at: 1,
        })
        .unwrap();
}
fn request() -> String {
    json!({"sandbox_id":"vm","argv":["controller"],"session_isolated":true,"max_runtime_secs":3600,
        "event_delivery":{"url":"https://cloud.example/internal/agent-events/job","token":"secret-host-only-01234567890123456789","request_hash":"a".repeat(64)}}).to_string()
}
fn admit(store: &Store, id: &str) -> ManagedRun {
    store
        .admit_managed_run(id, "vm", "u", &request(), 1, 3601)
        .unwrap()
        .0
}

#[test]
fn cursor_capture_ack_and_terminal_are_atomic() {
    let store = Store::open_in_memory().unwrap();
    setup(&store);
    let run = admit(&store, "job");
    let checkpoint = json!({"phase":"running","text":"one"}).to_string();
    store
        .append_managed_run_checkpoint("job", run.epoch, 100, 1, &checkpoint, 2)
        .unwrap();
    let first = store.first_managed_run_event("job").unwrap().unwrap();
    assert!(!first.payload_json.contains("secret-host-only"));
    assert!(!format!("{run:?}").contains("secret-host-only"));
    assert!(!public_managed_run(run.clone())
        .request_json
        .contains("event_delivery"));
    assert!(store
        .append_managed_run_checkpoint("job", run.epoch, 200, 3, &checkpoint, 3)
        .is_err());
    assert!(store.acknowledge_managed_run_event("job", 2).is_err());
    assert_eq!(
        store.first_managed_run_event("job").unwrap(),
        Some(first.clone())
    );
    store.with_conn(|conn|{conn.execute_batch("CREATE TRIGGER reject_terminal BEFORE INSERT ON managed_run_events WHEN json_extract(NEW.payload_json,'$.kind')='receipt' BEGIN SELECT RAISE(ABORT,'test write failure'); END;")?;Ok(())}).unwrap();
    assert!(store
        .update_managed_run("job", 1, "succeeded", None, None, Some(0), None, 4, true)
        .is_err());
    assert_eq!(store.get_managed_run("job").unwrap().finished_at, None);
    assert!(!store.managed_run_event_stream("job").unwrap().sealed);
    store
        .with_conn(|conn| {
            conn.execute_batch("DROP TRIGGER reject_terminal")?;
            Ok(())
        })
        .unwrap();
    store
        .update_managed_run("job", 1, "succeeded", None, None, Some(0), None, 4, true)
        .unwrap();
    store.acknowledge_managed_run_event("job", 1).unwrap();
    store.acknowledge_managed_run_event("job", 1).unwrap(); // Lost ACK retry.
    let terminal = store.first_managed_run_event("job").unwrap().unwrap();
    let event: Value = serde_json::from_str(&terminal.payload_json).unwrap();
    assert_eq!(terminal.seq, 2);
    assert_eq!(event["kind"], "receipt");
    assert_eq!(event["receipt"]["finished_at"], 4);
    assert_eq!(event["checkpoint"]["text"], "one");
    store.acknowledge_managed_run_event("job", 2).unwrap();
    assert!(store.first_managed_run_event("job").unwrap().is_none());
    let cursor = store.managed_run_event_stream("job").unwrap();
    assert_eq!(cursor.acked_seq, 2);
    assert_eq!(cursor.pending_bytes, 0);
    assert_eq!(store.managed_run_event_delivery("job").unwrap().0, "{}");
}

#[test]
fn crash_replay_preserves_exact_bytes_and_guest_cursor() {
    let directory = std::env::temp_dir().join(format!(
        "ahvm-events-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("state.db");
    let store = Store::open(&path).unwrap();
    setup(&store);
    admit(&store, "job");
    store
        .append_managed_run_checkpoint(
            "job",
            1,
            143,
            1,
            &json!({"text":"Živjo 🦀","phase":"running"}).to_string(),
            2,
        )
        .unwrap();
    let frozen = store.first_managed_run_event("job").unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.first_managed_run_event("job").unwrap(), frozen);
    let cursor = store.managed_run_event_stream("job").unwrap();
    assert_eq!(
        (cursor.source_offset, cursor.guest_seq, cursor.acked_seq),
        (143, 1, 0)
    );
    let claimed = store.claim_managed_run("job", 3).unwrap();
    assert_eq!(claimed.epoch, 2);
    assert!(store
        .append_managed_run_checkpoint("job", 1, 200, 2, "{}", 4)
        .is_err());
    store.acknowledge_managed_run_event("job", 1).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert!(store.first_managed_run_event("job").unwrap().is_none());
    store
        .append_managed_run_checkpoint("job", 2, 200, 2, "{}", 4)
        .unwrap();
    assert_eq!(
        store.first_managed_run_event("job").unwrap().unwrap().seq,
        2
    );
    drop(store);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn backpressure_keeps_cursor_and_reserves_terminal_storage() {
    let store = Store::open_in_memory().unwrap();
    setup(&store);
    admit(&store, "job");
    let snapshot = json!({"text":"x".repeat(250000),"phase":"succeeded"}).to_string();
    let mut seq = 1;
    while store
        .append_managed_run_checkpoint("job", 1, seq * 300000, seq, &snapshot, 2)
        .is_ok()
    {
        seq += 1;
    }
    assert!(seq > 20);
    let source = store.managed_run_event_stream("job").unwrap();
    assert_eq!(source.guest_seq, seq - 1);
    assert_eq!(source.source_offset, (seq - 1) * 300000);
    store
        .update_managed_run("job", 1, "succeeded", None, None, Some(0), None, 3, true)
        .unwrap();
    let terminal_seq = store
        .managed_run_event_stream("job")
        .unwrap()
        .next_event_seq
        - 1;
    for ack in 1..terminal_seq {
        store.acknowledge_managed_run_event("job", ack).unwrap();
    }
    let terminal = store.first_managed_run_event("job").unwrap().unwrap();
    assert_eq!(terminal.seq, terminal_seq);
    assert_eq!(
        serde_json::from_str::<Value>(&terminal.payload_json).unwrap()["kind"],
        "receipt"
    );
}

#[test]
fn vm_deletion_keeps_terminal_delivery_and_retained_run_identity() {
    let store = Store::open_in_memory().unwrap();
    setup(&store);
    admit(&store, "job");
    store
        .update_managed_run("job", 1, "interrupted", None, None, None, None, 2, true)
        .unwrap();
    let event = store.first_managed_run_event("job").unwrap();
    store.delete_sandbox("vm").unwrap();
    assert!(store.get_managed_run("job").is_err());
    assert_eq!(store.first_managed_run_event("job").unwrap(), event);
    assert!(store
        .managed_run_event_delivery("job")
        .unwrap()
        .0
        .contains("secret-host-only"));
    setup(&store);
    assert!(matches!(
        store.admit_managed_run("job", "vm", "u", "{}", 3, 100),
        Err(Error::Conflict(_))
    ));
    store.acknowledge_managed_run_event("job", 1).unwrap();
    assert!(store.first_managed_run_event("job").unwrap().is_none());
    assert!(store.managed_run_event_stream("job").is_err());
}

#[test]
fn admission_reserves_every_final_event_or_refuses_without_a_receipt() {
    let store = Store::open_in_memory().unwrap();
    setup(&store);
    let mut admitted = 0;
    while store
        .admit_managed_run(&format!("job{admitted}"), "vm", "u", &request(), 1, 100)
        .is_ok()
    {
        admitted += 1;
    }
    assert_eq!(admitted, MAX_TOTAL_PENDING / MAX_MANAGED_EVENT_BYTES as i64);
    assert!(store.get_managed_run(&format!("job{admitted}")).is_err());
    assert_eq!(
        store.list_active_managed_runs().unwrap().len(),
        admitted as usize
    );
}

#[test]
fn deleted_vm_churn_is_bounded_by_pending_stream_count_and_ack_restores_admission() {
    let store = Store::open_in_memory().unwrap();
    setup(&store);
    for index in 0..MAX_PENDING_STREAMS {
        let id = format!("churn{index}");
        admit(&store, &id);
        if index == 0 {
            store
                .append_managed_run_checkpoint(&id, 1, 100, 1, "{}", 2)
                .unwrap();
        }
        store
            .update_managed_run(&id, 1, "interrupted", None, None, None, None, 2, true)
            .unwrap();
        store.delete_sandbox("vm").unwrap();
        setup(&store);
    }
    assert!(store.list_active_managed_runs().unwrap().is_empty());
    assert_eq!(
        store.list_managed_run_event_streams().unwrap().len(),
        MAX_PENDING_STREAMS as usize
    );
    let payloads = store.with_conn(|conn| Ok(totals(conn)?.0)).unwrap();
    assert!(payloads < MAX_TOTAL_PENDING / 8); // Byte capacity alone permits much more churn.
    let retained = store.first_managed_run_event("churn0").unwrap().unwrap();
    assert!(matches!(
        store.admit_managed_run("blocked", "vm", "u", &request(), 3, 100),
        Err(Error::Conflict(_))
    ));
    assert!(store.get_managed_run("blocked").is_err());
    assert_eq!(
        store.first_managed_run_event("churn0").unwrap(),
        Some(retained)
    );
    store.acknowledge_managed_run_event("churn0", 1).unwrap();
    assert!(matches!(
        store.admit_managed_run("still-blocked", "vm", "u", &request(), 3, 100),
        Err(Error::Conflict(_))
    ));
    assert!(store.managed_run_event_stream("churn0").is_ok());
    store.acknowledge_managed_run_event("churn0", 2).unwrap();
    assert!(store.managed_run_event_stream("churn0").is_err());
    assert!(store
        .admit_managed_run("after-ack", "vm", "u", &request(), 3, 100)
        .is_ok());
    assert_eq!(
        store.list_managed_run_event_streams().unwrap().len(),
        MAX_PENDING_STREAMS as usize
    );
}

#[test]
fn final_ack_and_vm_delete_or_crash_cleanup_remove_only_retired_complete_streams() {
    let store = Store::open_in_memory().unwrap();
    setup(&store);
    admit(&store, "acked-before-delete");
    store
        .update_managed_run(
            "acked-before-delete",
            1,
            "interrupted",
            None,
            None,
            None,
            None,
            2,
            true,
        )
        .unwrap();
    store
        .acknowledge_managed_run_event("acked-before-delete", 1)
        .unwrap();
    assert!(store
        .managed_run_event_stream("acked-before-delete")
        .is_ok());
    assert_eq!(
        store
            .managed_run_event_delivery_state("acked-before-delete")
            .unwrap(),
        (false, false)
    );
    store.delete_sandbox("vm").unwrap();
    assert!(store
        .managed_run_event_stream("acked-before-delete")
        .is_err());
    setup(&store);
    admit(&store, "ack-orphan");
    admit(&store, "pending-orphan");
    for id in ["ack-orphan", "pending-orphan"] {
        store
            .update_managed_run(id, 1, "interrupted", None, None, None, None, 2, true)
            .unwrap();
    }
    store
        .acknowledge_managed_run_event("ack-orphan", 1)
        .unwrap();
    let pending = store.first_managed_run_event("pending-orphan").unwrap();
    // Simulate a process crash after the VM/receipt delete committed, before
    // the opportunistic fully-ACKed orphan cleanup statement ran.
    store
        .with_conn(|conn| {
            conn.execute("DELETE FROM sandboxes WHERE id='vm'", [])?;
            Ok(())
        })
        .unwrap();
    store.cleanup_managed_run_event_streams().unwrap();
    assert!(store.managed_run_event_stream("ack-orphan").is_err());
    assert_eq!(
        store.first_managed_run_event("pending-orphan").unwrap(),
        pending
    );
    assert_eq!(
        store
            .managed_run_event_delivery_state("pending-orphan")
            .unwrap(),
        (true, true)
    );
    setup(&store);
    assert!(store
        .admit_managed_run("new", "vm", "u", &request(), 3, 100)
        .is_ok());
}
