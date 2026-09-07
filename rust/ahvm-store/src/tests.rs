//! Store roundtrips. In-memory SQLite; each test owns its database.

use super::*;
use serde_json::json;

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
