use super::*;
use crate::thermal::{sweep_once, ActivityTracker, ThermalConfig};
use ahvm_engine::*;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

#[derive(Debug)]
struct Fake {
    inner: MockBackend,
    running: AtomicBool,
    lose_launch_reply: AtomicBool,
    fail_stop: AtomicBool,
    launches: AtomicUsize,
    stops: AtomicUsize,
}
macro_rules! delegate {
    ($(fn $name:ident($($arg:ident: $ty:ty),*) -> $out:ty;)*) => {$ (
        fn $name(&self, $($arg:$ty),*) -> $out { self.inner.$name($($arg),*) }
    )*};
}
impl Backend for Fake {
    delegate! {
        fn create(spec:&SandboxSpec)->ahvm_engine::Result<SandboxInfo>;
        fn destroy(id:&str)->ahvm_engine::Result<()>;
        fn start(id:&str)->ahvm_engine::Result<()>;
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
        fn file_read(id:&str,path:&str,offset:u64,limit:u64)->ahvm_engine::Result<FileChunk>;
        fn file_write(id:&str,path:&str,data:&[u8])->ahvm_engine::Result<u64>;
        fn file_list(id:&str,path:&str,offset:u64,limit:u64)->ahvm_engine::Result<DirListing>;
        fn session_input(id:&str,sid:&str,data:&[u8])->ahvm_engine::Result<u64>;
        fn session_delete(id:&str,sid:&str)->ahvm_engine::Result<()>;
        fn session_resize(id:&str,sid:&str,rows:u16,cols:u16)->ahvm_engine::Result<()>;
    }
    fn status(&self, id: &str) -> ahvm_engine::Result<SandboxInfo> {
        let mut v = self.inner.status(id)?;
        v.storage.mode = StorageMode::Replicated;
        Ok(v)
    }
    fn stop(&self, id: &str) -> ahvm_engine::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.fail_stop.load(Ordering::SeqCst) {
            return Err(Error::Control("sync unavailable".into()));
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
        let mut sessions = self.inner.session_list(id)?;
        for s in &mut sessions {
            s.running = self.running.load(Ordering::SeqCst);
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
        self.inner.session_read(id, sid, seq, budget)
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
        lose_launch_reply: AtomicBool::new(false),
        fail_stop: AtomicBool::new(false),
        launches: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
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
