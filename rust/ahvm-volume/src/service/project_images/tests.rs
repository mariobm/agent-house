use super::*;
use crate::indexed::tests::Store;
use std::sync::{atomic::AtomicU64, mpsc};
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    service: Arc<Service>,
    candidate: Candidate,
    raw: Arc<Store>,
    bytes: Vec<u8>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn root_tests() -> bool {
    rustix::process::geteuid().is_root()
}
fn metadata() -> ProjectImageMetadata {
    ProjectImageMetadata {
        schema: 1,
        tenant_id: "11111111-1111-1111-1111-111111111111".into(),
        project_id: "22222222-2222-2222-2222-222222222222".into(),
        build_id: "33333333-3333-3333-3333-333333333333".into(),
        source_commit: "a".repeat(40),
        base_image_digest: "b".repeat(64),
        base_content_digest: "e".repeat(64),
        revision_id: "c".repeat(64),
        install_hook_digest: Some("d".repeat(64)),
        start_hook_digest: None,
        canonical_workdir: "/workspace/project".into(),
    }
}
fn fixture() -> Fixture {
    let root = std::env::temp_dir().join(format!(
        "ahvm-project-storage-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    for name in [
        "state",
        "sandboxes",
        "images",
        "images/projects",
        "state/volumes",
        "state/base-metadata",
    ] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(
            root.join(name),
            fs::Permissions::from_mode(if name.contains("state") {
                0o700
            } else if name == "images/projects" {
                0o750
            } else {
                0o755
            }),
        )
        .unwrap();
    }
    let service = Service {
        executable: "/unused".into(),
        admission_failed: AtomicBool::new(false),
        reclamation: Mutex::new(()),
        imports: Mutex::new(BTreeMap::new()),
        project_images: Mutex::new(()),
        config: Config {
            client_uid: 0,
            worker_broker_socket: None,
            worker_broker_config: None,
            resources: None,
            limits: Limits {
                max_volume_bytes: 1 << 30,
                max_logical_bytes: 1 << 30,
                max_journal_bytes: 1 << 30,
                max_cache_bytes: 1 << 30,
            },
            image_roots: vec![root.join("images")],
            project_image_root: Some(root.join("images/projects")),
            local_base_reads: true,
            socket_dir: None,
            root: root.join("state"),
            engine_root: root.join("sandboxes"),
            credentials: root.join("unused"),
            nbd_client: "/usr/sbin/nbd-client".into(),
            devices: vec![],
        },
        entries: Mutex::new(BTreeMap::new()),
        _locks: vec![],
    };
    initialize(&service.config).unwrap();
    let raw = Arc::new(Store::default());
    let source = "a".repeat(64);
    let mut bytes = vec![0; crate::CHUNK_BYTES * 2];
    bytes[1028..1032].copy_from_slice(&32u32.to_le_bytes());
    bytes[1024 + 0x18..1024 + 0x1c].copy_from_slice(&2u32.to_le_bytes());
    bytes[1024 + 0x38..1024 + 0x3a].copy_from_slice(&0xef53u16.to_le_bytes());
    bytes[5000..5004].copy_from_slice(b"data");
    bytes[crate::CHUNK_BYTES + 20..crate::CHUNK_BYTES + 24].copy_from_slice(b"more");
    let mut disk = IndexedVolume::create(raw.clone(), &source, bytes.len() as u64).unwrap();
    disk.write(0, &bytes).unwrap();
    disk.commit().unwrap();
    let head = raw.head(&source).unwrap().unwrap();
    let metadata = metadata();
    let candidate = Candidate {
        schema: 1,
        image_id: metadata.build_id.clone(),
        source_volume_id: source,
        sandbox_dir: service.config.engine_root.join("builder"),
        output_root: root.join("images/projects"),
        source_logical_bytes: bytes.len() as u64,
        logical_bytes: crate::project_image::logical_bytes(bytes.len() as u64).unwrap(),
        metadata,
        source_revision: head.revision,
        source_manifest_sha256: crate::digest(&head.manifest),
        export_sha256: None,
        digest: None,
        base: None,
        file_identity: None,
        local_published: false,
        phase: Phase::Exporting,
        error_code: None,
        remote_retired: false,
        retirement_verified: false,
        cleanup_passes: 0,
        remote_clean: false,
        logical_released: false,
        reclamation_complete: false,
    };
    persist(&service.config, &candidate).unwrap();
    fs::create_dir(&candidate.sandbox_dir).unwrap();
    let mut builder = volume(
        candidate.source_volume_id.clone(),
        candidate.sandbox_dir.clone(),
        root.join("images/base.ext4"),
        candidate.source_logical_bytes,
    );
    builder.image_hash = candidate.metadata.base_content_digest.clone();
    builder.prepared = true;
    builder.evicted = true;
    register(&service, builder);
    Fixture {
        root,
        service: Arc::new(service),
        candidate,
        raw,
        bytes,
    }
}
fn capture(f: &mut Fixture) -> ProjectImageInfo {
    capture_result(f).unwrap()
}
fn capture_result(f: &mut Fixture) -> Result<ProjectImageInfo> {
    let record: Record = read(
        &f.service
            .config
            .root
            .join("volumes")
            .join(&f.candidate.source_volume_id)
            .join("record.json"),
    )?;
    validate_builder_record(
        &record,
        &f.candidate.metadata,
        f.candidate.source_logical_bytes,
    )?;
    let raw = f.raw.clone();
    f.service.capture_project_image_with(
        &mut f.candidate,
        f.raw.clone(),
        move |file, digest, size, bytes, deadline| {
            import_base_file(raw.clone(), file, digest, size, bytes, Some(deadline))
        },
    )
}
fn volume(id: String, sandbox: PathBuf, image: PathBuf, bytes: u64) -> Record {
    Record {
        id,
        sandbox,
        image,
        image_hash: String::new(),
        logical_bytes: bytes,
        device: "/dev/nbd0".into(),
        evicted: false,
        prepared: false,
        deleted: false,
        reclaimed: false,
        logical_released: false,
        compact_after: None,
        compact_revision: None,
        gc_after: None,
        gc_eligible: false,
        gc_last_completed: None,
        desired: false,
        worker: None,
        client: None,
        vm: None,
    }
}
fn register(service: &Service, record: Record) {
    private_dir(&service.dir(&record)).unwrap();
    service.persist(&record).unwrap();
    service.entries.lock().unwrap().insert(
        record.id.clone(),
        Arc::new(Slot::new(Entry {
            record,
            failures: 0,
            mark: None,
            retry_at: Instant::now(),
        })),
    );
}

