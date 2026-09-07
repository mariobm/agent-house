//! The dual-backend seam: one lifecycle trait every VMM backend
//! implements, plus the [`Capabilities`] struct callers use to gate
//! backend-specific operations.
//!
//! Lifecycle semantics mirror `pkg/engine/krucible`:
//!
//! * `create` boots a new sandbox (Go: prepare root, spawn the helper,
//!   wire vsock bridges) and reports it live.
//! * `destroy` kills the worker and removes sandbox state (Go: kills the
//!   helper, removes the sandbox dir). Destroying an unknown id is an
//!   error, never silent success.
//! * `stop` is the cold tier: quiesce at a clean boundary and persist a
//!   snapshot bundle the sandbox can cold-restore from (Go `Stop`).
//! * `start` relaunches a stopped sandbox from its bundle, or fresh when
//!   there is none (Go `Start`).
//! * `fork` clones a sandbox into a second live one (Go's restore/fork
//!   path); backends without `Capabilities::fork` reject it.
//! * `create_snapshot` / `restore` move typed [`SnapshotManifest`](crate::SnapshotManifest)
//!   bundles; `restore` re-checks the manifest compat gate.

use crate::{ExecResult, Result, SandboxInfo, SandboxSpec, SnapshotManifest};

/// What a backend can do. Capability-gated callers must check these
/// before invoking `fork` / typed snapshots / live migration instead of
/// probing for errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub fork: bool,
    pub typed_snapshots: bool,
    pub live_migration: bool,
    pub max_vcpus: u8,
}

/// Sandbox lifecycle. All methods are synchronous; slow backends are
/// expected to block the calling thread (the daemon dispatches them off
/// its async core).
pub trait Backend: Send + Sync + std::fmt::Debug {
    fn create(&self, spec: &SandboxSpec) -> Result<SandboxInfo>;
    fn destroy(&self, id: &str) -> Result<()>;
    fn start(&self, id: &str) -> Result<()>;
    fn stop(&self, id: &str) -> Result<()>;
    fn status(&self, id: &str) -> Result<SandboxInfo>;
    fn list(&self) -> Result<Vec<SandboxInfo>>;
    fn exec(&self, id: &str, argv: &[String]) -> Result<ExecResult>;
    fn create_snapshot(&self, id: &str, snapshot_id: &str) -> Result<SnapshotManifest>;
    fn restore(&self, snapshot: &SnapshotManifest, new_id: &str) -> Result<SandboxInfo>;
    fn fork(&self, id: &str, new_id: &str) -> Result<SandboxInfo>;
    fn capabilities(&self) -> Capabilities;

    /// Alias for [`Backend::create_snapshot`] kept so both spellings in
    /// circulation resolve to the same operation.
    fn snapshot(&self, id: &str, snapshot_id: &str) -> Result<SnapshotManifest> {
        self.create_snapshot(id, snapshot_id)
    }
}
