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
//! * `stop` is the cold tier: local storage persists a snapshot bundle;
//!   replicated storage drains the disk and later cold-boots without RAM.
//! * `start` relaunches a stopped sandbox from its bundle, or fresh when
//!   there is none (Go `Start`).
//! * `fork` clones a sandbox into a second live one (Go's restore/fork
//!   path); backends without `Capabilities::fork` reject it.
//! * `create_snapshot` / `restore` move typed [`SnapshotManifest`](crate::SnapshotManifest)
//!   bundles; `restore` re-checks the manifest compat gate.
//! * `file_*` / `session_*` are the guest file and session surface (Go's
//!   fileEngine + session ops); sessions stream via repeated
//!   `session_read` drains rather than a persistent connection.

use crate::{
    DirListing, ExecResult, FileChunk, Result, SandboxInfo, SandboxSpec, SessionChunk, SessionInfo,
    SnapshotManifest,
};
use std::time::Duration;

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
    fn start_with_network_bandwidth(&self, id: &str, bytes: Option<u64>) -> Result<()> {
        if bytes.is_some() {
            return Err(crate::Error::InvalidState(
                "network policy unavailable".into(),
            ));
        }
        self.start(id)
    }
    fn stop(&self, id: &str) -> Result<()>;
    fn status(&self, id: &str) -> Result<SandboxInfo>;
    fn list(&self) -> Result<Vec<SandboxInfo>>;
    fn exec(&self, id: &str, argv: &[String]) -> Result<ExecResult>;
    fn create_snapshot(&self, id: &str, snapshot_id: &str) -> Result<SnapshotManifest>;
    fn restore(&self, snapshot: &SnapshotManifest, new_id: &str) -> Result<SandboxInfo>;
    fn fork(&self, id: &str, new_id: &str) -> Result<SandboxInfo>;
    /// Open one raw connection to a loopback-only guest TCP service.
    fn preview_connect(&self, _id: &str, _port: u16) -> Result<std::os::unix::net::UnixStream> {
        Err(crate::Error::InvalidState(
            "preview transport unavailable".into(),
        ))
    }
    /// Raw VNC stream, only for explicitly desktop-enabled sandboxes.
    fn desktop_connect(&self, _id: &str) -> Result<std::os::unix::net::UnixStream> {
        Err(crate::Error::InvalidState("desktop unavailable".into()))
    }
    fn capabilities(&self) -> Capabilities;
    fn sandbox_capabilities(&self, id: &str) -> Result<Capabilities> {
        let mut caps = self.capabilities();
        if self.status(id)?.storage.mode == crate::StorageMode::Replicated {
            caps.fork = false;
            caps.typed_snapshots = false;
            caps.live_migration = false;
        }
        Ok(caps)
    }
    /// Explicit remote barrier. Local storage has no remote durability contract.
    fn sync_remote(&self, _id: &str) -> Result<crate::ReplicationStatus> {
        Err(crate::Error::InvalidState("remote sync unavailable".into()))
    }

    /// Read a registered snapshot's manifest (for `restore`, which takes
    /// the manifest, not just the id). Unknown ids are NotFound.
    fn snapshot_manifest(&self, snapshot_id: &str) -> Result<SnapshotManifest>;

    // -- files (cf. Go's fileEngine surface) --
    fn file_read(&self, id: &str, path: &str, offset: u64, limit: u64) -> Result<FileChunk>;
    fn file_write(&self, id: &str, path: &str, data: &[u8]) -> Result<u64>;
    /// Stream into a guest temporary file; publish only after reader EOF.
    /// Reader errors abort the transaction. Implementations must not buffer all input.
    fn file_upload(&self, _id: &str, _path: &str, _input: &mut dyn std::io::Read) -> Result<u64> {
        Err(crate::Error::InvalidState(
            "streaming uploads unavailable".into(),
        ))
    }
    fn file_list(&self, id: &str, path: &str, offset: u64, limit: u64) -> Result<DirListing>;

    // -- sessions (cf. forge SessionReq ops) --
    //
    // `session_read` drains output from `from_seq` until EOF or the budget
    // runs out (partial chunk with `eof: false`). Streaming callers (the
    // daemon's WS bridge) loop on it; the trait stays synchronous.
    fn session_create(&self, id: &str, argv: &[String], pty: bool) -> Result<String>;
    fn session_read(
        &self,
        id: &str,
        session_id: &str,
        from_seq: u64,
        budget: Duration,
    ) -> Result<SessionChunk>;
    /// Interactive read: return as soon as output is available, without
    /// waiting to fill the drain budget. Cursors retain normal read semantics.
    fn session_poll(
        &self,
        id: &str,
        session_id: &str,
        from_seq: u64,
        budget: Duration,
    ) -> Result<SessionChunk> {
        self.session_read(id, session_id, from_seq, budget)
    }
    fn session_input(&self, id: &str, session_id: &str, data: &[u8]) -> Result<u64>;
    fn session_kill(&self, id: &str, session_id: &str) -> Result<()>;
    fn session_delete(&self, id: &str, session_id: &str) -> Result<()>;
    fn session_list(&self, id: &str) -> Result<Vec<SessionInfo>>;
    fn session_resize(&self, id: &str, session_id: &str, rows: u16, cols: u16) -> Result<()>;

    /// Alias for [`Backend::create_snapshot`] kept so both spellings in
    /// circulation resolve to the same operation.
    fn snapshot(&self, id: &str, snapshot_id: &str) -> Result<SnapshotManifest> {
        self.create_snapshot(id, snapshot_id)
    }
}
