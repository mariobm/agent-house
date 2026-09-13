use super::*;
use std::sync::atomic::AtomicU64;
static NEXT: AtomicU64 = AtomicU64::new(0);
fn temp() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "volumed-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&p).unwrap();
    p
}
#[test]
fn process_reuse_never_signals_current_process() {
    let mut p = Process::read(std::process::id()).unwrap().unwrap();
    p.start += 1;
    p.stop().unwrap();
    assert!(!p.alive().unwrap());
}
#[test]
fn boot_identity_is_part_of_process_identity() {
    let mut p = Process::read(std::process::id()).unwrap().unwrap();
    p.boot.push('x');
    assert!(!p.alive().unwrap());
    p.stop().unwrap();
}
fn record(id: &str, root: &Path) -> Record {
    Record {
        id: id.into(),
        sandbox: root.join("sandbox"),
        image: root.join("image"),
        image_hash: String::new(),
        logical_bytes: crate::CHUNK_BYTES as u64,
        device: "/dev/nbd0".into(),
        evicted: false,
        prepared: false,
        deleted: false,
        reclaimed: false,
        gc_after: None,
        gc_eligible: false,
        gc_last_completed: None,
        desired: false,
        worker: None,
        client: None,
        vm: None,
    }
}
fn service(root: &Path) -> Service {
    Service {
        executable: "/unused".into(),
        admission_failed: AtomicBool::new(false),
        reclamation: Mutex::new(()),
        config: Config {
            client_uid: 0,
            resources: None,
            limits: Limits {
                max_volume_bytes: 1 << 30,
                max_logical_bytes: 1 << 30,
                max_journal_bytes: 1 << 30,
                max_cache_bytes: 1 << 30,
            },
            image_roots: vec![],
            socket_dir: None,
            root: root.into(),
            engine_root: root.into(),
            credentials: root.join("unused"),
            nbd_client: "/usr/sbin/nbd-client".into(),
            devices: vec![],
        },
        entries: Mutex::new(BTreeMap::new()),
        _locks: vec![],
    }
}
#[test]
fn busy_volume_does_not_block_other_volume_lookup() {
    let dir = temp();
    let s = service(&dir);
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    for id in [&a, &b] {
        s.entries.lock().unwrap().insert(
            id.clone(),
            Arc::new(Slot::new(Entry {
                record: record(id, &dir),
                failures: 0,
                mark: None,
                retry_at: Instant::now(),
            })),
        );
    }
    let request = |id: &str| Request {
        version: 1,
        operation: "status".into(),
        volume_id: id.into(),
        sandbox_dir: dir.join("sandbox"),
        image: None,
        logical_bytes: None,
    };
    let first = s.entry(&request(&a)).unwrap();
    let _held = first.lock().unwrap();
    assert!(s.entry(&request(&b)).unwrap().try_lock().is_ok());
    assert!(s.request(request(&a)).is_err());
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn copied_sandbox_path_cannot_reuse_attachment() {
    let dir = temp();
    let s = service(&dir);
    let id = "a".repeat(64);
    s.entries.lock().unwrap().insert(
        id.clone(),
        Arc::new(Slot::new(Entry {
            record: record(&id, &dir),
            failures: 0,
            mark: None,
            retry_at: Instant::now(),
        })),
    );
    assert!(s
        .request(Request {
            version: 1,
            operation: "attach".into(),
            volume_id: id,
            sandbox_dir: dir.join("copy"),
            image: None,
            logical_bytes: None,
        })
        .is_err());
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn atomic_records_keep_binding_and_deletion_intent() {
    let dir = temp();
    let path = dir.join("record.json");
    let mut r = record(&"a".repeat(64), &dir);
    r.deleted = true;
    save(&path, &r).unwrap();
    let restored: Record = read(&path).unwrap();
    assert!(restored.deleted);
    assert_eq!(restored.sandbox, r.sandbox);
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn unknown_and_oversized_requests_are_not_records() {
    let dir = temp();
    let s = service(&dir);
    assert!(s
        .entry(&Request {
            version: 1,
            operation: "attach".into(),
            volume_id: "a".repeat(64),
            sandbox_dir: dir.clone(),
            image: None,
            logical_bytes: None,
        })
        .is_err());
    assert!(s.entries.lock().unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn privileged_import_refuses_writable_or_symlink_sources() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run make test-volume-root"
    );
    let dir = temp();
    let file = dir.join("image");
    fs::write(&file, "test").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    trusted_image_path(&file).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(trusted_image_path(&file).is_err());
    let link = dir.join("link");
    std::os::unix::fs::symlink(&file, &link).unwrap();
    assert!(trusted_image_path(&link).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn binding_cannot_target_a_different_host_uid() {
    let dir = temp();
    let mut s = service(&dir);
    s.config.client_uid = rustix::process::geteuid().as_raw().wrapping_add(1);
    let mut r = record(&"a".repeat(64), &dir);
    r.vm = Process::read(std::process::id()).unwrap();
    assert!(s.vm(&r).is_err());
    fs::remove_dir_all(dir).unwrap();
}

fn prepare_request(s: &Service, suffix: char) -> Request {
    let id = suffix.to_string().repeat(64);
    let sandbox = s.config.engine_root.join(format!("sandbox-{suffix}"));
    fs::create_dir(&sandbox).unwrap();
    fs::write(
        sandbox.join("sandbox.json"),
        serde_json::json!({"info":{"storage":{"volume_id":id,"mode":"replicated"}}}).to_string(),
    )
    .unwrap();
    let image = s.config.root.join(format!("image-{suffix}"));
    File::create(&image)
        .unwrap()
        .set_len(crate::CHUNK_BYTES as u64)
        .unwrap();
    Request {
        version: 1,
        operation: "prepare".into(),
        volume_id: id,
        sandbox_dir: sandbox,
        image: Some(image),
        logical_bytes: None,
    }
}
fn budget_service(dir: &Path) -> Service {
    let mut s = service(dir);
    s.config.devices = vec![
        "/dev/ahvm-test-unused-a".into(),
        "/dev/ahvm-test-unused-b".into(),
    ];
    s.config.limits.max_volume_bytes = crate::CHUNK_BYTES as u64;
    s.config.limits.max_logical_bytes = crate::CHUNK_BYTES as u64;
    fs::create_dir(dir.join("volumes")).unwrap();
    s
}
#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn concurrent_imports_reserve_before_remote_effects() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run make test-volume-root"
    );
    let dir = temp();
    let s = Arc::new(budget_service(&dir));
    let requests = [prepare_request(&s, 'a'), prepare_request(&s, 'b')];
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = requests
        .into_iter()
        .map(|q| {
            let s = s.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                s.entry(&q).is_ok()
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().unwrap().then_some(()))
            .count(),
        1
    );
    assert_eq!(s.usage().unwrap().retained_volumes, 1);
    assert_eq!(fs::read_dir(dir.join("volumes")).unwrap().count(), 1);
    fs::remove_dir_all(dir).unwrap();
}
#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn failed_import_and_tombstone_keep_durable_reservation() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run make test-volume-root"
    );
    let dir = temp();
    let s = budget_service(&dir);
    let a = prepare_request(&s, 'a');
    let b = prepare_request(&s, 'b');
    let entry = s.entry(&a).unwrap();
    assert!(!entry.lock().unwrap().record.prepared);
    assert!(s.entry(&b).is_err());
    // A retry neither doubles the reservation nor needs another device slot.
    assert!(Arc::ptr_eq(&entry, &s.entry(&a).unwrap()));
    let mut record = entry.lock().unwrap().record.clone();
    record.deleted = true;
    s.persist(&record).unwrap();
    // Reconstruct the registry from disk, as restart does, with no RAM accounting.
    s.entries.lock().unwrap().clear();
    let persisted: Record = read(&s.dir(&record).join("record.json")).unwrap();
    s.entries.lock().unwrap().insert(
        record.id.clone(),
        Arc::new(Slot::new(Entry {
            record: persisted,
            failures: 0,
            mark: None,
            retry_at: Instant::now(),
        })),
    );
    assert_eq!(s.usage().unwrap().logical_bytes, crate::CHUNK_BYTES as u64);
    assert!(s.entry(&b).is_err());
    fs::remove_dir_all(dir).unwrap();
}
#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn source_growth_cannot_exceed_reserved_capacity() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run make test-volume-root"
    );
    let dir = temp();
    let s = budget_service(&dir);
    let a = prepare_request(&s, 'a');
    let e = s.entry(&a).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(a.image.unwrap())
        .unwrap()
        .set_len(2 * crate::CHUNK_BYTES as u64)
        .unwrap();
    assert!(s
        .prepare(&mut e.lock().unwrap().record)
        .unwrap_err()
        .to_string()
        .contains("size"));
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn each_budget_is_enforced_independently() {
    let dir = temp();
    let s = budget_service(&dir);
    let mut used = Usage::default();
    used.add(crate::CHUNK_BYTES as u64).unwrap();
    let mut limits = s.config.limits.clone();
    limits.max_logical_bytes = 1 << 30;
    limits.max_journal_bytes = accounting::JOURNAL_BYTES;
    assert!(limits
        .admit(&used, crate::CHUNK_BYTES as u64)
        .unwrap_err()
        .to_string()
        .contains("journal"));
    limits.max_journal_bytes *= 2;
    limits.max_cache_bytes = CACHE_BYTES;
    assert!(limits
        .admit(&used, crate::CHUNK_BYTES as u64)
        .unwrap_err()
        .to_string()
        .contains("cache"));
    limits.max_cache_bytes *= 2;
    limits.admit(&used, crate::CHUNK_BYTES as u64).unwrap();
    assert!(limits.admit(&used, 2 * crate::CHUNK_BYTES as u64).is_err());
    assert!(limits.admit(&used, 0).is_err());
    limits.max_cache_bytes = 0;
    assert!(limits.validate().is_err());
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn usage_counts_retained_files_and_refuses_symlinks() {
    let dir = temp();
    File::create(dir.join("journal"))
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();
    fs::write(dir.join("journal.new"), "pending").unwrap();
    let u = accounting::volume_usage(&dir, crate::CHUNK_BYTES as u64).unwrap();
    assert_eq!(u.local_file_bytes, 1024 * 1024 + 7);
    assert_eq!(
        u.reservation.journal_reserved_bytes,
        2 * crate::local::LOG_LIMIT
    );
    std::os::unix::fs::symlink("/etc", dir.join("outside")).unwrap();
    assert!(accounting::volume_usage(&dir, crate::CHUNK_BYTES as u64).is_err());
    fs::remove_dir_all(dir).unwrap();
}
#[test]
fn old_records_without_capacity_fail_closed() {
    let dir = temp();
    let mut value = serde_json::to_value(record(&"a".repeat(64), &dir)).unwrap();
    value.as_object_mut().unwrap().remove("logical_bytes");
    assert!(serde_json::from_value::<Record>(value).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn failed_record_publication_freezes_import_admission() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run make test-volume-root"
    );
    let dir = temp();
    let s = budget_service(&dir);
    let a = prepare_request(&s, 'a');
    let b = prepare_request(&s, 'b');
    let volume_dir = dir.join("volumes").join(&a.volume_id);
    private_dir(&volume_dir).unwrap();
    fs::create_dir(volume_dir.join("record.tmp")).unwrap();
    assert!(s.entry(&a).is_err());
    assert!(s
        .entry(&b)
        .unwrap_err()
        .to_string()
        .contains("restart required"));
    assert!(s
        .entry(&a)
        .unwrap_err()
        .to_string()
        .contains("restart required"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn lower_budgets_preserve_existing_identity_and_usage_includes_tombstones() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run make test-volume-root"
    );
    let dir = temp();
    let mut s = budget_service(&dir);
    s.config.limits.max_logical_bytes *= 2;
    let a = prepare_request(&s, 'a');
    let b = prepare_request(&s, 'b');
    let first = s.entry(&a).unwrap();
    s.entry(&b).unwrap();
    s.config.limits.max_logical_bytes /= 2;
    assert!(Arc::ptr_eq(&first, &s.entry(&a).unwrap()));
    assert!(s.entry(&prepare_request(&s, 'c')).is_err());
    {
        let mut e = first.lock().unwrap();
        e.record.deleted = true;
        s.persist(&e.record).unwrap();
    }
    fs::remove_dir_all(&a.sandbox_dir).unwrap();
    let reply = s
        .request(Request {
            operation: "usage".into(),
            ..a
        })
        .unwrap();
    assert_eq!(reply["host_reservations"]["retained_volumes"], 2);
    assert_eq!(
        reply["usage"]["reservation"]["logical_bytes"],
        crate::CHUNK_BYTES
    );
    assert!(reply["usage"]["local_file_bytes"].as_u64().unwrap() > 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn reclamation_releases_budget_only_after_remote_and_local_cleanup() {
    assert!(rustix::process::geteuid().is_root());
    let dir = temp();
    let s = budget_service(&dir);
    let a = prepare_request(&s, 'a');
    let b = prepare_request(&s, 'b');
    let entry = s.entry(&a).unwrap();
    let raw = Arc::new(crate::reclaim::tests::Memory::default());
    let bytes = vec![1; crate::CHUNK_BYTES];
    raw.put_chunk(&a.volume_id, &crate::digest(&bytes), &bytes)
        .unwrap();
    let mut e = entry.lock().unwrap();
    assert!(s.reclaim_with_store(&mut e.record, raw.clone()).is_err());
    e.record.deleted = true;
    s.persist(&e.record).unwrap();
    let owner = s.dir(&e.record).join("owner");
    private_dir(&owner).unwrap();
    fs::write(owner.join("pending-journal"), b"local").unwrap();
    s.reclaim_with_store(&mut e.record, raw.clone()).unwrap();
    assert!(!e.record.reclaimed); // First batch removed chunks, no empty listing yet.
    assert!(owner.exists());
    assert!(s.entry(&b).is_err());
    s.reclaim_with_store(&mut e.record, raw.clone()).unwrap();
    assert!(e.record.reclaimed);
    assert!(!owner.exists());
    assert_eq!(s.usage().unwrap().retained_volumes, 0);
    let persisted: Record = read(&s.dir(&e.record).join("record.json")).unwrap();
    assert!(persisted.reclaimed);
    assert!(s.entry(&b).is_ok());
    // An hourly recheck is idempotent and leaves no owner directory behind.
    s.reclaim_with_store(&mut e.record, raw).unwrap();
    assert!(!owner.exists());
    drop(e);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn busy_foreground_admission_cancels_an_already_admitted_collector() {
    let dir = temp();
    let s = service(&dir);
    let id = "a".repeat(64);
    let slot = Arc::new(Slot::new(Entry {
        record: record(&id, &dir),
        failures: 0,
        mark: None,
        retry_at: Instant::now(),
    }));
    s.entries.lock().unwrap().insert(id.clone(), slot.clone());
    let ticket = slot.cancellation.load(Ordering::SeqCst);
    let held = slot.lock().unwrap();
    let err = s
        .request(Request {
            version: 1,
            operation: "attach".into(),
            volume_id: id,
            sandbox_dir: dir.join("sandbox"),
            image: None,
            logical_bytes: None,
        })
        .unwrap_err();
    assert_eq!(err.to_string(), "volume busy");
    assert_ne!(slot.cancellation.load(Ordering::SeqCst), ticket);
    drop(held);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn foreground_waits_for_offline_collector_to_yield() {
    let dir = temp();
    let slot = Arc::new(Slot::new(Entry {
        record: record(&"a".repeat(64), &dir),
        failures: 0,
        mark: None,
        retry_at: Instant::now(),
    }));
    let ticket = slot.cancellation.load(Ordering::SeqCst);
    let held = slot.lock().unwrap();
    let collection = slot.collection();
    // Read-only polling must not continually cancel background maintenance.
    assert!(slot.foreground(false).is_err());
    assert_eq!(slot.cancellation.load(Ordering::SeqCst), ticket);
    let waiter = slot.clone();
    let thread = thread::spawn(move || drop(waiter.foreground(true).unwrap()));
    let end = Instant::now() + Duration::from_secs(2);
    while slot.cancellation.load(Ordering::SeqCst) == ticket {
        assert!(Instant::now() < end);
        thread::yield_now();
    }
    drop(collection);
    drop(held);
    thread.join().unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn cold_disks_release_local_capacity_and_reacquire_without_double_charging() {
    use crate::nbd::Disk;
    let dir = temp();
    let mut s = budget_service(&dir);
    s.config.devices.truncate(1);
    s.config.limits.max_logical_bytes = 2 * crate::CHUNK_BYTES as u64;
    s.config.limits.max_journal_bytes = accounting::JOURNAL_BYTES;
    s.config.limits.max_cache_bytes = CACHE_BYTES;
    let raw = Arc::new(crate::reclaim::tests::Memory::default());
    let a = prepare_request(&s, 'a');
    let b = prepare_request(&s, 'b');
    let entry = s.entry(&a).unwrap();
    let mut e = entry.lock().unwrap();
    let r = &mut e.record;
    IndexedVolume::create(raw.clone(), &r.id, r.logical_bytes).unwrap();
    OwnedDisk::enroll(raw.clone(), &r.id).unwrap();
    r.prepared = true;
    r.gc_eligible = true;
    let owner = s.dir(r).join("owner");
    private_dir(&owner).unwrap();
    let mut disk = OwnedDisk::open(raw.clone(), &r.id, &owner).unwrap();
    disk.write(0, b"keep").unwrap();
    disk.flush().unwrap();
    drop(disk);
    s.persist(r).unwrap();
    assert!(s.entry(&b).is_err());
    s.evict_with_store(r, raw.clone()).unwrap();
    let usage = s.usage().unwrap();
    assert_eq!(usage.logical_bytes, r.logical_bytes);
    assert_eq!(usage.journal_reserved_bytes, 0);
    assert_eq!(usage.cache_reserved_bytes, 0);
    assert!(!owner.join("journal").exists());
    let peer = s.entry(&b).unwrap();
    assert!(s.reserve_residency(r).is_err());
    assert!(r.evicted);
    assert_eq!(s.usage().unwrap().retained_volumes, 2);
    // The cold record's historical device path must not affect its new owner.
    s.detach(r).unwrap();
    assert!(s.attach(r).is_err());
    assert!(!peer.lock().unwrap().record.evicted);
    {
        let mut peer = peer.lock().unwrap();
        peer.record.deleted = true;
        peer.record.reclaimed = true;
        s.persist(&peer.record).unwrap();
    }
    // Simulate reopening the durable record after a service restart.
    *r = read(&s.dir(r).join("record.json")).unwrap();
    s.reserve_residency(r).unwrap();
    assert!(!r.evicted);
    let usage = s.usage().unwrap();
    assert_eq!(usage.logical_bytes, r.logical_bytes);
    assert_eq!(usage.journal_reserved_bytes, accounting::JOURNAL_BYTES);
    assert_eq!(usage.cache_reserved_bytes, CACHE_BYTES);
    s.reserve_residency(r).unwrap(); // Idempotent admission.
    let mut disk = OwnedDisk::open(raw.clone(), &r.id, &owner).unwrap();
    let mut data = [0; 4];
    disk.read(0, &mut data).unwrap();
    assert_eq!(&data, b"keep");
    drop(disk);
    s.evict_with_store(r, raw.clone()).unwrap();
    r.deleted = true;
    s.persist(r).unwrap();
    for _ in 0..4 {
        s.reclaim_with_store(r, raw.clone()).unwrap();
        if r.reclaimed {
            break;
        }
    }
    assert!(r.reclaimed);
    assert_eq!(s.usage().unwrap().logical_bytes, 0);
    drop(e);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn retirement_of_unregistered_disk_requires_observed_cleanup() {
    let dir = temp();
    let s = budget_service(&dir);
    let id = "a".repeat(64);
    let request = || Request {
        version: 1,
        operation: "retire".into(),
        volume_id: id.clone(),
        sandbox_dir: s.config.engine_root.join("missing-sandbox"),
        image: None,
        logical_bytes: Some(crate::CHUNK_BYTES as u64),
    };
    let ack = s.request(request()).unwrap();
    assert_eq!(ack["reclamation_complete"], false);
    assert_eq!(s.usage().unwrap().logical_bytes, crate::CHUNK_BYTES as u64);
    assert_eq!(s.usage().unwrap().journal_reserved_bytes, 0);
    let entry = s.entry(&request()).unwrap();
    let raw = Arc::new(crate::reclaim::tests::Memory::default());
    {
        let mut e = entry.lock().unwrap();
        assert!(e.record.deleted && e.record.evicted);
        assert!(!e.record.prepared);
        s.reclaim_with_store(&mut e.record, raw).unwrap();
    }
    assert_eq!(s.request(request()).unwrap()["reclamation_complete"], true);
    assert_eq!(s.usage().unwrap().logical_bytes, 0);
    let mut wrong = request();
    wrong.logical_bytes = Some(2 * crate::CHUNK_BYTES as u64);
    assert!(s.request(wrong).is_err());
    let mut wrong = request();
    wrong.sandbox_dir = dir.join("different");
    assert!(s.request(wrong).is_err());
    assert_eq!(s.request(request()).unwrap()["reclamation_complete"], true);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn unknown_retirement_cannot_claim_a_foreign_owned_disk() {
    use crate::nbd::Disk;
    let dir = temp();
    let s = budget_service(&dir);
    let id = "a".repeat(64);
    let raw = Arc::new(crate::reclaim::tests::Memory::default());
    IndexedVolume::create(raw.clone(), &id, crate::CHUNK_BYTES as u64).unwrap();
    OwnedDisk::enroll(raw.clone(), &id).unwrap();
    let foreign = dir.join("foreign");
    private_dir(&foreign).unwrap();
    let mut disk = OwnedDisk::open(raw.clone(), &id, &foreign).unwrap();
    disk.write(0, b"keep").unwrap();
    disk.sync_remote().unwrap();
    let request = || Request {
        version: 1,
        operation: "retire".into(),
        volume_id: id.clone(),
        sandbox_dir: dir.join("missing"),
        image: None,
        logical_bytes: Some(crate::CHUNK_BYTES as u64),
    };
    assert_eq!(s.request(request()).unwrap()["reclamation_complete"], false);
    let entry = s.entry(&request()).unwrap();
    {
        let mut e = entry.lock().unwrap();
        assert!(s.reclaim_with_store(&mut e.record, raw.clone()).is_err());
        assert!(!e.record.reclaimed);
    }
    let mut bytes = [0; 4];
    disk.read(0, &mut bytes).unwrap();
    assert_eq!(&bytes, b"keep");
    assert_eq!(s.request(request()).unwrap()["reclamation_complete"], false);
    drop(disk);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn admitted_size_refuses_growth_before_registration() {
    let dir = temp();
    let s = budget_service(&dir);
    let mut q = prepare_request(&s, 'a');
    q.logical_bytes = Some(crate::CHUNK_BYTES as u64 * 2);
    assert!(s
        .entry(&q)
        .err()
        .unwrap()
        .to_string()
        .contains("sizing mismatch"));
    assert!(s.entries.lock().unwrap().is_empty());
    assert_eq!(fs::read_dir(dir.join("volumes")).unwrap().count(), 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires Linux root; run make test-volume-root"]
fn admitted_size_refuses_changed_reservation_on_retry() {
    assert!(
        rustix::process::geteuid().is_root(),
        "run make test-volume-root"
    );
    let dir = temp();
    let s = budget_service(&dir);
    let mut q = prepare_request(&s, 'a');
    // Successful registration persists a root-owned service directory. Keep
    // that real permission check and exercise it in the privileged CI gate.
    q.logical_bytes = Some(crate::CHUNK_BYTES as u64);
    s.entry(&q).unwrap();
    q.logical_bytes = Some(crate::CHUNK_BYTES as u64 * 2);
    assert!(s
        .request(q)
        .unwrap_err()
        .to_string()
        .contains("sizing mismatch"));
    fs::remove_dir_all(dir).unwrap();
}