fn cached_ubuntu(f: &Fixture) -> (Record, Arc<Store>) {
    let image = f.root.join("images/ubuntu-dev.ext4");
    fs::write(&image, &f.bytes).unwrap();
    fs::set_permissions(&image, fs::Permissions::from_mode(0o444)).unwrap();
    let raw = Arc::new(Store::default());
    let mut file = File::open(&image).unwrap();
    let mut bytes = vec![0; crate::indexed::WRITE_LIMIT];
    let hash = f
        .service
        .cached_image_hash(
            &mut file,
            &mut f.service.imports.lock().unwrap(),
            &mut bytes,
        )
        .unwrap();
    import_base_file(
        raw.clone(),
        &mut file,
        &hash,
        f.bytes.len() as u64,
        &mut bytes,
        None,
    )
    .unwrap();
    assert!(IndexedVolume::open(raw.clone(), &hash).is_ok());
    let record = volume(
        "e".repeat(64),
        f.service.config.engine_root.join("ordinary-ubuntu"),
        image,
        f.bytes.len() as u64,
    );
    register(&f.service, record.clone());
    (record, raw)
}

fn prepare_cached_ubuntu(service: &Service, mut record: Record, raw: Arc<Store>) -> Result<Record> {
    let bases = raw.clone();
    service.prepare_with(
        &mut record,
        || Ok(raw),
        |file, digest, size, bytes| import_base_file(bases, file, digest, size, bytes, None),
    )?;
    assert!(record.prepared && !record.image_hash.is_empty());
    Ok(record)
}

#[test]
fn distinct_catalog_and_raw_content_digests_capture_and_wrong_raw_is_rejected() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    assert_ne!(
        f.candidate.metadata.base_image_digest,
        f.candidate.metadata.base_content_digest
    );
    let image = capture_result(&mut f).unwrap();
    assert_eq!(image.metadata.base_image_digest, "b".repeat(64));
    assert_eq!(image.metadata.base_content_digest, "e".repeat(64));
    assert_eq!(
        image.metadata,
        crate::project_image::read_metadata(&mut File::open(image.image_path).unwrap()).unwrap()
    );

    let mut wrong = fixture();
    // A catalog/gzip digest cannot stand in for the raw ext4 SHA persisted by
    // ordinary prepare, even when both digest values are syntactically valid.
    wrong.candidate.metadata.base_content_digest =
        wrong.candidate.metadata.base_image_digest.clone();
    persist(&wrong.service.config, &wrong.candidate).unwrap();
    assert!(capture_result(&mut wrong)
        .unwrap_err()
        .to_string()
        .contains("stopped builder"));
    assert!(!wrong.candidate.temporary().exists());
    assert_eq!(
        load(&wrong.service.config, &wrong.candidate.image_id)
            .unwrap()
            .phase,
        Phase::Exporting
    );
}

#[test]
fn cached_ubuntu_prepare_completes_during_private_upload() {
    if !root_tests() {
        return;
    }
    let f = fixture();
    let (ordinary, ordinary_raw) = cached_ubuntu(&f);
    let (started, receive) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let service = f.service.clone();
    let raw = f.raw.clone();
    let mut candidate = f.candidate.clone();
    let private = thread::spawn(move || {
        let _operation = service.project_images.lock().unwrap();
        service.capture_project_image_with(
            &mut candidate,
            raw.clone(),
            |file, digest, size, bytes, deadline| {
                started.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(10))?;
                import_base_file(raw.clone(), file, digest, size, bytes, Some(deadline))
            },
        )
    });
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    let (done, completed) = mpsc::channel();
    let service = f.service.clone();
    let ordinary = thread::spawn(move || {
        done.send(prepare_cached_ubuntu(&service, ordinary, ordinary_raw))
            .unwrap();
    });
    let result = completed.recv_timeout(Duration::from_secs(2));
    release.send(()).unwrap();
    private.join().unwrap().unwrap();
    ordinary.join().unwrap();
    assert!(result.unwrap().unwrap().prepared);
}

