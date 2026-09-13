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
        device: "/dev/nbd0".into(),
        prepared: false,
        deleted: false,
        desired: false,
        worker: None,
        client: None,
        vm: None,
    }
}
fn service(root: &Path) -> Service {
    Service {
        executable: "/unused".into(),
        config: Config {
            client_uid: 0,
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
            Arc::new(Mutex::new(Entry {
                record: record(id, &dir),
                failures: 0,
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
        Arc::new(Mutex::new(Entry {
            record: record(&id, &dir),
            failures: 0,
            retry_at: Instant::now(),
        })),
    );
    assert!(s
        .request(Request {
            version: 1,
            operation: "attach".into(),
            volume_id: id,
            sandbox_dir: dir.join("copy"),
            image: None
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
            image: None
        })
        .is_err());
    assert!(s.entries.lock().unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn privileged_import_refuses_writable_or_symlink_sources() {
    if !rustix::process::geteuid().is_root() {
        return;
    }
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
