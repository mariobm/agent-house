//! Store roundtrips. In-memory SQLite; each test owns its database.

use super::*;
use serde_json::json;

#[test]
fn combined_idle_policy_is_atomic_and_survives_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "ahvm-agent-policy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("policy.db");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.agent_idle_stop_secs().unwrap(), None);
    store.set_pause_after_secs(30).unwrap(); // Existing database, before agent policy.
    assert_eq!(store.agent_idle_stop_secs().unwrap(), None);
    store.set_idle_policy(45, 600).unwrap();
    store.with_conn(|c| {
        c.execute_batch("CREATE TRIGGER refuse_agent_policy BEFORE INSERT ON host_settings WHEN NEW.key='agent_idle_stop_secs' BEGIN SELECT RAISE(ABORT,'test failure'); END;")?;
        Ok(())
    }).unwrap();
    assert!(store.set_idle_policy(90, 120).is_err());
    assert_eq!(store.pause_after_secs().unwrap(), Some(45));
    assert_eq!(store.agent_idle_stop_secs().unwrap(), Some(600));
    assert!(store.set_idle_policy(30, 0).is_err());
    drop(store);
    let reopened = Store::open(path).unwrap();
    assert_eq!(reopened.pause_after_secs().unwrap(), Some(45));
    assert_eq!(reopened.agent_idle_stop_secs().unwrap(), Some(600));
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

fn user(id: &str, now: i64) -> User {
    User {
        id: id.into(),
        name: format!("{id}-name"),
        api_key_hash: format!("hash-{id}"),
        max_sandboxes: 5,
        max_cpus: 4,
        max_memory_mb: 4096,
        max_volumes_mb: 20480,
        max_snapshots: 5,
        created_at: now,
        updated_at: now,
    }
}

fn sandbox(id: &str, owner: &str, ts: i64) -> Sandbox {
    Sandbox {
        id: id.into(),
        owner_user_id: owner.into(),
        name: format!("{id}-sb"),
        backend: Backend::Krucible,
        state: "running".into(),
        thermal: "hot".into(),
        cpus: 2,
        memory_mb: 1024,
        ip: "100.64.0.2".into(),
        created_at: ts,
        updated_at: ts,
    }
}

#[test]
fn user_crud_and_key_lookup() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u1", 100)).unwrap();
    assert_eq!(s.get_user("u1").unwrap().name, "u1-name");
    assert_eq!(s.get_user_by_key_hash("hash-u1").unwrap().id, "u1");
    assert!(matches!(s.get_user("nope"), Err(Error::NotFound(_))));

    // Rename conflict with a different id is rejected.
    let mut clash = user("u2", 100);
    clash.name = "u1-name".into();
    s.upsert_user(&user("u2", 100)).unwrap();
    assert!(matches!(s.upsert_user(&clash), Err(Error::Conflict(_))));
}

#[test]
fn sandbox_lifecycle_and_cursor_pages() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u1", 1)).unwrap();
    for (i, ts) in [("s1", 10), ("s2", 20), ("s3", 30)] {
        s.create_sandbox(&sandbox(i, "u1", ts)).unwrap();
    }
    s.set_sandbox_state("s1", "stopped", "cold", 40).unwrap();
    let got = s.get_sandbox("s1").unwrap();
    assert_eq!(
        (got.state.as_str(), got.thermal.as_str()),
        ("stopped", "cold")
    );
    assert!(matches!(
        s.set_sandbox_state("nope", "stopped", "cold", 40),
        Err(Error::NotFound(_))
    ));

    let p1 = s.list_sandboxes("u1", None, 2).unwrap();
    assert_eq!(
        p1.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        ["s3", "s2"]
    );
    let cursor = (p1[1].created_at, p1[1].id.clone());
    let p2 = s.list_sandboxes("u1", Some(cursor), 2).unwrap();
    assert_eq!(p2.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["s1"]);

    // Firecracker rows coexist with krucible rows.
    let mut fc = sandbox("s4", "u1", 40);
    fc.backend = Backend::Firecracker;
    s.create_sandbox(&fc).unwrap();
    assert_eq!(s.get_sandbox("s4").unwrap().backend, Backend::Firecracker);

    s.delete_sandbox("s1").unwrap();
    assert!(matches!(s.get_sandbox("s1"), Err(Error::NotFound(_))));
}