#[derive(Debug)]
struct PausedGc {
    raw: Arc<Store>,
    started: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

#[derive(Debug)]
struct ExportReads {
    raw: Arc<Store>,
    base_data: Vec<String>,
    source: String,
    base_gets: AtomicU64,
    private_gets: AtomicU64,
}
impl ObjectStore for ExportReads {
    fn base_chunk(&self, image: &str, hash: &str, offset: Option<u64>) -> crate::Result<Vec<u8>> {
        if self.base_data.iter().any(|data| data == hash) {
            self.base_gets.fetch_add(1, Ordering::SeqCst);
        }
        self.raw.base_chunk(image, hash, offset)
    }
    fn head(&self, id: &str) -> crate::Result<Option<crate::Head>> {
        self.raw.head(id)
    }
    fn chunk(&self, id: &str, hash: &str) -> crate::Result<Vec<u8>> {
        if id == self.source {
            self.private_gets.fetch_add(1, Ordering::SeqCst);
        }
        self.raw.chunk(id, hash)
    }
    fn put_chunk(&self, id: &str, hash: &str, bytes: &[u8]) -> crate::Result<()> {
        self.raw.put_chunk(id, hash, bytes)
    }
    fn publish(&self, id: &str, expected: Option<&str>, bytes: &[u8]) -> crate::Result<String> {
        self.raw.publish(id, expected, bytes)
    }
}

#[test]
fn capture_verifies_local_inherited_base_and_falls_back_without_losing_private_changes() {
    if !root_tests() {
        return;
    }
    for cache in [
        "valid", "corrupt", "missing", "writable", "owner", "symlink", "fifo",
    ] {
        let mut f = fixture();
        // Separate the inherited block from the sixteen-block private pack
        // rewritten below, so this fixture actually exercises both read paths.
        f.bytes.resize(crate::CHUNK_BYTES * 17, 0);
        f.candidate.source_logical_bytes = f.bytes.len() as u64;
        f.candidate.logical_bytes =
            crate::project_image::logical_bytes(f.bytes.len() as u64).unwrap();
        let image = f.root.join("images/base.ext4");
        fs::write(&image, &f.bytes).unwrap();
        fs::set_permissions(&image, fs::Permissions::from_mode(0o444)).unwrap();
        let raw_digest = crate::digest(&f.bytes);
        assert_ne!(raw_digest, f.candidate.metadata.base_image_digest);
        let mut file = File::open(&image).unwrap();
        let base = import_base_file(
            f.raw.clone(),
            &mut file,
            &raw_digest,
            f.bytes.len() as u64,
            &mut vec![0; crate::indexed::WRITE_LIMIT],
            None,
        )
        .unwrap();
        drop(file);
        let source = "9".repeat(64);
        let mut disk = IndexedVolume::create_from_base(f.raw.clone(), &source, base).unwrap();
        let mut expected = f.bytes.clone();
        let offset = crate::CHUNK_BYTES * 16 + 20;
        expected[offset..offset + 4].copy_from_slice(b"edit");
        disk.write(offset as u64, b"edit").unwrap();
        disk.commit().unwrap();
        OwnedDisk::enroll(f.raw.clone(), &source).unwrap();
        let before = f.raw.head(&source).unwrap().unwrap();
        f.candidate.source_volume_id = source.clone();
        f.candidate.source_revision = before.revision.clone();
        f.candidate.source_manifest_sha256 = crate::digest(&before.manifest);
        f.candidate.metadata.base_content_digest = raw_digest.clone();
        persist(&f.service.config, &f.candidate).unwrap();
        let mut record = volume(
            source.clone(),
            f.candidate.sandbox_dir.clone(),
            image.clone(),
            f.bytes.len() as u64,
        );
        record.image_hash = raw_digest;
        record.prepared = true;
        record.evicted = true;
        register(&f.service, record.clone());
        validate_builder_record(
            &record,
            &f.candidate.metadata,
            f.candidate.source_logical_bytes,
        )
        .unwrap();
        match cache {
            "valid" => (),
            "corrupt" => fs::write(&image, vec![0; f.bytes.len()]).unwrap(),
            "missing" => fs::remove_file(&image).unwrap(),
            "writable" => fs::set_permissions(&image, fs::Permissions::from_mode(0o666)).unwrap(),
            "owner" => std::os::unix::fs::chown(&image, Some(65534), None).unwrap(),
            "symlink" => {
                let target = f.root.join("images/other.ext4");
                fs::rename(&image, &target).unwrap();
                std::os::unix::fs::symlink(&target, &image).unwrap();
            }
            "fifo" => {
                fs::remove_file(&image).unwrap();
                rustix::fs::mkfifoat(rustix::fs::CWD, &image, rustix::fs::Mode::RUSR).unwrap();
            }
            _ => unreachable!(),
        }
        let reads = Arc::new(ExportReads {
            raw: f.raw.clone(),
            base_data: f
                .bytes
                .as_chunks::<{ crate::CHUNK_BYTES }>()
                .0
                .iter()
                .map(|bytes| crate::digest(bytes))
                .collect(),
            source: source.clone(),
            base_gets: AtomicU64::new(0),
            private_gets: AtomicU64::new(0),
        });
        let store = f
            .service
            .capture_store_with(&record, reads.clone())
            .unwrap();
        let raw = f.raw.clone();
        let captured = f
            .service
            .capture_project_image_with(
                &mut f.candidate,
                store,
                |file, digest, size, bytes, deadline| {
                    import_base_file(raw.clone(), file, digest, size, bytes, Some(deadline))
                },
            )
            .unwrap();
        let mut exported = fs::read(&captured.image_path).unwrap();
        assert_eq!(&exported[..expected.len()], expected, "{cache}");
        assert_eq!(crate::digest(&exported), captured.digest, "{cache}");
        exported.truncate(expected.len());
        assert_eq!(
            f.candidate.export_sha256.as_deref(),
            Some(crate::digest(&exported).as_str()),
            "{cache}"
        );
        assert!(
            reads.private_gets.load(Ordering::SeqCst) > 0,
            "private changes must stay remote: {cache}"
        );
        if cache == "valid" {
            assert_eq!(reads.base_gets.load(Ordering::SeqCst), 0);
        } else {
            assert!(
                reads.base_gets.load(Ordering::SeqCst) > 0,
                "fallback: {cache}"
            );
        }
        let after = f.raw.head(&source).unwrap().unwrap();
        assert_eq!(after.revision, before.revision, "{cache}");
        assert_eq!(after.manifest, before.manifest, "{cache}");
    }
}

impl ObjectStore for PausedGc {
    fn head(&self, id: &str) -> crate::Result<Option<crate::Head>> {
        self.raw.head(id)
    }
    fn chunk(&self, id: &str, hash: &str) -> crate::Result<Vec<u8>> {
        self.raw.chunk(id, hash)
    }
    fn put_chunk(&self, id: &str, hash: &str, bytes: &[u8]) -> crate::Result<()> {
        self.raw.put_chunk(id, hash, bytes)
    }
    fn publish(&self, id: &str, expected: Option<&str>, bytes: &[u8]) -> crate::Result<String> {
        self.raw.publish(id, expected, bytes)
    }
    fn list_chunks(&self, id: &str, limit: usize) -> crate::Result<Vec<String>> {
        self.started.send(()).map_err(|_| crate::Error::Store)?;
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| crate::Error::Store)?;
        self.raw.list_chunks(id, limit)
    }
    fn delete_chunk(&self, id: &str, hash: &str) -> crate::Result<()> {
        self.raw.delete_chunk(id, hash)
    }
}

