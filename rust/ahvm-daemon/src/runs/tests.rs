use super::*;
use crate::thermal::{sweep_once, ActivityTracker, ThermalConfig};
use ahvm_engine::*;
use std::sync::{
    atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
    Arc,
};

#[derive(Debug)]
struct Fake {
    inner: MockBackend,
    running: AtomicBool,
    exit_code: AtomicI32,
    lose_launch_reply: AtomicBool,
    fail_stop: AtomicBool,
    ignore_stop: AtomicBool,
    launches: AtomicUsize,
    stops: AtomicUsize,
    starts: AtomicUsize,
    receipt_pinning: AtomicBool,
    boot_reads: AtomicUsize,
    session_lists: AtomicUsize,
    session_running: std::sync::Mutex<std::collections::HashMap<String, bool>>,
}
macro_rules! delegate {
    ($(fn $name:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {$ (
        fn $name(&self, $($arg:$ty),*) -> $out { self.inner.$name($($arg),*) }
    )*};
}
impl Backend for Fake {
    fn session_receipt_pinning(&self, _id: &str) -> ahvm_engine::Result<bool> {
        Ok(self.receipt_pinning.load(Ordering::SeqCst))
    }
    delegate! {
        fn create(spec:&SandboxSpec)->ahvm_engine::Result<SandboxInfo>;
        fn destroy(id:&str)->ahvm_engine::Result<()>;
        fn resume_paused(id:&str)->ahvm_engine::Result<Option<SandboxInfo>>;
        fn supports_pause(id:&str)->bool;
        fn pause(id:&str)->ahvm_engine::Result<()>;
        fn list()->ahvm_engine::Result<Vec<SandboxInfo>>;
        fn exec(id:&str,argv:&[String])->ahvm_engine::Result<ExecResult>;
        fn create_snapshot(id:&str,snapshot_id:&str)->ahvm_engine::Result<SnapshotManifest>;
        fn restore(snapshot:&SnapshotManifest,new_id:&str)->ahvm_engine::Result<SandboxInfo>;
        fn fork(id:&str,new_id:&str)->ahvm_engine::Result<SandboxInfo>;
        fn capabilities()->Capabilities;
        fn snapshot_manifest(snapshot_id:&str)->ahvm_engine::Result<SnapshotManifest>;
        fn file_write(id:&str,path:&str,data:&[u8])->ahvm_engine::Result<u64>;
        fn file_list(id:&str,path:&str,offset:u64,limit:u64)->ahvm_engine::Result<DirListing>;
        fn session_input(id:&str,sid:&str,data:&[u8])->ahvm_engine::Result<u64>;
        fn session_delete(id:&str,sid:&str)->ahvm_engine::Result<()>;
        fn session_resize(id:&str,sid:&str,rows:u16,cols:u16)->ahvm_engine::Result<()>;
    }
    fn file_read(
        &self,
        id: &str,
        path: &str,
        offset: u64,
        limit: u64,
    ) -> ahvm_engine::Result<FileChunk> {
        if path == "/proc/sys/kernel/random/boot_id" {
            self.boot_reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.file_read(id, path, offset, limit)
    }
    fn status(&self, id: &str) -> ahvm_engine::Result<SandboxInfo> {
        let mut v = self.inner.status(id)?;
        v.storage.mode = StorageMode::Replicated;
        Ok(v)
    }
    fn start(&self, id: &str) -> ahvm_engine::Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.inner.start(id)
    }
    fn stop(&self, id: &str) -> ahvm_engine::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.fail_stop.load(Ordering::SeqCst) {
            return Err(Error::Control("sync unavailable".into()));
        }
        if self.ignore_stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.inner.stop(id)
    }
    fn session_create(&self, id: &str, argv: &[String], pty: bool) -> ahvm_engine::Result<String> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        let sid = self.inner.session_create(id, argv, pty)?;
        if self.lose_launch_reply.load(Ordering::SeqCst) {
            return Err(Error::Control("reply lost after spawn".into()));
        }
        Ok(sid)
    }
    fn session_list(&self, id: &str) -> ahvm_engine::Result<Vec<SessionInfo>> {
        self.session_lists.fetch_add(1, Ordering::SeqCst);
        let mut sessions = self.inner.session_list(id)?;
        for s in &mut sessions {
            s.running = self
                .session_running
                .lock()
                .unwrap()
                .get(&s.id)
                .copied()
                .unwrap_or_else(|| self.running.load(Ordering::SeqCst));
        }
        Ok(sessions)
    }
    fn session_read(
        &self,
        id: &str,
        sid: &str,
        seq: u64,
        budget: Duration,
    ) -> ahvm_engine::Result<SessionChunk> {
        let mut chunk = self.inner.session_read(id, sid, seq, budget)?;
        chunk.exit_code = Some(self.exit_code.load(Ordering::SeqCst));
        Ok(chunk)
    }
    fn session_kill(&self, _id: &str, _sid: &str) -> ahvm_engine::Result<()> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }
}
fn setup() -> (AppState, Arc<Fake>) {
    let store = Arc::new(ahvm_store::Store::open_in_memory().unwrap());
    crate::auth::ensure_admin(&store, "test-managed-token").unwrap();
    let fake = Arc::new(Fake {
        inner: MockBackend::new(std::env::temp_dir().join("ahvm-managed-tests")),
        running: AtomicBool::new(true),
        exit_code: AtomicI32::new(0),
        lose_launch_reply: AtomicBool::new(false),
        fail_stop: AtomicBool::new(false),
        ignore_stop: AtomicBool::new(false),
        launches: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
        starts: AtomicUsize::new(0),
        receipt_pinning: AtomicBool::new(true),
        boot_reads: AtomicUsize::new(0),
        session_lists: AtomicUsize::new(0),
        session_running: Default::default(),
    });
    fake.create(&SandboxSpec {
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
    fake.file_write(
        "vm",
        "/proc/sys/kernel/random/boot_id",
        b"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\n",
    )
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
    let state = AppState {
        private_owners: Default::default(),
        store,
        backend: fake.clone(),
        activity: ActivityTracker::new(),
        quotas: crate::quotas::Registry::new(),
        ops: crate::scheduler::OpsLimiter::new(4),
        lifecycle: crate::scheduler::LifecycleLocks::new(),
    };
    state.activity.set_pause_after_secs(30);
    (state, fake)
}
fn request() -> RunRequest {
    RunRequest {
        sandbox_id: "vm".into(),
        argv: vec!["/bin/sleep".into(), "120".into()],
        max_runtime_secs: 3600,
        fence_on_failure: false,
        session_isolated: false,
    }
}
fn admit(state: &AppState) -> ManagedRun {
    let now = unix_now();
    let (run, _) = state
        .store
        .admit_managed_run(
            "job",
            "vm",
            "admin",
            &serde_json::to_string(&request()).unwrap(),
            now,
            now + 3600,
        )
        .unwrap();
    assert!(state.activity.set_managed_run("vm", "job", run.epoch));
    run
}
#[tokio::test]
async fn quiet_job_survives_idle_then_finishes_before_five_minute_stop() {
    let (s, fake) = setup();
    let r = admit(&s);
    assert!(!step(&s, &r.id, r.epoch, true).unwrap());
    s.activity
        .touch_at("vm", Instant::now() - Duration::from_secs(7200));
    let result = sweep_once(&s, ThermalConfig::default(), Instant::now()).await;
    assert_eq!(result.paused + result.stopped, 0);
    fake.running.store(false, Ordering::SeqCst);
    assert!(step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "succeeded");
    assert!(!s.activity.has_managed_run("vm"));
    let now = Instant::now();
    s.activity.touch_at("vm", now - Duration::from_secs(299));
    assert_eq!(
        sweep_once(&s, ThermalConfig::default(), now).await.stopped,
        0
    );
    s.activity.touch_at("vm", now - Duration::from_secs(301));
    assert_eq!(
        sweep_once(&s, ThermalConfig::default(), now).await.stopped,
        1
    );
    assert!(s.store.get_sandbox("vm").is_ok());
}
#[test]
fn lost_launch_reply_is_adopted_without_duplicate_execution() {
    let (s, f) = setup();
    f.lose_launch_reply.store(true, Ordering::SeqCst);
    let r = admit(&s);
    assert!(!step(&s, &r.id, r.epoch, true).unwrap());
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "uncertain");
    let r = s.store.claim_managed_run("job", unix_now()).unwrap();
    s.activity.set_managed_run("vm", "job", r.epoch);
    assert!(!step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "running");
    assert_eq!(f.launches.load(Ordering::SeqCst), 1);
    assert!(step(&s, &r.id, r.epoch - 1, true).unwrap());
    assert_eq!(f.launches.load(Ordering::SeqCst), 1);
}
#[test]
fn reboot_interrupts_instead_of_reusing_native_session_identity() {
    let (s, f) = setup();
    let r = admit(&s);
    step(&s, &r.id, r.epoch, true).unwrap();
    f.file_write(
        "vm",
        "/proc/sys/kernel/random/boot_id",
        b"bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
    )
    .unwrap();
    assert!(step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "interrupted");
    assert_eq!(f.launches.load(Ordering::SeqCst), 1);
}
#[test]
fn crash_before_dispatch_does_not_launch_on_recovery() {
    let (s, f) = setup();
    let r = admit(&s);
    assert!(step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(f.launches.load(Ordering::SeqCst), 0);
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "interrupted");
}
#[test]
fn ambiguous_lost_session_keeps_hold_until_verified_cold_stop() {
    let (s, f) = setup();
    let r = admit(&s);
    step(&s, &r.id, r.epoch, true).unwrap();
    let r = s.store.get_managed_run("job").unwrap();
    f.session_delete("vm", r.session_id.as_deref().unwrap())
        .unwrap();
    let r = s
        .store
        .update_managed_run(
            "job",
            r.epoch,
            "uncertain",
            None,
            None,
            None,
            Some("unknown"),
            unix_now() - 61,
            false,
        )
        .unwrap();
    f.fail_stop.store(true, Ordering::SeqCst);
    assert!(step(&s, &r.id, r.epoch, false).is_err());
    assert!(s.activity.has_managed_run("vm"));
    assert!(s
        .store
        .get_managed_run("job")
        .unwrap()
        .finished_at
        .is_none());
    f.fail_stop.store(false, Ordering::SeqCst);
    assert!(step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(f.status("vm").unwrap().state, VmState::Stopped);
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "interrupted");
    assert!(!s.activity.has_managed_run("vm"));
}
#[test]
fn deadline_cancels_and_waits_for_observed_exit() {
    let (s, f) = setup();
    let r = admit(&s);
    step(&s, &r.id, r.epoch, true).unwrap();
    let r = s
        .store
        .update_managed_run(
            "job",
            r.epoch,
            "cancelling",
            None,
            None,
            None,
            Some("timeout"),
            unix_now(),
            false,
        )
        .unwrap();
    assert!(!step(&s, &r.id, r.epoch, false).unwrap());
    assert!(s.activity.has_managed_run("vm"));
    assert!(!f.running.load(Ordering::SeqCst));
    assert!(step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "interrupted");
}
#[tokio::test]
async fn submit_is_admin_only_and_retries_do_not_launch_again() {
    let (s, f) = setup();
    assert!(matches!(
        submit(
            State(s.clone()),
            Extension(UserId("alice".into())),
            Path("job".into()),
            Json(request())
        )
        .await,
        Err(ApiError::Forbidden(_))
    ));
    let Json(first) = submit(
        State(s.clone()),
        Extension(UserId("admin".into())),
        Path("job".into()),
        Json(request()),
    )
    .await
    .unwrap();
    let Json(second) = submit(
        State(s.clone()),
        Extension(UserId("admin".into())),
        Path("job".into()),
        Json(request()),
    )
    .await
    .unwrap();
    assert_eq!(first.id, second.id);
    let mut other = request();
    other.argv.push("oops".into());
    assert!(matches!(
        submit(
            State(s.clone()),
            Extension(UserId("admin".into())),
            Path("job".into()),
            Json(other)
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert!(check_lifecycle(&s, "vm").is_err());
    // The request ended without a WebSocket attachment; the node owns the job.
    for _ in 0..50 {
        if f.launches.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(f.launches.load(Ordering::SeqCst), 1);
    assert!(s.activity.has_managed_run("vm"));
}
#[tokio::test]
async fn recovery_reinstalls_protection_before_background_tasks_run() {
    let (mut s, _) = setup();
    let r = admit(&s);
    step(&s, &r.id, r.epoch, true).unwrap();
    s.activity = ActivityTracker::new();
    recover(&s).unwrap();
    assert!(s.activity.has_managed_run("vm"));
    assert_eq!(s.store.get_managed_run("job").unwrap().epoch, 2);
}

#[test]
fn deadline_before_dispatch_never_starts_expired_work() {
    let (s, f) = setup();
    let now = unix_now();
    let (r, _) = s
        .store
        .admit_managed_run(
            "job",
            "vm",
            "admin",
            &serde_json::to_string(&request()).unwrap(),
            now - 10,
            now - 1,
        )
        .unwrap();
    s.activity.set_managed_run("vm", "job", r.epoch);
    assert!(step(&s, &r.id, r.epoch, true).unwrap());
    assert_eq!(f.launches.load(Ordering::SeqCst), 0);
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "interrupted");
}

#[test]
fn runtime_deadline_cancels_an_already_started_session() {
    let (s, f) = setup();
    let now = unix_now();
    let (r, _) = s
        .store
        .admit_managed_run(
            "job",
            "vm",
            "admin",
            &serde_json::to_string(&request()).unwrap(),
            now - 10,
            now - 1,
        )
        .unwrap();
    s.activity.set_managed_run("vm", "job", r.epoch);
    let sid = f
        .session_create("vm", &command(&r).unwrap(), false)
        .unwrap();
    s.store
        .update_managed_run(
            "job",
            r.epoch,
            "running",
            Some(&sid),
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            None,
            None,
            now - 5,
            false,
        )
        .unwrap();
    assert!(!step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "cancelling");
    assert!(s.activity.has_managed_run("vm"));
    assert!(step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "interrupted");
}

fn admit_fenced(state: &AppState, expired: bool) -> ManagedRun {
    let now = unix_now();
    let mut body = request();
    body.fence_on_failure = true;
    let (run, _) = state
        .store
        .admit_managed_run(
            "job",
            "vm",
            "admin",
            &serde_json::to_string(&body).unwrap(),
            now - 10,
            if expired { now - 1 } else { now + 3600 },
        )
        .unwrap();
    state.activity.set_managed_run("vm", "job", run.epoch);
    let sid = state
        .backend
        .session_create("vm", &command(&run).unwrap(), false)
        .unwrap();
    state
        .store
        .update_managed_run(
            "job",
            run.epoch,
            "running",
            Some(&sid),
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            None,
            None,
            now - 5,
            false,
        )
        .unwrap()
}

#[test]
fn omitted_or_false_fencing_preserves_canonical_receipts() {
    let expected = r#"{"sandbox_id":"vm","argv":["/bin/sleep","120"],"max_runtime_secs":3600}"#;
    let body: RunRequest = serde_json::from_str(expected).unwrap();
    assert!(!body.fence_on_failure);
    assert_eq!(serde_json::to_string(&body).unwrap(), expected);
    let explicit = expected.replace("3600}", "3600,\"fence_on_failure\":false}");
    assert_eq!(
        serde_json::to_string(&serde_json::from_str::<RunRequest>(&explicit).unwrap()).unwrap(),
        expected
    );
}

#[test]
fn fenced_outcomes_stop_only_failed_or_interrupted_controllers() {
    // Includes the deadline race: exit observed before cancellation was saved.
    for (code, cancel, expired, phase, stopped) in [
        (0, false, false, "succeeded", false),
        (23, false, false, "failed", true),
        (0, true, false, "interrupted", true),
        (0, false, true, "interrupted", true),
    ] {
        let (s, f) = setup();
        let r = admit_fenced(&s, expired);
        if cancel {
            update(
                &s,
                &r,
                "cancelling",
                None,
                None,
                None,
                Some("cancellation requested"),
                false,
            )
            .unwrap();
        }
        f.running.store(false, Ordering::SeqCst);
        f.exit_code.store(code, Ordering::SeqCst);
        assert!(step(&s, &r.id, r.epoch, false).unwrap());
        let result = s.store.get_managed_run("job").unwrap();
        assert_eq!(result.phase, phase);
        assert_eq!(result.exit_code, Some(code));
        assert_eq!(f.status("vm").unwrap().state == VmState::Stopped, stopped);
        assert_eq!(f.stops.load(Ordering::SeqCst), usize::from(stopped));
        assert!(!s.activity.has_managed_run("vm"));
        if !stopped {
            assert_eq!(s.store.list_managed_run_cooldowns().unwrap().len(), 1);
        }
    }
}

#[test]
fn fenced_failed_stop_retains_result_and_hold_until_retry() {
    let (s, f) = setup();
    let r = admit_fenced(&s, false);
    f.running.store(false, Ordering::SeqCst);
    f.exit_code.store(23, Ordering::SeqCst);
    f.fail_stop.store(true, Ordering::SeqCst);
    assert!(step(&s, &r.id, r.epoch, false).is_err());
    let pending = s.store.get_managed_run("job").unwrap();
    assert!(pending.finished_at.is_none());
    assert_eq!(pending.exit_code, Some(23));
    assert!(s.activity.has_managed_run("vm"));
    // Retry/recovery needs no native session to finish the durable fence.
    f.session_delete("vm", pending.session_id.as_deref().unwrap())
        .unwrap();
    let claimed = s.store.claim_managed_run("job", unix_now()).unwrap();
    s.activity.set_managed_run("vm", "job", claimed.epoch);
    f.fail_stop.store(false, Ordering::SeqCst);
    assert!(step(&s, &claimed.id, claimed.epoch, false).unwrap());
    assert_eq!(f.status("vm").unwrap().state, VmState::Stopped);
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "failed");
    assert!(!s.activity.has_managed_run("vm"));
}

#[test]
fn fenced_stop_acknowledgement_requires_verified_stopped_state() {
    let (s, f) = setup();
    let r = admit_fenced(&s, false);
    f.running.store(false, Ordering::SeqCst);
    f.exit_code.store(1, Ordering::SeqCst);
    f.ignore_stop.store(true, Ordering::SeqCst);
    assert!(step(&s, &r.id, r.epoch, false).is_err());
    assert!(s.activity.has_managed_run("vm"));
    assert!(s
        .store
        .get_managed_run("job")
        .unwrap()
        .finished_at
        .is_none());
    f.ignore_stop.store(false, Ordering::SeqCst);
    assert!(step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(f.status("vm").unwrap().state, VmState::Stopped);
}

#[tokio::test]
async fn repeated_cancel_preserves_pending_fence_result() {
    let (s, f) = setup();
    let r = admit_fenced(&s, false);
    f.running.store(false, Ordering::SeqCst);
    f.exit_code.store(23, Ordering::SeqCst);
    f.fail_stop.store(true, Ordering::SeqCst);
    assert!(step(&s, &r.id, r.epoch, false).is_err());
    let Json(receipt) = cancel(
        State(s.clone()),
        Extension(UserId("admin".into())),
        Path(r.id.clone()),
    )
    .await
    .unwrap();
    assert_eq!(receipt.exit_code, Some(23));
    f.fail_stop.store(false, Ordering::SeqCst);
    assert!(step(&s, &r.id, r.epoch, false).unwrap());
    assert_eq!(s.store.get_managed_run("job").unwrap().phase, "failed");
}

#[tokio::test]
async fn deleting_storage_refuses_new_managed_run_and_persisted_run_blocks_reuse() {
    let (state, fake) = setup();
    let reservation = state
        .store
        .reserve_replicated_volume("admin", "vm", &"d".repeat(64), 65536, unix_now())
        .unwrap();
    state
        .store
        .delete_replicated_reservation("admin", &reservation.volume_id, unix_now())
        .unwrap();
    let result = submit(
        State(state.clone()),
        Extension(UserId("admin".into())),
        Path("rejected".into()),
        Json(request()),
    )
    .await;
    assert!(matches!(result, Err(ApiError::Conflict(_))));
    assert_eq!(fake.launches.load(Ordering::SeqCst), 0);
    assert!(state.store.managed_run_for_sandbox("vm").unwrap().is_none());

    // A persisted controller is authoritative even before activity recovery.
    state
        .store
        .admit_managed_run(
            "persisted",
            "vm",
            "admin",
            &serde_json::to_string(&request()).unwrap(),
            unix_now(),
            unix_now() + 3600,
        )
        .unwrap();
    assert!(!state.activity.in_flight("vm"));
    assert!(matches!(
        crate::routes::identity_available(&state, "vm"),
        Err(ApiError::Conflict(_))
    ));
    assert!(check_lifecycle(&state, "vm").is_err());
}

fn admit_isolated(state: &AppState, id: &str) -> ManagedRun {
    let mut body = request();
    body.session_isolated = true;
    let now = unix_now();
    let (run, _) = state
        .store
        .admit_managed_run(
            id,
            "vm",
            "admin",
            &serde_json::to_string(&body).unwrap(),
            now,
            now + 3600,
        )
        .unwrap();
    assert!(state.activity.set_managed_run("vm", id, run.epoch));
    run
}

#[tokio::test]
async fn concurrent_cancel_waits_for_receipt_and_keeps_sibling_awake() {
    let (s, fake) = setup();
    let runs: Vec<_> = (0..3)
        .map(|i| admit_isolated(&s, &format!("chat{i}")))
        .collect();
    for run in &runs {
        assert!(!step(&s, &run.id, run.epoch, true).unwrap());
    }
    let cancelled = s.store.get_managed_run("chat0").unwrap();
    s.store
        .update_managed_run(
            &cancelled.id,
            cancelled.epoch,
            "cancelling",
            None,
            None,
            None,
            Some("cancellation requested"),
            unix_now() - 120,
            false,
        )
        .unwrap();
    assert!(!step(&s, "chat0", 1, false).unwrap());
    assert!(s
        .store
        .get_managed_run("chat0")
        .unwrap()
        .finished_at
        .is_none());
    assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
    let marker = fake
        .file_read("vm", "/run/ahvm-managed-cancel/chat0", 0, 64)
        .unwrap();
    assert_eq!(marker.data, b"cancel\n");
    fake.session_running
        .lock()
        .unwrap()
        .insert(cancelled.session_id.unwrap(), false);
    assert!(step(&s, "chat0", 1, false).unwrap());
    assert_eq!(
        s.store.get_managed_run("chat0").unwrap().phase,
        "interrupted"
    );
    assert!(s.activity.has_managed_run("vm"));
    assert!(!step(&s, "chat1", 1, false).unwrap());
    s.activity
        .touch_at("vm", Instant::now() - Duration::from_secs(7200));
    let sweep = sweep_once(&s, ThermalConfig::default(), Instant::now()).await;
    assert_eq!(sweep.stopped + sweep.paused, 0);
    assert_eq!(fake.launches.load(Ordering::SeqCst), 3);
    assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
}

#[test]
fn isolated_nonzero_receipt_and_missing_session_never_fence_siblings() {
    let (s, fake) = setup();
    let a = admit_isolated(&s, "a");
    let b = admit_isolated(&s, "b");
    assert!(!step(&s, "a", a.epoch, true).unwrap());
    assert!(!step(&s, "b", b.epoch, true).unwrap());
    let a = s.store.get_managed_run("a").unwrap();
    fake.session_running
        .lock()
        .unwrap()
        .insert(a.session_id.clone().unwrap(), false);
    fake.exit_code.store(137, Ordering::SeqCst);
    assert!(!step(&s, "a", a.epoch, false).unwrap());
    let uncertain = s.store.get_managed_run("a").unwrap();
    assert_eq!(uncertain.phase, "uncertain");
    s.store
        .update_managed_run(
            "a",
            a.epoch,
            "uncertain",
            None,
            None,
            None,
            None,
            unix_now() - 120,
            false,
        )
        .unwrap();
    assert!(!step(&s, "a", a.epoch, false).unwrap());
    fake.session_delete("vm", a.session_id.as_deref().unwrap())
        .unwrap();
    assert!(!step(&s, "a", a.epoch, false).unwrap());
    assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
    assert!(s.activity.has_managed_run("vm"));
    let recovered = s.store.claim_managed_run("a", unix_now()).unwrap();
    s.activity.set_managed_run("vm", "a", recovered.epoch);
    fake.file_write(
        "vm",
        "/proc/sys/kernel/random/boot_id",
        b"bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb\n",
    )
    .unwrap();
    assert!(step(&s, "a", recovered.epoch, false).unwrap());
    assert!(s.activity.has_managed_run("vm"));
    assert!(step(&s, "b", b.epoch, false).unwrap());
    assert!(!s.activity.has_managed_run("vm"));
    assert_eq!(fake.launches.load(Ordering::SeqCst), 2);
}

#[test]
fn terminal_commit_releases_pinned_receipt_and_recovery_cleans_crash_window() {
    let (s, fake) = setup();
    let a = admit_isolated(&s, "a");
    assert!(!step(&s, "a", a.epoch, true).unwrap());
    let a = s.store.get_managed_run("a").unwrap();
    fake.session_running
        .lock()
        .unwrap()
        .insert(a.session_id.clone().unwrap(), false);
    assert!(step(&s, "a", a.epoch, false).unwrap());
    assert!(fake.session_list("vm").unwrap().is_empty());
    assert_eq!(s.store.get_managed_run("a").unwrap().exit_code, Some(0));

    let b = admit_isolated(&s, "b");
    assert!(!step(&s, "b", b.epoch, true).unwrap());
    let b = s.store.get_managed_run("b").unwrap();
    fake.session_running
        .lock()
        .unwrap()
        .insert(b.session_id.clone().unwrap(), false);
    s.store
        .update_managed_run(
            "b",
            b.epoch,
            "succeeded",
            None,
            None,
            Some(0),
            None,
            unix_now(),
            true,
        )
        .unwrap();
    let terminal = s.store.get_managed_run("b").unwrap();
    assert_eq!(fake.session_list("vm").unwrap().len(), 1);
    release_guest_receipt(&s, &terminal);
    assert!(fake.session_list("vm").unwrap().is_empty());
    assert_eq!(fake.launches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn old_guest_forge_cannot_admit_isolated_work() {
    let (s, fake) = setup();
    fake.receipt_pinning.store(false, Ordering::SeqCst);
    let mut body = request();
    body.session_isolated = true;
    assert!(matches!(
        submit(
            State(s.clone()),
            Extension(UserId("admin".into())),
            Path("new".into()),
            Json(body)
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert!(s.store.list_active_managed_runs().unwrap().is_empty());
    assert_eq!(fake.launches.load(Ordering::SeqCst), 0);
}

#[test]
fn recovery_batches_terminal_receipt_cleanup_once_per_vm_and_checks_identity() {
    let (s, fake) = setup();
    for i in 0..70 {
        let id = format!("history{i}");
        let run = admit_isolated(&s, &id);
        assert!(!step(&s, &id, run.epoch, true).unwrap());
        let run = s.store.get_managed_run(&id).unwrap();
        fake.session_running
            .lock()
            .unwrap()
            .insert(run.session_id.clone().unwrap(), i == 69);
        s.store
            .update_managed_run(
                &id,
                run.epoch,
                "succeeded",
                None,
                None,
                Some(0),
                None,
                unix_now(),
                true,
            )
            .unwrap();
    }
    // A terminal record naming a current session with different argv is not
    // permission to delete it, nor is an identity from a previous guest boot.
    for (id, boot, argv) in [
        (
            "wrong-argv",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
            vec!["/bin/sleep".into(), "10".into()],
        ),
        (
            "old-boot",
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
            Vec::new(),
        ),
    ] {
        let run = admit_isolated(&s, id);
        let argv = if argv.is_empty() {
            command(&run).unwrap()
        } else {
            argv
        };
        let sid = fake.session_create("vm", &argv, false).unwrap();
        fake.session_running
            .lock()
            .unwrap()
            .insert(sid.clone(), false);
        s.store
            .update_managed_run(
                id,
                run.epoch,
                "succeeded",
                Some(&sid),
                Some(boot),
                Some(0),
                None,
                unix_now(),
                true,
            )
            .unwrap();
    }
    fake.boot_reads.store(0, Ordering::SeqCst);
    fake.session_lists.store(0, Ordering::SeqCst);
    recover(&s).unwrap();
    assert_eq!(fake.boot_reads.load(Ordering::SeqCst), 1);
    assert_eq!(fake.session_lists.load(Ordering::SeqCst), 1);
    let remaining = fake.session_list("vm").unwrap();
    assert_eq!(remaining.len(), 3); // running, different argv, and old boot
    assert_eq!(fake.launches.load(Ordering::SeqCst), 72);
}

#[tokio::test]
async fn running_plain_wake_preserves_active_isolated_runs_and_lifecycle_guards() {
    let (s, fake) = setup();
    let a = admit_isolated(&s, "a");
    assert!(!step(&s, "a", a.epoch, true).unwrap());
    let before = s.store.get_managed_run("a").unwrap();
    let started = crate::sandboxes::start(
        State(s.clone()),
        Extension(UserId("admin".into())),
        Path("vm".into()),
    )
    .await
    .unwrap();
    assert_eq!(started.0.state, "running");
    assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    let after = s.store.get_managed_run("a").unwrap();
    assert_eq!(after.session_id, before.session_id);
    assert_eq!(after.boot_id, before.boot_id);
    assert!(after.finished_at.is_none());
    let b = admit_isolated(&s, "b");
    assert!(!step(&s, "b", b.epoch, true).unwrap());
    assert_eq!(fake.launches.load(Ordering::SeqCst), 2);
    assert!(matches!(
        crate::sandboxes::stop(
            State(s.clone()),
            Extension(UserId("admin".into())),
            Path("vm".into()),
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert!(matches!(
        crate::sandboxes::start_operation(
            State(s.clone()),
            Extension(UserId("admin".into())),
            Path("vm".into()),
            None,
            Some(65536),
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert!(matches!(
        crate::sandboxes::destroy(
            State(s.clone()),
            Extension(UserId("admin".into())),
            Path("vm".into()),
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    fake.inner.stop("vm").unwrap();
    assert!(matches!(
        crate::sandboxes::start(
            State(s.clone()),
            Extension(UserId("admin".into())),
            Path("vm".into()),
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    fake.inner.start("vm").unwrap();
    fake.pause("vm").unwrap();
    assert!(matches!(
        crate::sandboxes::start(
            State(s.clone()),
            Extension(UserId("admin".into())),
            Path("vm".into()),
        )
        .await,
        Err(ApiError::Conflict(_))
    ));
    assert_eq!(fake.stops.load(Ordering::SeqCst), 0);
    assert!(s.activity.has_managed_run("vm"));
}