#[test]
fn list_all_sandboxes_spans_owners() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u1", 1)).unwrap();
    s.upsert_user(&user("u2", 1)).unwrap();
    s.create_sandbox(&sandbox("s1", "u1", 10)).unwrap();
    s.create_sandbox(&sandbox("s2", "u2", 20)).unwrap();
    let all = s.list_all_sandboxes(None, 10).unwrap();
    assert_eq!(
        all.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        ["s2", "s1"]
    );
    let cursor = (all[0].created_at, all[0].id.clone());
    let rest = s.list_all_sandboxes(Some(cursor), 10).unwrap();
    assert_eq!(
        rest.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        ["s1"]
    );
}

#[test]
fn snapshot_remote_lifecycle() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u1", 1)).unwrap();
    s.create_sandbox(&sandbox("s1", "u1", 10)).unwrap();
    let snap = Snapshot {
        id: "snap1".into(),
        owner_user_id: "u1".into(),
        sandbox_id: Some("s1".into()),
        name: "cp".into(),
        kind: "memory".into(),
        state: "local".into(),
        local_bytes: 512 << 20,
        remote_state: "none".into(),
        remote_manifest_key: None,
        created_at: 20,
        expires_at: None,
    };
    s.create_snapshot(&snap).unwrap();
    s.set_snapshot_remote("snap1", "backed", Some("snapshots/u1/snap1.json"))
        .unwrap();
    let got = s.get_snapshot("snap1").unwrap();
    assert_eq!(got.remote_state, "backed");
    assert_eq!(
        got.remote_manifest_key.as_deref(),
        Some("snapshots/u1/snap1.json")
    );
    s.delete_snapshot("snap1").unwrap();
    assert!(matches!(s.get_snapshot("snap1"), Err(Error::NotFound(_))));
}

#[test]
fn volume_attach_cycle() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u1", 1)).unwrap();
    s.create_sandbox(&sandbox("s1", "u1", 10)).unwrap();
    s.create_sandbox(&sandbox("s2", "u1", 11)).unwrap();
    let vol = Volume {
        id: "v1".into(),
        owner_user_id: "u1".into(),
        name: "data".into(),
        size_mb: 1024,
        attached_to: None,
        created_at: 15,
    };
    s.create_volume(&vol).unwrap();
    // Attach, idempotent re-attach to the same sandbox, detach.
    s.attach_volume("v1", "s1").unwrap();
    assert_eq!(
        s.get_volume("v1").unwrap().attached_to.as_deref(),
        Some("s1")
    );
    s.attach_volume("v1", "s1").unwrap();
    s.detach_volume("v1", "s1").unwrap();
    assert_eq!(s.get_volume("v1").unwrap().attached_to, None);
    // Detaching when free, or from the wrong sandbox, is a Conflict —
    // never a silent no-op that drops someone else's claim.
    assert!(matches!(
        s.detach_volume("v1", "s1"),
        Err(Error::Conflict(_))
    ));
    s.attach_volume("v1", "s1").unwrap();
    assert!(matches!(
        s.detach_volume("v1", "s2"),
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        s.get_volume("v1").unwrap().attached_to.as_deref(),
        Some("s1")
    );
    // Stealing an attached volume fails instead of forgetting s1.
    assert!(matches!(
        s.attach_volume("v1", "s2"),
        Err(Error::Conflict(_))
    ));
    assert_eq!(
        s.get_volume("v1").unwrap().attached_to.as_deref(),
        Some("s1")
    );
    // Unknown volume is NotFound, not Conflict.
    assert!(matches!(
        s.attach_volume("nope", "s1"),
        Err(Error::NotFound(_))
    ));
    s.delete_volume("v1").unwrap();
}