#[test]
fn cached_ubuntu_prepare_completes_during_private_gc_and_alias_stays_fenced() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let image = capture(&mut f);
    let (ordinary, ordinary_raw) = cached_ubuntu(&f);
    let alias = f.root.join("images/operator-alias.ext4");
    fs::copy(&image.image_path, &alias).unwrap();
    fs::set_permissions(&alias, fs::Permissions::from_mode(0o444)).unwrap();
    let (started, receive) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let raw = Arc::new(PausedGc {
        raw: f.raw.clone(),
        started,
        release: Mutex::new(wait),
    });
    let service = f.service.clone();
    let mut candidate = f.candidate.clone();
    let private = thread::spawn(move || {
        let _operation = service.project_images.lock().unwrap();
        service.retire_project_image_with(&mut candidate, || Ok(raw))
    });
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(
        load(&f.service.config, &image.image_id).unwrap().phase,
        Phase::Retiring
    );
    assert!(check_image_admission(&f.service, &image.image_path).is_err());
    let (done, completed) = mpsc::channel();
    let service = f.service.clone();
    let ordinary = thread::spawn(move || {
        let prepared = prepare_cached_ubuntu(&service, ordinary, ordinary_raw);
        let alias = service.warm_base(&alias).unwrap_err().to_string();
        done.send((prepared, alias)).unwrap();
    });
    let result = completed.recv_timeout(Duration::from_secs(2));
    release.send(()).unwrap();
    assert!(!private.join().unwrap().unwrap().reclamation_complete);
    ordinary.join().unwrap();
    let (prepared, alias) = result.unwrap();
    assert!(prepared.unwrap().prepared);
    assert!(alias.contains("private project image is not available"));
}

#[test]
fn export_lock_releases_on_drop_even_with_an_inherited_descriptor() {
    if !root_tests() {
        return;
    }
    let f = fixture();
    let path = f.candidate.temporary();
    let file = open_file(&path, true, true).unwrap();
    let inherited = file.try_clone().unwrap();
    assert!(open_file(&path, false, false).is_err());
    drop(file);
    // A cloned descriptor models the shared open file description inherited at
    // fork. It remains alive, but must not retain the completed export's flock.
    let reopened = open_file(&path, false, false).unwrap();
    drop(inherited);
    assert!(open_file(&path, false, false).is_err());
    reopened.release().unwrap();
    assert!(open_file(&path, false, false).is_ok());
}

#[test]
fn real_ready_indexed_capture_retires_only_after_observed_empty_sweep() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let image = capture(&mut f);
    let head = f.raw.head(&image.digest).unwrap().unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&head.manifest).unwrap()["ready"],
        true
    );
    assert!(IndexedVolume::open(f.raw.clone(), &image.digest).is_ok());
    let raw = f.raw.clone();
    let first = f
        .service
        .retire_project_image_with(&mut f.candidate, || Ok(raw))
        .unwrap();
    assert!(!first.logical_released && !first.reclamation_complete);
    assert_eq!(
        logical_charge(&f.service.config).unwrap(),
        image.logical_bytes
    );
    assert!(image.image_path.exists());
    let mut restarted = load(&f.service.config, &image.image_id).unwrap();
    let raw = f.raw.clone();
    let second = f
        .service
        .retire_project_image_with(&mut restarted, || Ok(raw))
        .unwrap();
    assert!(second.logical_released && second.reclamation_complete);
    assert_eq!(logical_charge(&f.service.config).unwrap(), 0);
    assert!(!image.image_path.exists());
    assert!(
        crate::reclaim::retired(&image.digest, &f.raw.head(&image.digest).unwrap().unwrap())
            .unwrap()
    );
    assert!(f.raw.head(&f.candidate.source_volume_id).unwrap().is_some());
}

