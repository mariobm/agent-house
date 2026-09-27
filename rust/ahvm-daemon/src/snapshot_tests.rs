use super::*;
use ahvm_engine::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};

#[derive(Debug, Default)]
struct Gate {
    state: Mutex<(bool, bool)>,
    ready: Condvar,
}
impl Gate {
    fn enter(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        self.ready.notify_all();
        while !state.1 {
            state = self.ready.wait(state).unwrap();
        }
    }
    fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.ready.notify_all();
    }
    async fn entered(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !self.state.lock().unwrap().0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
#[derive(Debug)]
struct TestBackend {
    inner: MockBackend,
    create_gate: Option<Arc<Gate>>,
    fail_delete: AtomicBool,
}
macro_rules! delegate {
    ($(fn $name:ident(&self $(, $arg:ident : $ty:ty)*) -> $ret:ty;)*) => {$ (
        fn $name(&self $(, $arg: $ty)*) -> $ret { self.inner.$name($($arg),*) }
    )*};
}
impl Backend for TestBackend {
    delegate! {
        fn create(&self, spec: &SandboxSpec) -> Result<SandboxInfo>;
        fn destroy(&self, id: &str) -> Result<()>;
        fn start(&self, id: &str) -> Result<()>;
        fn stop(&self, id: &str) -> Result<()>;
        fn status(&self, id: &str) -> Result<SandboxInfo>;
        fn list(&self) -> Result<Vec<SandboxInfo>>;
        fn exec(&self, id: &str, argv: &[String]) -> Result<ExecResult>;
        fn restore(&self, snapshot: &SnapshotManifest, new_id: &str) -> Result<SandboxInfo>;
        fn fork(&self, id: &str, new_id: &str) -> Result<SandboxInfo>;
        fn capabilities(&self) -> Capabilities;
        fn snapshot_manifest(&self, id: &str) -> Result<SnapshotManifest>;
        fn snapshot_ids(&self) -> Result<Vec<String>>;
        fn snapshot_local_bytes(&self, id: &str) -> Result<u64>;
        fn file_read(&self, id: &str, path: &str, offset: u64, limit: u64) -> Result<FileChunk>;
        fn file_write(&self, id: &str, path: &str, data: &[u8]) -> Result<u64>;
        fn file_list(&self, id: &str, path: &str, offset: u64, limit: u64) -> Result<DirListing>;
        fn session_create(&self, id: &str, argv: &[String], pty: bool) -> Result<String>;
        fn session_read(&self, id: &str, session: &str, seq: u64, budget: std::time::Duration) -> Result<SessionChunk>;
        fn session_input(&self, id: &str, session: &str, data: &[u8]) -> Result<u64>;
        fn session_kill(&self, id: &str, session: &str) -> Result<()>;
        fn session_delete(&self, id: &str, session: &str) -> Result<()>;
        fn session_list(&self, id: &str) -> Result<Vec<SessionInfo>>;
        fn session_resize(&self, id: &str, session: &str, rows: u16, cols: u16) -> Result<()>;
    }
    fn create_snapshot(&self, id: &str, snap: &str) -> Result<SnapshotManifest> {
        if let Some(gate) = &self.create_gate {
            gate.enter();
        }
        self.inner.create_snapshot(id, snap)
    }
    fn delete_snapshot(&self, id: &str) -> Result<()> {
        if self.fail_delete.load(Ordering::Relaxed) {
            return Err(Error::InvalidState("injected cleanup failure".into()));
        }
        self.inner.delete_snapshot(id)
    }
}
fn fixture(gate: Option<Arc<Gate>>) -> (AppState, Arc<TestBackend>, std::path::PathBuf) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "ahvm-snapshot-security-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(ahvm_store::Store::open_in_memory().unwrap());
    store
        .upsert_user(&ahvm_store::User {
            id: "alice".into(),
            name: "alice".into(),
            api_key_hash: "hash".into(),
            max_sandboxes: 8,
            max_cpus: 32,
            max_memory_mb: 65536,
            max_volumes_mb: 102400,
            max_snapshots: 1,
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    let backend = Arc::new(TestBackend {
        inner: MockBackend::new(root.join("snapshots")),
        create_gate: gate,
        fail_delete: AtomicBool::new(false),
    });
    let spec = SandboxSpec {
        name: "vm".into(),
        cpus: 1,
        memory_mb: 512,
        backend: BackendKind::Krucible,
        storage_mode: None,
        root_image: None,
        kernel_image: None,
        desktop: false,
        desktop_gpu: false,
        network_bytes_per_sec: None,
        extra_env: Default::default(),
    };
    backend.create(&spec).unwrap();
    store
        .create_sandbox(&ahvm_store::Sandbox {
            id: "vm".into(),
            name: "vm".into(),
            owner_user_id: "alice".into(),
            backend: ahvm_store::Backend::Krucible,
            state: "running".into(),
            thermal: "hot".into(),
            cpus: 1,
            memory_mb: 512,
            ip: String::new(),
            created_at: 0,
            updated_at: 0,
        })
        .unwrap();
    let state = AppState {
        store,
        backend: backend.clone(),
        private_owners: Arc::default(),
        quotas: crate::quotas::Registry::new(),
        activity: crate::thermal::ActivityTracker::new(),
        ops: crate::scheduler::OpsLimiter::new(4),
        lifecycle: crate::scheduler::LifecycleLocks::new(),
    };
    (state, backend, root)
}
async fn snapshot(state: AppState, id: &str) -> ApiResult<impl IntoResponse> {
    create(
        State(state),
        Extension(UserId("alice".into())),
        Path("vm".into()),
        Json(CreateBody { name: id.into() }),
    )
    .await
}
async fn remove(state: AppState, id: &str) -> ApiResult<StatusCode> {
    delete(
        State(state),
        Extension(UserId("alice".into())),
        Path(id.into()),
    )
    .await
}
#[tokio::test]
async fn deleting_snapshots_removes_files_before_releasing_quota() {
    let (state, backend, root) = fixture(None);
    for id in ["one", "two"] {
        snapshot(state.clone(), id).await.unwrap();
        assert!(state.store.get_snapshot(id).unwrap().local_bytes > 0);
        assert_eq!(state.store.count_snapshots("alice").unwrap(), 1);
        assert!(matches!(
            snapshot(state.clone(), "over-limit").await,
            Err(ApiError::Forbidden(_))
        ));
        remove(state.clone(), id).await.unwrap();
        assert_eq!(state.store.count_snapshots("alice").unwrap(), 0);
        assert!(backend.snapshot_ids().unwrap().is_empty());
        assert!(!root.join("snapshots").join(id).exists());
    }
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn failed_cleanup_stays_owned_and_charged_until_recovery() {
    let (state, backend, root) = fixture(None);
    snapshot(state.clone(), "one").await.unwrap();
    backend.fail_delete.store(true, Ordering::Relaxed);
    assert!(remove(state.clone(), "one").await.is_err());
    assert_eq!(state.store.get_snapshot("one").unwrap().state, "deleting");
    assert_eq!(state.store.count_snapshots("alice").unwrap(), 1);
    assert!(recover(&state).is_err());
    assert!(matches!(
        snapshot(state.clone(), "two").await,
        Err(ApiError::Forbidden(_))
    ));
    assert!(matches!(
        delete(
            State(state.clone()),
            Extension(UserId("bob".into())),
            Path("one".into())
        )
        .await,
        Err(ApiError::NotFound(_))
    ));
    backend.fail_delete.store(false, Ordering::Relaxed);
    recover(&state).unwrap();
    assert_eq!(state.store.count_snapshots("alice").unwrap(), 0);
    assert!(backend.snapshot_ids().unwrap().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn canceled_request_keeps_snapshot_charge_and_finishes_publication() {
    let gate = Arc::new(Gate::default());
    let (state, backend, root) = fixture(Some(gate.clone()));
    let task = tokio::spawn(snapshot(state.clone(), "one"));
    gate.entered().await;
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    assert_eq!(state.store.get_snapshot("one").unwrap().state, "creating");
    assert_eq!(state.store.count_snapshots("alice").unwrap(), 1);
    assert!(matches!(
        snapshot(state.clone(), "two").await,
        Err(ApiError::Forbidden(_))
    ));
    gate.release();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state.store.get_snapshot("one").unwrap().state != "ready" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(backend.snapshot_manifest("one").is_ok());
    remove(state.clone(), "one").await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn recovery_finishes_publication_and_cleans_partial_and_legacy_orphans() {
    let (state, backend, root) = fixture(None);
    snapshot(state.clone(), "one").await.unwrap();
    state
        .store
        .set_snapshot_state("one", "creating", 0)
        .unwrap();
    recover(&state).unwrap();
    let row = state.store.get_snapshot("one").unwrap();
    assert_eq!(row.state, "ready");
    assert!(row.local_bytes > 0);
    state.store.set_snapshot_state("one", "ready", 0).unwrap();
    recover(&state).unwrap();
    assert_eq!(
        state.store.get_snapshot("one").unwrap().local_bytes,
        row.local_bytes
    );
    backend.inner.create_snapshot("vm", "orphan").unwrap();
    recover(&state).unwrap();
    assert!(!root.join("snapshots/orphan").exists());
    state
        .store
        .set_snapshot_state("one", "creating", 0)
        .unwrap();
    std::fs::remove_file(root.join("snapshots/one/ahvm-manifest.json")).unwrap();
    recover(&state).unwrap();
    assert_eq!(state.store.count_snapshots("alice").unwrap(), 0);
    assert!(backend.snapshot_ids().unwrap().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}
