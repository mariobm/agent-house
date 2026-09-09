//! ahvm-engine: VMM lifecycle core — backend seam, neutral types, snapshot
//! compat gate, worker supervision, and an in-memory mock.
//!
//! This crate links **no VMM library** and needs **no KVM**: it is the
//! control-plane half of the engine. The real VMM worker (linking
//! libkrucible as a native crate, no cgo) lands separately; this crate
//! spawns and supervises one such worker process per sandbox and talks to
//! it over a control socket, mirroring the `pkg/engine/krucible` split
//! where pure-Go orchestration spawned the `cmd/vmm` helper.
//!
//! Dual-backend seam (plan §7): [`Backend`] is backend-neutral with
//! [`Capabilities`] gating what each backend can do. krucible is the
//! default and only real backend; Firecracker is a typed placeholder so
//! rows, configs, and manifests for both backends coexist without a
//! rewrite when/if it returns.
//!
//! Module map:
//!
//! * [`backend`]: the [`Backend`] trait + [`Capabilities`].
//! * [`spec`]: neutral [`SandboxSpec`](spec::SandboxSpec) /
//!   [`SandboxInfo`](spec::SandboxInfo) / [`ExecResult`](spec::ExecResult).
//! * [`snapshot`]: v2 [`SnapshotManifest`](snapshot::SnapshotManifest) with
//!   the strict compat gate (arch, VMM, kernel, device layout).
//! * [`worker`]: per-sandbox worker supervision ([`LiveWorker`] owns the
//!   child handle and reaps it; [`is_alive`]/[`terminate_adopted`] cover
//!   adopted pids after supervisor restart).
//! * [`krucible`]: the real krucible [`Backend`] (one libkrun worker per
//!   sandbox, cold snapshot/restore, crash recovery by pid adoption).
//! * [`mock`]: in-memory [`Backend`] for unit tests.

mod backend;
mod krucible;
mod mock;
mod network;
pub use network::NetworkConfig;
mod snapshot;
mod spec;
mod worker;

pub use backend::{Backend, Capabilities};
pub use krucible::{KrucibleBackend, KrucibleConfig};
pub use mock::{MockBackend, MAX_EXEC_OUTPUT, MOCK_KERNEL_DIGEST};
pub use snapshot::{
    host_caps, Artifacts, Compat, HostCaps, SnapshotManifest, VmmId, DEVICE_LAYOUT_VER,
    MANIFEST_VER, SIDECAR_NAME,
};
pub use spec::{
    BackendKind, DirEntry, DirListing, ExecResult, FileChunk, SandboxInfo, SandboxSpec,
    SessionChunk, SessionInfo, State, Thermal,
};
pub use worker::{
    is_alive, process_starttime, send_ctl, spawn_worker, spawn_worker_cfg, terminate_adopted,
    LiveWorker, SpawnConfig, Worker,
};

/// Engine-wide error. Mirrors the `ahvm-store` style: typed variants for
/// expected domain failures (`NotFound`, `Conflict`, `InvalidState`,
/// `Incompatible`), transparent wrappers for IO/JSON.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid state: {0}")]
    InvalidState(String),
    #[error("incompatible: {0}")]
    Incompatible(String),
    #[error("control: {0}")]
    Control(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
pub(crate) fn test_scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ahvm-engine-test-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}