#[test]
fn interrupted_append_rebuilds_the_same_frozen_source_and_never_publishes_partial() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    f.candidate.export_sha256 = Some(crate::digest(&f.bytes));
    persist(&f.service.config, &f.candidate).unwrap();
    let mut partial = open_file(&f.candidate.temporary(), true, true).unwrap();
    partial.write_all(&f.bytes).unwrap();
    partial.write_all(b"AHVMPJ01\0\0\0\0\0").unwrap();
    partial.sync_all().unwrap();
    drop(partial);
    assert!(info(&f.candidate).is_err());
    let image = capture(&mut f);
    assert_eq!(image.logical_bytes, f.bytes.len() as u64 + 65536);
    assert_eq!(
        crate::project_image::read_metadata(&mut File::open(&image.image_path).unwrap()).unwrap(),
        f.candidate.metadata
    );
    assert!(!f.candidate.temporary().exists());
}

#[test]
fn incomplete_upload_keeps_its_charge_and_can_retire_after_restart() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let raw = f.raw.clone();
    let failure = capture_bytes(
        &f.service.config,
        &mut f.candidate,
        f.raw.clone(),
        move |file, digest, size, _, _| {
            let mut disk = IndexedVolume::create_import(raw.clone(), digest, size)?;
            file.seek(SeekFrom::Start(0))?;
            let mut first = vec![0; crate::CHUNK_BYTES];
            file.read_exact(&mut first)?;
            disk.write(0, &first)?;
            disk.commit_import()?;
            Err("injected upload interruption".into())
        },
    );
    assert!(failure.is_err());
    let mut restart = load(&f.service.config, &f.candidate.image_id).unwrap();
    assert_eq!(restart.phase, Phase::Importing);
    assert!(restart.digest.is_some());
    assert_eq!(
        logical_charge(&f.service.config).unwrap(),
        restart.logical_bytes
    );
    assert!(info(&restart).is_err());
    let raw = f.raw.clone();
    assert!(
        !f.service
            .retire_project_image_with(&mut restart, || Ok(raw))
            .unwrap()
            .reclamation_complete
    );
    let raw = f.raw.clone();
    assert!(
        f.service
            .retire_project_image_with(&mut restart, || Ok(raw))
            .unwrap()
            .reclamation_complete
    );
    assert_eq!(logical_charge(&f.service.config).unwrap(), 0);
}

#[test]
fn create_only_file_publication_recovers_hardlink_gap_without_replacement() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let raw = f.raw.clone();
    assert!(capture_bytes(
        &f.service.config,
        &mut f.candidate,
        f.raw.clone(),
        |_, _, _, _, _| Err("before upload".into())
    )
    .is_err());
    let digest = f.candidate.digest.clone().unwrap();
    fs::hard_link(f.candidate.path().unwrap(), f.candidate.temporary()).unwrap();
    f.candidate.local_published = false;
    f.candidate.file_identity = None;
    persist(&f.service.config, &f.candidate).unwrap();
    let image = capture_bytes(
        &f.service.config,
        &mut f.candidate,
        f.raw.clone(),
        move |file, digest, size, bytes, deadline| {
            import_base_file(raw.clone(), file, digest, size, bytes, Some(deadline))
        },
    )
    .unwrap();
    assert_eq!(image.digest, digest);
    assert_eq!(fs::metadata(image.image_path).unwrap().nlink(), 1);
    assert!(!f.candidate.temporary().exists());
}

#[test]
fn changed_source_and_corrupt_final_size_fail_closed_and_keep_charge() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let mut source = IndexedVolume::open(f.raw.clone(), &f.candidate.source_volume_id).unwrap();
    source.write(6000, b"changed").unwrap();
    source.commit().unwrap();
    assert!(capture_bytes(
        &f.service.config,
        &mut f.candidate,
        f.raw.clone(),
        |_, _, _, _, _| panic!("changed source must not upload")
    )
    .is_err());
    assert_eq!(
        logical_charge(&f.service.config).unwrap(),
        f.candidate.logical_bytes
    );
    let mut f = fixture();
    let image = capture(&mut f);
    fs::OpenOptions::new()
        .write(true)
        .open(&image.image_path)
        .unwrap()
        .set_len(image.logical_bytes - 1)
        .unwrap();
    assert!(info(&f.candidate).is_err());
    assert!(capture_bytes(
        &f.service.config,
        &mut f.candidate,
        f.raw.clone(),
        |_, _, _, _, _| panic!("corrupt ready artifact must not upload")
    )
    .is_err());
    assert_eq!(
        logical_charge(&f.service.config).unwrap(),
        image.logical_bytes
    );
}