#[test]
fn transaction_commits_state_plus_event_atomically() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u1", 1)).unwrap();
    s.create_sandbox(&sandbox("s1", "u1", 10)).unwrap();
    let seq = s
        .transaction(|tx| {
            tx.set_sandbox_state("s1", "stopped", "cold", 40)?;
            tx.record_event("sandbox.stopped", "u1", "s1", &json!({}), 40)
        })
        .unwrap();
    assert_eq!(s.get_sandbox("s1").unwrap().state, "stopped");
    let evts = s
        .query_events(&EventFilter {
            after_seq: seq - 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(evts.len(), 1);
    assert_eq!(evts[0].r#type, "sandbox.stopped");
}

#[test]
fn transaction_rolls_back_event_when_mutation_fails() {
    // The dashboard must never see an event for a change that didn't commit.
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u1", 1)).unwrap();
    let before = s.query_events(&EventFilter::default()).unwrap().len();
    let err = s
        .transaction(|tx| {
            tx.record_event("sandbox.stopped", "u1", "ghost", &json!({}), 40)?;
            tx.set_sandbox_state("ghost", "stopped", "cold", 40)
        })
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
    let after = s.query_events(&EventFilter::default()).unwrap().len();
    assert_eq!(before, after, "rolled-back event leaked");
}

#[test]
fn images_and_tasks() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_image(&Image {
        id: "img1".into(),
        name: "minimal".into(),
        source: "built-in".into(),
        size_mb: 200,
        created_at: 5,
    })
    .unwrap();
    assert_eq!(s.list_images().unwrap().len(), 1);
    s.create_task(&Task {
        id: "t1".into(),
        kind: "snapshot-push".into(),
        state: "running".into(),
        progress: 0.25,
        created_at: 6,
        updated_at: 6,
    })
    .unwrap();
    s.set_task("t1", "done", 1.0, 9).unwrap();
    let t = s.get_task("t1").unwrap();
    assert_eq!((t.state.as_str(), t.progress), ("done", 1.0));
}

#[test]
fn event_bus_cursors_and_filters() {
    let s = Store::open_in_memory().unwrap();
    let q1 = s
        .record_event("sandbox.created", "u1", "s1", &json!({"a": 1}), 10)
        .unwrap();
    let q2 = s
        .record_event("sandbox.stopped", "u1", "s1", &json!({}), 11)
        .unwrap();
    s.record_event("volume.created", "u1", "", &json!({}), 12)
        .unwrap();
    assert!(q2 > q1);

    let all = s.query_events(&EventFilter::default()).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].seq, q1);

    let sandbox_evts = s
        .query_events(&EventFilter {
            type_prefix: Some("sandbox.".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(sandbox_evts.len(), 2);

    let tail = s
        .query_events(&EventFilter {
            after_seq: q2,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].r#type, "volume.created");

    let scoped = s
        .query_events(&EventFilter {
            sandbox_id: Some("s1".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(scoped.len(), 2);
}

#[test]
fn replicated_tenant_quota_survives_failure_stop_and_deletion_intent() {
    let s = Store::open_in_memory().unwrap();
    let mut alice = user("alice", 0);
    alice.max_volumes_mb = 1;
    s.upsert_user(&alice).unwrap();
    s.upsert_user(&user("bob", 0)).unwrap();
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let bytes = 1024 * 1024;
    s.reserve_replicated_volume("alice", "vm", &a, bytes, 0)
        .unwrap();
    // No sandbox row was ever committed: a failed create still holds its disk.
    assert_eq!(s.replicated_usage("alice").unwrap().logical_bytes, bytes);
    assert!(s
        .reserve_replicated_volume("alice", "other", &b, bytes, 1)
        .is_err());
    s.reserve_replicated_volume("alice", "vm", &a, bytes, 1)
        .unwrap();
    assert_eq!(s.replicated_usage("alice").unwrap().retained_volumes, 1);
    assert!(matches!(
        s.replicated_reservation("bob", &a),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        s.delete_replicated_reservation("bob", &a, 2),
        Err(Error::NotFound(_))
    ));
    assert!(s.confirm_replicated_reclamation("alice", &a, 2).is_err());
    s.delete_replicated_reservation("alice", &a, 2).unwrap();
    assert!(s
        .reserve_replicated_volume("alice", "other", &b, bytes, 3)
        .is_err());
    s.confirm_replicated_reclamation("alice", &a, 3).unwrap();
    s.confirm_replicated_reclamation("alice", &a, 4).unwrap();
    s.delete_replicated_reservation("alice", &a, 4).unwrap();
    assert_eq!(s.replicated_usage("alice").unwrap().logical_bytes, 0);
    assert!(s
        .reserve_replicated_volume("alice", "vm", &a, bytes, 4)
        .is_err());
    // Sandbox names can be reused only after the former disk is reclaimed.
    s.reserve_replicated_volume("alice", "vm", &b, bytes, 4)
        .unwrap();
    s.reserve_replicated_volume("bob", "bobs-vm", &"c".repeat(64), bytes, 4)
        .unwrap();
}

#[test]
fn replicated_reservation_identity_and_limits_are_immutable() {
    let s = Store::open_in_memory().unwrap();
    let mut alice = user("alice", 0);
    s.upsert_user(&alice).unwrap();
    s.upsert_user(&user("bob", 0)).unwrap();
    let a = "a".repeat(64);
    s.reserve_replicated_volume("alice", "vm", &a, 65536, 0)
        .unwrap();
    for (owner, sandbox, size) in [
        ("bob", "vm", 65536),
        ("alice", "other", 65536),
        ("alice", "vm", 131072),
    ] {
        assert!(s
            .reserve_replicated_volume(owner, sandbox, &a, size, 0)
            .is_err());
    }
    assert!(s
        .reserve_replicated_volume("bob", "vm", &"b".repeat(64), 65536, 0)
        .is_err());
    for size in [-1, 0, 1, i64::MAX, 64 * 1024 * 1024 * 1024 + 65536] {
        assert!(s
            .reserve_replicated_volume("alice", "new", &"b".repeat(64), size, 0)
            .is_err());
    }
    for limit in [0, -1, i64::MAX] {
        alice.max_volumes_mb = limit;
        s.upsert_user(&alice).unwrap();
        assert!(s
            .reserve_replicated_volume("alice", "new", &"b".repeat(64), 65536, 0)
            .is_err());
        s.reserve_replicated_volume("alice", "vm", &a, 65536, 0)
            .unwrap();
    }
}

#[test]
fn independent_connections_cannot_overbook_and_restart_keeps_charges() {
    let dir = std::env::temp_dir().join(format!(
        "ahvm-ledger-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("store.db");
    let s = Store::open(&path).unwrap();
    let mut alice = user("alice", 0);
    alice.max_volumes_mb = 1;
    s.upsert_user(&alice).unwrap();
    let peer = Store::open(&path).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let outcomes = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            s.reserve_replicated_volume("alice", "a", &"a".repeat(64), 1024 * 1024, 0)
        });
        let b = scope.spawn(|| {
            barrier.wait();
            peer.reserve_replicated_volume("alice", "b", &"b".repeat(64), 1024 * 1024, 0)
        });
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(outcomes.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(outcomes
        .iter()
        .any(|r| matches!(r, Err(Error::Conflict(_)))));
    let winner = outcomes.into_iter().find_map(Result::ok).unwrap();
    s.delete_replicated_reservation("alice", &winner.volume_id, 1)
        .unwrap();
    drop(s);
    drop(peer);
    let reopened = Store::open(&path).unwrap();
    assert_eq!(
        reopened.replicated_usage("alice").unwrap().logical_bytes,
        1024 * 1024
    );
    assert_eq!(
        reopened
            .replicated_reservation("alice", &winner.volume_id)
            .unwrap()
            .state,
        "deleting"
    );
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn replicated_recovery_scan_and_existing_volume_capacity() {
    let s = Store::open_in_memory().unwrap();
    let mut u = user("alice", 0);
    u.max_volumes_mb = 1;
    s.upsert_user(&u).unwrap();
    s.create_volume(&Volume {
        id: "data".into(),
        owner_user_id: u.id.clone(),
        name: "data".into(),
        size_mb: 1,
        attached_to: None,
        created_at: 0,
    })
    .unwrap();
    assert!(s
        .reserve_replicated_volume("alice", "a", &"a".repeat(64), 65536, 0)
        .is_err());
    s.delete_volume("data").unwrap();
    for key in ["a", "b", "c"] {
        s.reserve_replicated_volume("alice", key, &key.repeat(64), 65536, 0)
            .unwrap();
    }
    s.delete_replicated_reservation("alice", &"a".repeat(64), 1)
        .unwrap();
    s.confirm_replicated_reclamation("alice", &"a".repeat(64), 2)
        .unwrap();
    s.delete_replicated_reservation("alice", &"c".repeat(64), 1)
        .unwrap();
    let first = s.retained_replicated_reservations(None, 1).unwrap();
    assert_eq!(first[0].volume_id, "b".repeat(64));
    let second = s
        .retained_replicated_reservations(Some(&first[0].volume_id), 1)
        .unwrap();
    assert_eq!(second[0].volume_id, "c".repeat(64));
    assert_eq!(second[0].state, "deleting");
    assert!(s
        .retained_replicated_reservations(Some(&second[0].volume_id), 1)
        .unwrap()
        .is_empty());
    assert!(s.retained_replicated_reservations(None, 0).is_err());
    assert!(s.retained_replicated_reservations(None, 1001).is_err());
}

#[test]
fn idle_policy_survives_reopen_including_disabled_value() {
    let dir = std::env::temp_dir().join(format!(
        "ahvm-policy-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("state.db");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.pause_after_secs().unwrap(), None);
    store.set_pause_after_secs(60).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.pause_after_secs().unwrap(), Some(60));
    store.set_pause_after_secs(0).unwrap();
    drop(store);
    assert_eq!(
        Store::open(&path).unwrap().pause_after_secs().unwrap(),
        Some(0)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn managed_run_retries_fences_and_terminal_receipts() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u", 1)).unwrap();
    s.create_sandbox(&sandbox("vm", "u", 1)).unwrap();
    let (r, fresh) = s.admit_managed_run("r", "vm", "u", "{}", 10, 1000).unwrap();
    assert!(fresh);
    assert_eq!(
        s.admit_managed_run("r", "vm", "u", "{}", 20, 2000).unwrap(),
        (r.clone(), false)
    );
    assert!(matches!(
        s.admit_managed_run("r", "vm", "u", "other", 20, 2000),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        s.admit_managed_run("another", "vm", "u", "{}", 20, 2000),
        Err(Error::Conflict(_))
    ));
    let owned = s.claim_managed_run("r", 30).unwrap();
    assert_eq!(owned.epoch, 2);
    assert!(s
        .update_managed_run("r", 1, "succeeded", None, None, Some(0), None, 40, true)
        .is_err());
    s.update_managed_run(
        "r",
        2,
        "running",
        Some("sid"),
        Some("boot"),
        None,
        None,
        40,
        false,
    )
    .unwrap();
    assert!(s
        .update_managed_run(
            "r",
            2,
            "running",
            Some("other"),
            None,
            None,
            None,
            41,
            false
        )
        .is_err());
    assert!(s
        .update_managed_run(
            "r",
            2,
            "running",
            None,
            Some("other"),
            None,
            None,
            41,
            false
        )
        .is_err());
    assert!(s
        .update_managed_run("r", 2, "running", None, None, None, None, 41, true)
        .is_err());
    let done = s
        .update_managed_run("r", 2, "succeeded", None, None, Some(0), None, 50, true)
        .unwrap();
    assert_eq!(done.session_id.as_deref(), Some("sid"));
    assert_eq!(done.finished_at, Some(50));
    assert!(s.claim_managed_run("r", 60).is_err());
    assert!(s.managed_run_for_sandbox("vm").unwrap().is_none());
    assert_eq!(
        s.list_managed_run_cooldowns().unwrap(),
        vec![("vm".into(), 50)]
    );
    assert_eq!(
        s.admit_managed_run("r", "vm", "u", "{}", 10000, 20000)
            .unwrap(),
        (done, false)
    );
    assert!(
        s.admit_managed_run("next", "vm", "u", "{}", 60, 1000)
            .unwrap()
            .1
    );
    s.delete_sandbox("vm").unwrap();
    assert!(s.list_active_managed_runs().unwrap().is_empty());
    assert!(matches!(s.get_managed_run("r"), Err(Error::NotFound(_))));
}

#[test]
fn managed_runs_have_bounded_admission_without_evicting_retry_receipts() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_user(&user("u", 1)).unwrap();
    for i in 0..65 {
        let id = format!("vm{i}");
        s.create_sandbox(&sandbox(&id, "u", 1)).unwrap();
        let r = s.admit_managed_run(&format!("run{i}"), &id, "u", "{}", 1, 1000);
        assert_eq!(r.is_ok(), i < 64);
    }
    assert!(
        !s.admit_managed_run("run0", "vm0", "u", "{}", 1, 1000)
            .unwrap()
            .1
    );
    s.update_managed_run("run0", 1, "failed", None, None, None, None, 2, true)
        .unwrap();
    assert!(s
        .admit_managed_run("run64", "vm64", "u", "{}", 2, 1000)
        .is_ok());
}

#[test]
fn managed_runs_survive_database_reopen() {
    let path = std::env::temp_dir().join(format!("ahvm-run-reopen-{}.db", std::process::id()));
    let s = Store::open(&path).unwrap();
    s.upsert_user(&user("u", 1)).unwrap();
    s.create_sandbox(&sandbox("vm", "u", 1)).unwrap();
    s.admit_managed_run("job", "vm", "u", "{}", 1, 100).unwrap();
    s.update_managed_run(
        "job",
        1,
        "running",
        Some("session"),
        Some("boot"),
        None,
        None,
        2,
        false,
    )
    .unwrap();
    drop(s);
    let s = Store::open(&path).unwrap();
    let r = s.claim_managed_run("job", 10).unwrap();
    assert_eq!(r.epoch, 2);
    assert_eq!(r.session_id.as_deref(), Some("session"));
    assert_eq!(r.deadline_at, 100);
    drop(s);
    std::fs::remove_file(path).unwrap();
}