#[test]
fn export_cannot_write_beyond_frozen_source_reservation() {
    if !root_tests() {
        return;
    }
    let f = fixture();
    let mut file = open_file(&f.candidate.temporary(), true, true).unwrap();
    let mut export = SparseExport {
        file: &mut file,
        deadline: Instant::now() + Duration::from_secs(1),
        written: 0,
        limit: crate::CHUNK_BYTES as u64,
    };
    assert!(
        crate::owned::export_remote(f.raw.clone(), &f.candidate.source_volume_id, &mut export)
            .is_err()
    );
    assert!(file.metadata().unwrap().len() <= crate::CHUNK_BYTES as u64);
}

#[test]
fn partial_create_and_deleted_unreclaimed_volume_keep_private_base_pinned() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let image = capture(&mut f);
    let id = "e".repeat(64);
    let mut r = volume(
        id.clone(),
        f.service.config.engine_root.join("consumer"),
        image.image_path.clone(),
        image.logical_bytes,
    );
    register(&f.service, r.clone());
    assert!(f
        .service
        .retire_project_image_with(&mut f.candidate, || panic!(
            "live ref must not touch remote base"
        ))
        .is_err());
    r.deleted = true;
    f.service.persist(&r).unwrap();
    assert!(f
        .service
        .retire_project_image_with(&mut f.candidate, || panic!(
            "deleted incomplete ref must stay pinned"
        ))
        .is_err());
    r.reclaimed = true;
    r.logical_released = true;
    r.evicted = true;
    f.service.persist(&r).unwrap();
    let raw = f.raw.clone();
    assert!(
        !f.service
            .retire_project_image_with(&mut f.candidate, || Ok(raw))
            .unwrap()
            .reclamation_complete
    );
}

#[test]
fn retirement_waits_for_import_without_blocking_admission_and_rechecks_new_ref() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let image = capture(&mut f);
    let service = f.service.clone();
    let mut candidate = f.candidate.clone();
    let imports = service.imports.lock().unwrap();
    let (started, receive) = mpsc::channel();
    let retired = service.clone();
    let thread = thread::spawn(move || {
        started.send(()).unwrap();
        retired.retire_project_image_with(&mut candidate, || {
            panic!("new reference must prevent remote retirement")
        })
    });
    receive.recv_timeout(Duration::from_secs(1)).unwrap();
    // Unlike the old entries->imports ordering, the registry remains available.
    assert!(service.entries.try_lock().is_ok());
    let r = volume(
        "e".repeat(64),
        service.config.engine_root.join("consumer"),
        image.image_path,
        image.logical_bytes,
    );
    register(&service, r);
    drop(imports);
    assert!(thread
        .join()
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("still referenced"));
    assert_eq!(
        load(&service.config, &image.image_id).unwrap().phase,
        Phase::Ready
    );
}

#[test]
fn retiring_barrier_blocks_normal_admission_and_delayed_alias_prewarm() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let image = capture(&mut f);
    let alias = f.root.join("images/operator-alias.ext4");
    fs::copy(&image.image_path, &alias).unwrap();
    fs::set_permissions(&alias, fs::Permissions::from_mode(0o444)).unwrap();
    let service = f.service.clone();
    let imports = service.imports.lock().unwrap();
    let warm = service.clone();
    let thread = thread::spawn(move || warm.warm_base(&alias));
    f.candidate.phase = Phase::Retiring;
    persist(&service.config, &f.candidate).unwrap();
    assert!(check_image_admission(&service, &image.image_path).is_err());
    drop(imports);
    assert!(thread
        .join()
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("private project image is not available"));
}

#[test]
fn each_new_image_uploads_all_nonzero_source_blocks_in_its_own_namespace() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let first = capture(&mut f);
    let mut second = f.candidate.clone();
    second.image_id = "44444444-4444-4444-4444-444444444444".into();
    second.metadata.build_id = second.image_id.clone();
    second.phase = Phase::Exporting;
    second.digest = None;
    second.export_sha256 = None;
    second.base = None;
    second.file_identity = None;
    second.local_published = false;
    persist(&f.service.config, &second).unwrap();
    let raw = f.raw.clone();
    let next = capture_bytes(
        &f.service.config,
        &mut second,
        f.raw.clone(),
        move |file, digest, size, bytes, deadline| {
            import_base_file(raw.clone(), file, digest, size, bytes, Some(deadline))
        },
    )
    .unwrap();
    assert_ne!(first.digest, next.digest);
    for part in f.bytes.as_chunks::<{ crate::CHUNK_BYTES }>().0 {
        let hash = crate::digest(part);
        assert_eq!(f.raw.chunk(&first.digest, &hash).unwrap(), part);
        assert_eq!(f.raw.chunk(&next.digest, &hash).unwrap(), part);
    }
}

#[test]
fn headroom_includes_pending_exports_but_not_completed_local_file_again() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    assert_eq!(
        pending_bytes(&f.service.config, None).unwrap(),
        f.candidate.logical_bytes
    );
    capture(&mut f);
    assert_eq!(pending_bytes(&f.service.config, None).unwrap(), 0);
    assert_eq!(
        logical_charge(&f.service.config).unwrap(),
        f.candidate.logical_bytes
    );
    assert!(available(&f.service.config).unwrap() > 0);
}

#[test]
fn caller_cannot_retire_an_operator_base_by_supplying_its_sha() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let image = capture(&mut f);
    let operator = "e".repeat(64);
    let mut base =
        IndexedVolume::create_import(f.raw.clone(), &operator, f.bytes.len() as u64).unwrap();
    base.write(0, &f.bytes).unwrap();
    base.finish_import().unwrap();
    let before = f.raw.head(&operator).unwrap().unwrap();
    let request = Request {
        version: 1,
        operation: "project-image-retire".into(),
        volume_id: f.candidate.source_volume_id.clone(),
        sandbox_dir: f.candidate.sandbox_dir.clone(),
        image: None,
        logical_bytes: None,
        project_image: Some(ProjectImageRequest {
            image_id: image.image_id,
            metadata: None,
            source_logical_bytes: None,
            expected_digest: Some(operator.clone()),
        }),
    };
    assert!(f
        .service
        .project_image_request(request)
        .unwrap_err()
        .to_string()
        .contains("intent changed"));
    let after = f.raw.head(&operator).unwrap().unwrap();
    assert_eq!(before.revision, after.revision);
    assert_eq!(before.manifest, after.manifest);
    assert_eq!(
        load(&f.service.config, &f.candidate.image_id)
            .unwrap()
            .phase,
        Phase::Ready
    );
}

#[test]
fn reloaded_reclaimed_builder_record_still_binds_image_status_and_retirement() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let image = capture(&mut f);
    let path = f
        .service
        .config
        .root
        .join("volumes")
        .join(&f.candidate.source_volume_id)
        .join("record.json");
    let mut source = Record::load(&path).unwrap();
    source.deleted = true;
    source.reclaimed = true;
    source.logical_released = true;
    source.evicted = true;
    f.service.persist(&source).unwrap();
    fs::remove_dir_all(&source.sandbox).unwrap();
    // The supervisor's restart adopts its durable tombstone, not a live sandbox.
    f.service.entries.lock().unwrap().clear();
    register(&f.service, Record::load(&path).unwrap());
    let request = Request {
        version: 1,
        operation: "project-image-status".into(),
        volume_id: source.id,
        sandbox_dir: source.sandbox,
        image: None,
        logical_bytes: None,
        project_image: Some(ProjectImageRequest {
            image_id: image.image_id.clone(),
            metadata: None,
            source_logical_bytes: None,
            expected_digest: Some(image.digest.clone()),
        }),
    };
    let reply = f.service.project_image_request(request).unwrap();
    assert_eq!(reply["project_image"]["digest"], image.digest);
    let raw = f.raw.clone();
    assert!(
        !f.service
            .retire_project_image_with(&mut f.candidate, || Ok(raw))
            .unwrap()
            .reclamation_complete
    );
    let raw = f.raw.clone();
    assert!(
        f.service
            .retire_project_image_with(&mut f.candidate, || Ok(raw))
            .unwrap()
            .reclamation_complete
    );
}

fn image_request(f: &Fixture, operation: &str) -> Request {
    Request {
        version: 1,
        operation: operation.into(),
        volume_id: f.candidate.source_volume_id.clone(),
        sandbox_dir: f.candidate.sandbox_dir.clone(),
        image: None,
        logical_bytes: None,
        project_image: Some(ProjectImageRequest {
            image_id: f.candidate.image_id.clone(),
            metadata: if operation == "project-image-capture" {
                Some(f.candidate.metadata.clone())
            } else {
                None
            },
            source_logical_bytes: if operation == "project-image-capture" {
                Some(f.candidate.source_logical_bytes)
            } else {
                None
            },
            expected_digest: None,
        }),
    }
}

#[test]
fn absent_status_is_not_failure_proof_and_definite_early_rejection_is_sealed() {
    if !root_tests() {
        return;
    }
    let f = fixture();
    fs::remove_file(candidate_path(&f.service.config, &f.candidate.image_id)).unwrap();
    let absent = f
        .service
        .project_image_request(image_request(&f, "project-image-status"))
        .unwrap();
    assert_eq!(absent["ok"], true);
    assert_eq!(absent["project_image"], serde_json::Value::Null);
    assert_eq!(absent["project_image_phase"], "absent");
    assert_eq!(absent["project_image_quiescent"], false);
    // Builder sandbox metadata is intentionally absent: rejection precedes any
    // export/remote write and must still make a permanent failed-intent proof.
    let rejected = f
        .service
        .project_image_request(image_request(&f, "project-image-capture"))
        .unwrap();
    assert_eq!(rejected["project_image_phase"], "failed");
    assert_eq!(rejected["project_image_quiescent"], true);
    assert!(!candidate_path(&f.service.config, &f.candidate.image_id).exists());
    assert_eq!(logical_charge(&f.service.config).unwrap(), 0);
    let replay = f
        .service
        .project_image_request(image_request(&f, "project-image-capture"))
        .unwrap();
    assert_eq!(replay["project_image_phase"], "failed");
    let status = f
        .service
        .project_image_request(image_request(&f, "project-image-status"))
        .unwrap();
    assert_eq!(status["project_image_phase"], "failed");
    assert_eq!(status["project_image_quiescent"], true);
    assert_eq!(
        f.service
            .project_image_request(image_request(&f, "project-image-retire"))
            .unwrap()["reclamation_complete"],
        true
    );
    assert_eq!(
        f.service
            .project_image_request(image_request(&f, "project-image-capture"))
            .unwrap()["project_image_phase"],
        "retired"
    );
}

#[test]
fn live_capture_status_does_not_seal_but_supervisor_restart_seals_incomplete_charge() {
    if !root_tests() {
        return;
    }
    let f = fixture();
    let active = intent_lock(&f.service.config, &f.candidate.image_id).unwrap();
    let live = f
        .service
        .project_image_request(image_request(&f, "project-image-status"))
        .unwrap();
    assert_eq!(live["project_image_phase"], "capturing");
    assert_eq!(live["project_image_quiescent"], false);
    assert_eq!(
        load(&f.service.config, &f.candidate.image_id)
            .unwrap()
            .phase,
        Phase::Exporting
    );
    drop(active);
    let restarted = f
        .service
        .project_image_request(image_request(&f, "project-image-status"))
        .unwrap();
    assert_eq!(restarted["project_image_phase"], "failed");
    assert_eq!(restarted["project_image_quiescent"], true);
    assert_eq!(
        logical_charge(&f.service.config).unwrap(),
        f.candidate.logical_bytes
    );
    assert_eq!(
        f.service
            .project_image_request(image_request(&f, "project-image-capture"))
            .unwrap()["project_image_phase"],
        "failed"
    );
    assert_eq!(
        load(&f.service.config, &f.candidate.image_id)
            .unwrap()
            .phase,
        Phase::Failed
    );
}

#[test]
fn unrelated_capture_contention_is_transient_and_never_seals_a_fresh_build() {
    if !root_tests() {
        return;
    }
    let f = fixture();
    fs::remove_file(candidate_path(&f.service.config, &f.candidate.image_id)).unwrap();
    let busy = f.service.project_images.lock().unwrap();
    let reply = f
        .service
        .project_image_request(image_request(&f, "project-image-capture"))
        .unwrap();
    assert_eq!(reply["project_image_phase"], "capturing");
    assert_eq!(reply["project_image_quiescent"], false);
    assert!(!intent_path(&f.service.config, &f.candidate.image_id).exists());
    assert!(!candidate_path(&f.service.config, &f.candidate.image_id).exists());
    drop(busy);
    assert_eq!(
        f.service
            .project_image_request(image_request(&f, "project-image-status"))
            .unwrap()["project_image_phase"],
        "absent"
    );
}

#[test]
fn never_started_retirement_seals_late_capture_without_a_live_builder_record() {
    if !root_tests() {
        return;
    }
    let f = fixture();
    fs::remove_file(candidate_path(&f.service.config, &f.candidate.image_id)).unwrap();
    f.service.entries.lock().unwrap().clear();
    fs::remove_dir_all(f.service.config.root.join("volumes")).unwrap();
    let retired = f
        .service
        .project_image_request(image_request(&f, "project-image-retire"))
        .unwrap();
    assert_eq!(retired["project_image_phase"], "retired");
    assert_eq!(retired["project_image_quiescent"], true);
    assert_eq!(retired["logical_released"], true);
    assert_eq!(retired["reclamation_complete"], true);
    let late = f
        .service
        .project_image_request(image_request(&f, "project-image-capture"))
        .unwrap();
    assert_eq!(late["project_image_phase"], "retired");
    assert_eq!(late["project_image_quiescent"], true);
    assert!(!candidate_path(&f.service.config, &f.candidate.image_id).exists());
    assert_eq!(logical_charge(&f.service.config).unwrap(), 0);
}

#[test]
fn daemon_identity_must_be_disjoint_from_broker_workers_and_private_parent_is_required() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let config = f.root.join("broker.json");
    let value = serde_json::json!({"socket":"/run/synthetic-broker.sock","daemon_uid":65534,"daemon_gid":65534,"uid_base":65536,"gid_base":65536,"identity_count":100});
    save(&config, &value).unwrap();
    let service = Arc::get_mut(&mut f.service).unwrap();
    service.config.client_uid = 65534;
    service.config.worker_broker_socket = Some("/run/synthetic-broker.sock".into());
    service.config.worker_broker_config = Some(config.clone());
    std::os::unix::fs::chown(
        f.service.config.project_image_root.as_ref().unwrap(),
        Some(0),
        Some(65534),
    )
    .unwrap();
    assert!(configured_root(&f.service.config).is_ok());
    Arc::get_mut(&mut f.service).unwrap().config.client_uid = 65537;
    let mut overlapping = value.clone();
    overlapping["daemon_uid"] = 65537.into();
    save(&config, &overlapping).unwrap();
    assert!(configured_root(&f.service.config).is_err());
    Arc::get_mut(&mut f.service).unwrap().config.client_uid = 65534;
    save(&config, &value).unwrap();
    fs::set_permissions(
        f.service.config.project_image_root.as_ref().unwrap(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(configured_root(&f.service.config).is_err());
}

#[test]
fn private_parent_blocks_sibling_worker_but_allows_trusted_daemon_read() {
    if !root_tests() {
        return;
    }
    let mut f = fixture();
    let image = capture(&mut f);
    std::os::unix::fs::chown(
        f.service.config.project_image_root.as_ref().unwrap(),
        Some(0),
        Some(65534),
    )
    .unwrap();
    let read = |uid: &str, gid: &str| {
        Command::new("setpriv")
            .args(["--reuid", uid, "--regid", gid, "--clear-groups", "cat"])
            .arg(&image.image_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
    };
    assert!(read("65534", "65534").success());
    assert!(!read("65537", "65537").success());
    assert_eq!(
        fs::metadata(&image.image_path).unwrap().mode() & 0o777,
        0o444
    );
}
