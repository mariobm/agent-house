//! Real krucible [`Backend`](crate::Backend): one libkrun worker per
//! sandbox, supervised out-of-band.
//!
//! Layout per sandbox (`<data_dir>/<id>/`):
//!
//! ```text
//! spec.json      worker spec for ahvm-vmm
//! state.json     worker pid record (see [`crate::Worker`])
//! sandbox.json   backend record (spec + info + snapshot registry view)
//! vmm.log        worker stderr (evidence on dead-on-arrival)
//! sock/          c.sock / f.sock (vsock bridges) + control.sock
//! root.qcow2     live overlay (CoW over the base image)
//! bundle/        cold snapshot: memory.img + checkpoint.bin +
//!                manifest.json (libkrun) + ahvm-manifest.json (sidecar)
//!                + root.qcow2 (frozen disk)
//! ```
//!
//! Disk honesty (STATUS-engine §"disk rollback"): libkrun's checkpoint
//! captures RAM only, so the backend freezes the overlay itself —
//! `bundle/root.qcow2` is a file copy taken while SNAPSHOT leaves vCPUs
//! paused, and every restore boots a fresh copy of it. Reusing the live
//! overlay across a restore is NOT a rollback and is never done here.
//!
//! Worker recovery: [`KrucibleBackend::open`] re-adopts live pids from
//! `state.json` (a supervisor restart must not kill VMs); dead pids surface
//! as [`State::Failed`](crate::State). Owned children are reaped via
//! [`LiveWorker`](crate::LiveWorker); adopted ones via pid polling.

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
use serde::{Deserialize, Serialize};

use crate::{
    host_caps, is_alive, process_starttime, send_ctl, spawn_worker_cfg, terminate_adopted, Backend,
    BackendKind, Capabilities, DirListing, Error, ExecResult, FileChunk, LiveWorker, Result,
    SandboxInfo, SandboxSpec, SessionChunk, SessionInfo, SnapshotManifest, SpawnConfig, State,
    Thermal, Worker, MAX_EXEC_OUTPUT,
};

/// VMM identity stamped into snapshot sidecars (must match the worker's
/// libkrun build; the compat gate refuses anything else on restore).
pub const KRUCIBLE_VMM_NAME: &str = "krucible";
pub const KRUCIBLE_VMM_VERSION: &str = "libkrun-2.0.0-dev";
pub const KRUCIBLE_KERNEL_DIGEST: &str = "bundled:libkrunfw.so.5";

/// One forge RPC round trip (readiness probes; execution waits longer).
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// Connect-retry budget for one exec (the bridge either exists or the
/// worker is dead; liveness reconciliation owns the rest).
const CONNECT_BUDGET: Duration = Duration::from_secs(30);
/// Default agent-readiness budget (slow hosts override via config).
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(60);
/// Default exec budget, matching forge's own execution deadline.
const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(300);

/// Static backend configuration. Everything a worker needs that must NOT
/// come from the daemon's environment travels here explicitly.
#[derive(Debug, Clone)]
pub struct KrucibleConfig {
    /// Release `ahvm-vmm` binary (also serves `create-overlay`).
    pub vmm_bin: PathBuf,
    /// Backing ext4 image overlays are cut from (or per-spec `root_image`).
    pub base_image: PathBuf,
    /// Sandbox state lives here (`<data_dir>/<id>/`, see module docs).
    pub data_dir: PathBuf,
    /// Value for the worker's `LD_LIBRARY_PATH` (libkrunfw discovery).
    pub lib_path: String,
    /// Agent-readiness budget per boot/restore.
    pub ready_timeout: Duration,
    /// Exec response budget. Must cover forge's own execution deadline
    /// (default 300s): readiness polling uses `ready_timeout` with short
    /// per-attempt RPCs, but a started exec waits up to this long.
    pub exec_timeout: Duration,
    /// Opt-in isolated outbound TCP/DNS.
    pub network: Option<crate::NetworkConfig>,
    /// Optional host-delegated per-VM resource limits.
    pub resources: Option<crate::ResourceConfig>,
}

impl KrucibleConfig {
    pub fn new(vmm_bin: PathBuf, base_image: PathBuf, data_dir: PathBuf, lib_path: String) -> Self {
        Self {
            vmm_bin,
            base_image,
            data_dir,
            lib_path,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            exec_timeout: DEFAULT_EXEC_TIMEOUT,
            network: None,
            resources: None,
        }
    }

    fn validate(&self) -> Result<()> {
        if !self.vmm_bin.is_file() {
            return Err(Error::InvalidState(format!(
                "vmm binary missing: {}",
                self.vmm_bin.display()
            )));
        }
        if !self.base_image.is_file() {
            return Err(Error::InvalidState(format!(
                "base image missing: {}",
                self.base_image.display()
            )));
        }
        Ok(())
    }
}

/// On-disk backend record (`sandbox.json`). Socket/spec/log paths derive
/// from `dir` and are not duplicated here.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SandboxRecord {
    spec: SandboxSpec,
    info: SandboxInfo,
    /// Overlay backing path used at create (base image or spec root_image).
    backing: PathBuf,
}

/// A supervised worker handle: owned (we spawned it, we reap it) or
/// adopted (supervisor restarted; pid polling only, no wait).
#[derive(Debug)]
pub(crate) enum WorkerHandle {
    Owned(LiveWorker),
    Adopted(Worker),
}

impl WorkerHandle {
    /// True while the worker process exists and is not a zombie.
    /// Reaps an owned child that already exited (no zombie left behind).
    /// Adopted pids additionally require identity verification: a reused
    /// pid is NOT our worker, however live it looks.
    pub(crate) fn alive(&mut self) -> bool {
        match self {
            WorkerHandle::Owned(w) => match w.try_reap() {
                Ok(Some(_)) => false,
                Ok(None) => true,
                Err(_) => false,
            },
            WorkerHandle::Adopted(w) => is_alive(w.pid) && verified(w),
        }
    }

    /// Stop and clean up. Owned: SIGKILL + reap (no zombie, idempotent).
    /// Adopted: pin and verify identity before SIGTERM, then poll for exit.
    pub(crate) fn terminate(&mut self) -> Result<()> {
        match self {
            WorkerHandle::Owned(w) => w.terminate().map_err(Error::Io),
            WorkerHandle::Adopted(w) => {
                if !is_alive(w.pid) || !verified(w) {
                    // Already gone, or the pid belongs to someone else now.
                    return Ok(());
                }
                terminate_adopted(w)?;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if !is_alive(w.pid) {
                        return Ok(());
                    }
                    // Original exited and the pid was reused: hands off.
                    if !verified(w) {
                        return Ok(());
                    }
                    if Instant::now() > deadline {
                        return Err(Error::Control(format!(
                            "adopted worker {} did not exit after SIGTERM",
                            w.pid
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

#[derive(Debug)]
struct LiveRec {
    needs_resume: bool,
    record: SandboxRecord,
    dir: PathBuf,
    worker: Option<WorkerHandle>,
}

#[derive(Debug)]
struct Inner {
    sandboxes: HashMap<String, LiveRec>,
    /// snapshot_id -> bundle dir (rebuilt from `<data_dir>/snapshots/`).
    snapshots: HashMap<String, PathBuf>,
    next_ip_octet: u8,
}

/// Holds one sandbox's reservation until the operation completes.
/// A dedicated reservation lock avoids re-locking the sandbox map in Drop.
/// Poison recovery ensures unwinding still releases the reservation.
struct OpGuard<'a> {
    be: &'a KrucibleBackend,
    id: String,
}

impl<'a> OpGuard<'a> {
    fn take(be: &'a KrucibleBackend, id: &str) -> Result<Self> {
        let mut busy = be.reservations.lock().unwrap_or_else(|e| e.into_inner());
        if !busy.insert(id.to_string()) {
            return Err(Error::Conflict(format!(
                "sandbox {id}: operation already in progress"
            )));
        }
        Ok(Self {
            be,
            id: id.to_string(),
        })
    }
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        self.be
            .reservations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// Process identity for adoption: a recorded starttime must match the live
/// pid. Unknown identities cannot be adopted; signalling additionally pins
/// the process with a pidfd before verifying identity.
fn verified(w: &Worker) -> bool {
    match w.starttime {
        Some(t) => process_starttime(w.pid) == Some(t),
        None => false,
    }
}

/// Real krucible backend. Synchronous and blocking (the daemon dispatches
/// off its async core); internally a single map lock with take-operate-
/// reinsert discipline so long worker waits never hold the lock.
#[derive(Debug)]
pub struct KrucibleBackend {
    networks: Option<crate::network::Networks>,
    cfg: KrucibleConfig,
    inner: Mutex<Inner>,
    reservations: Mutex<HashSet<String>>,
}

impl KrucibleBackend {
    /// Open (or create) `cfg.data_dir`, adopting live workers from
    /// `state.json` records. Dead pids surface as `Failed`.
    pub fn open(cfg: KrucibleConfig) -> Result<Self> {
        cfg.validate()?;
        if let Some(resources) = &cfg.resources {
            resources.validate()?;
        }
        std::fs::create_dir_all(&cfg.data_dir)?;
        // Recover interrupted publication before deleting uncommitted debris.
        sweep_debris(&cfg.data_dir)?;
        let mut inner = Inner {
            sandboxes: HashMap::new(),
            snapshots: HashMap::new(),
            next_ip_octet: 2,
        };
        // Snapshot registry first (restores reference it).
        let snaps = cfg.data_dir.join("snapshots");
        if snaps.is_dir() {
            let mut ids: Vec<String> = Vec::new();
            for entry in std::fs::read_dir(&snaps)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        // Residual copies mid-rename are never registered.
                        if !name.ends_with(".tmp") {
                            ids.push(name.to_string());
                        }
                    }
                }
            }
            ids.sort();
            for id in ids {
                inner.snapshots.insert(id.clone(), snaps.join(id));
            }
        }
        // Sandbox records.
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&cfg.data_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.file_name() != "snapshots" {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        for dir in dirs {
            let record_path = dir.join("sandbox.json");
            if !record_path.is_file() {
                if dir.join("net.json").exists() {
                    crate::network::cleanup_orphan(&dir)?;
                }
                continue;
            }
            let raw = std::fs::read(&record_path)?;
            let record: SandboxRecord = serde_json::from_slice(&raw).map_err(|e| {
                Error::InvalidState(format!("{}: bad sandbox.json: {e}", dir.display()))
            })?;
            let id = record.info.id.clone();
            let worker = match Worker::load(dir.join("state.json")) {
                // Identity-verified adoption only: a live pid with a
                // mismatched starttime belongs to someone else (PID reuse).
                Ok(w) if is_alive(w.pid) && w.starttime.is_none() => {
                    return Err(Error::InvalidState(format!(
                        "sandbox {id}: live worker has no verifiable process identity; refusing adoption"
                    )));
                }
                Ok(w) if is_alive(w.pid) && verified(&w) => Some(WorkerHandle::Adopted(w)),
                _ => None,
            };
            if cfg.network.is_none() && dir.join("net.json").exists() {
                if worker.is_some() {
                    let saved: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(dir.join("spec.json"))?)?;
                    if saved["net_uds"].as_str().is_some_and(|s| !s.is_empty()) {
                        return Err(Error::InvalidState(
                            "networked live VMs require network configuration on daemon restart"
                                .into(),
                        ));
                    }
                } else {
                    crate::network::cleanup_orphan(&dir)?;
                }
            }
            if let Some(resources) = &cfg.resources {
                let group = resources.prepare(&id, record.spec.cpus, record.spec.memory_mb)?;
                if let Some(WorkerHandle::Adopted(w)) = &worker {
                    crate::resources::verify_member(&group, w.pid)?;
                }
            }
            let mut info = record.info.clone();
            // A surviving process may be paused inside an interrupted snapshot.
            // Probe once on adoption; process liveness alone cannot prove that
            // guest commands can run. Desktop workers have no control socket.
            let needs_resume =
                worker.is_some() && !record.spec.desktop && recover_control(&dir).is_err();
            if worker.is_some() {
                info.state = if needs_resume {
                    State::Failed
                } else {
                    State::Running
                };
            } else if info.state == State::Running {
                info.state = State::Failed;
            }
            inner.next_ip_octet = inner.next_ip_octet.max(2);
            inner.sandboxes.insert(
                id,
                LiveRec {
                    needs_resume,
                    record: SandboxRecord { info, ..record },
                    dir,
                    worker,
                },
            );
        }
        let networks = cfg
            .network
            .clone()
            .map(crate::network::Networks::new)
            .transpose()?;
        if let Some(networks) = &networks {
            for rec in inner.sandboxes.values() {
                if let Some(WorkerHandle::Adopted(vm)) = &rec.worker {
                    let spec: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(rec.dir.join("spec.json"))?)?;
                    if spec
                        .get("net_uds")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .is_empty()
                    {
                        return Err(Error::InvalidState(
                            "stop existing VMs before enabling networking".into(),
                        ));
                    }
                    let group = cfg
                        .resources
                        .as_ref()
                        .map(|r| r.path(&rec.record.info.id))
                        .transpose()?;
                    networks.prepare(&rec.dir, group.as_deref())?;
                    networks.attach(&rec.dir, vm.clone());
                } else {
                    networks.remove(&rec.dir)?;
                }
            }
        }
        Ok(Self {
            networks,
            cfg,
            inner: Mutex::new(inner),
            reservations: Mutex::new(HashSet::new()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("krucible mutex poisoned")
    }

    fn host_caps(&self) -> crate::HostCaps {
        host_caps(
            KRUCIBLE_VMM_NAME,
            KRUCIBLE_VMM_VERSION,
            KRUCIBLE_KERNEL_DIGEST,
        )
    }
}

// ---------------------------------------------------------------------------
// Sandbox directory helpers (pure path derivation; no IO).
// ---------------------------------------------------------------------------

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        return Err(Error::InvalidState(format!("bad sandbox id {id:?}")));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(Error::InvalidState(format!("bad sandbox id {id:?}")));
    }
    if id == "snapshots" || id.starts_with('.') {
        return Err(Error::InvalidState(format!("reserved sandbox id {id:?}")));
    }
    Ok(())
}

fn sock_dir(dir: &Path) -> PathBuf {
    dir.join("sock")
}
/// Restore execution after a supervisor died between PAUSE and RESUME.
/// A failed probe remains retryable via start(), without hiding other VMs.
fn recover_control(dir: &Path) -> Result<()> {
    let ctl = control_sock(dir);
    match send_ctl(&ctl, "STATUS")?.as_str() {
        "OK running" => Ok(()),
        "OK paused" => {
            let reply = send_ctl(&ctl, "RESUME")?;
            if reply == "OK running" {
                Ok(())
            } else {
                Err(Error::Control(format!("adoption RESUME refused: {reply}")))
            }
        }
        reply => Err(Error::Control(format!("adoption STATUS refused: {reply}"))),
    }
}

fn control_sock(dir: &Path) -> PathBuf {
    // Must match Worker::control_socket (sock_dir/control.sock): the worker
    // binds whatever path its spec names, so name it for the record API.
    sock_dir(dir).join("control.sock")
}
fn forge_sock(dir: &Path) -> PathBuf {
    sock_dir(dir).join("c.sock")
}

// ---------------------------------------------------------------------------
// Forge client (exec only; files/sessions follow in the daemon phase).
// ---------------------------------------------------------------------------

fn connect_rpc(sock: &Path, budget: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + budget;
    loop {
        match UnixStream::connect(sock) {
            Ok(c) => {
                c.set_read_timeout(Some(RPC_TIMEOUT)).map_err(Error::Io)?;
                c.set_write_timeout(Some(RPC_TIMEOUT)).map_err(Error::Io)?;
                return Ok(c);
            }
            Err(e) => {
                if Instant::now() > deadline {
                    return Err(Error::Control(format!(
                        "forge bridge {} never accepted: {e}",
                        sock.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// One forge exec. Readiness (bridge accept) is bounded by CONNECT_BUDGET;
/// the EXECUTION budget is separate and must cover forge's own deadline
/// (default 300s) — a slow-but-valid reply must never be abandoned by a
/// readiness-sized timeout.
fn rpc_exec(sock: &Path, argv: &[String], exec_timeout: Duration) -> Result<ExecResult> {
    let body = serde_json::json!({ "argv": argv });
    let v = forge_call(
        sock,
        FrameType::ExecReq,
        body,
        FrameType::ExecResp,
        exec_timeout,
    )?;
    let exit_code = v["exit_code"]
        .as_i64()
        .ok_or_else(|| Error::Control("exec reply lacks exit_code".to_string()))?;
    // Forge caps output itself and reports `truncated`; the local cap is
    // backstop only. Either flag means the output is NOT complete.
    let guest_truncated = v["truncated"].as_bool().unwrap_or(false);
    let cap = |mut s: String| -> (String, bool) {
        let truncated = s.len() > MAX_EXEC_OUTPUT;
        if truncated {
            s = s.chars().take(MAX_EXEC_OUTPUT).collect();
        }
        (s, truncated)
    };
    let stdout = String::from_utf8_lossy(&forge_b64(&v, "stdout_b64")?).into_owned();
    let stderr = String::from_utf8_lossy(&forge_b64(&v, "stderr_b64")?).into_owned();
    let (stdout, t1) = cap(stdout);
    let (stderr, t2) = cap(stderr);
    Ok(ExecResult {
        exit_code: exit_code as i32,
        stdout,
        stderr,
        truncated: guest_truncated || t1 || t2,
    })
}

/// One forge request/response round trip. `Error` frames map to
/// `NotFound` for unknown sessions/files (best-effort message match —
/// forge reports OS errors as text) and `Control` for everything else.
fn forge_call(
    sock: &Path,
    req_type: FrameType,
    body: serde_json::Value,
    expect: FrameType,
    exec_timeout: Duration,
) -> Result<serde_json::Value> {
    let mut c = connect_rpc(sock, CONNECT_BUDGET)?;
    c.set_read_timeout(Some(exec_timeout)).map_err(Error::Io)?;
    let payload = serde_json::to_vec(&body)?;
    write_frame(
        &mut c,
        &Frame {
            msg_type: req_type,
            payload,
        },
    )
    .map_err(|e| Error::Control(format!("forge write: {e}")))?;
    let mut r = BufReader::new(c.try_clone().map_err(Error::Io)?);
    let f = read_frame(&mut r).map_err(|e| Error::Control(format!("forge read: {e}")))?;
    if f.msg_type == FrameType::Error {
        let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap_or_default();
        let msg = v["message"].as_str().unwrap_or("forge error").to_string();
        if msg.starts_with("no such session") || msg.contains("No such file") {
            return Err(Error::NotFound(msg));
        }
        return Err(Error::Control(msg));
    }
    if f.msg_type != expect {
        return Err(Error::Control(format!(
            "want {expect:?}, got {:?}",
            f.msg_type
        )));
    }
    serde_json::from_slice(&f.payload).map_err(|e| Error::Control(format!("forge JSON: {e}")))
}

fn forge_b64(v: &serde_json::Value, k: &str) -> Result<Vec<u8>> {
    let s = v[k]
        .as_str()
        .ok_or_else(|| Error::Control(format!("forge reply lacks {k}")))?;
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s)
        .map_err(|e| Error::Control(format!("forge {k} not base64: {e}")))
}

fn wait_ready(sock: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let probe = vec!["/bin/true".to_string()];
    loop {
        // Short per-attempt budget: readiness is a poll loop, and a hung
        // worker must surface at the outer deadline, not per attempt.
        match rpc_exec(sock, &probe, RPC_TIMEOUT) {
            Ok(r) if r.exit_code == 0 => return Ok(()),
            _ => {
                if Instant::now() > deadline {
                    return Err(Error::Control(format!(
                        "agent not ready within {timeout:?}"
                    )));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Worker lifecycle helpers.
// ---------------------------------------------------------------------------

impl KrucibleBackend {
    /// Cut a qcow2 overlay over `backing` sized to the backing file.
    fn create_overlay(&self, overlay: &Path, backing: &Path) -> Result<()> {
        let size = std::fs::metadata(backing)?.len().to_string();
        let out = Command::new(&self.cfg.vmm_bin)
            .args([
                "create-overlay",
                &overlay.to_string_lossy(),
                &backing.to_string_lossy(),
                &size,
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(Error::Io)?;
        if !out.status.success() {
            return Err(Error::Control(format!(
                "create-overlay failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    fn write_worker_spec(
        &self,
        dir: &Path,
        overlay: &Path,
        spec: &SandboxSpec,
        snapshot_dir: Option<&Path>,
    ) -> Result<PathBuf> {
        if spec.cpus == 0 || spec.cpus > BackendKind::Krucible.capabilities().max_vcpus {
            return Err(Error::InvalidState(format!(
                "cpus {} out of range 1..={}",
                spec.cpus,
                BackendKind::Krucible.capabilities().max_vcpus
            )));
        }
        let sock = sock_dir(dir);
        std::fs::create_dir_all(&sock)?;
        let mut js = serde_json::json!({
            "vcpus": spec.cpus,
            "mem_mib": spec.memory_mb,
            "log_level": 3,
            "root_disk": overlay.to_string_lossy(),
            "root_disk_format": "qcow2",
            "pid1": true,
            "exec_path": "/init.krun",
            "vsock_control_uds": sock.join("c.sock").to_string_lossy(),
            "vsock_forward_uds": sock.join("f.sock").to_string_lossy(),
            "control_socket_uds": control_sock(dir).to_string_lossy(),
            "env": spec.extra_env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
        });
        if spec.desktop {
            js["gpu"] = spec.desktop_gpu.into();
            js.as_object_mut().unwrap().remove("control_socket_uds");
        }
        if self.networks.is_some() {
            js["net_uds"] = sock.join("net.sock").to_string_lossy().into();
            js["net_mac"] = "02:00:00:00:00:02".into();
        }
        // Omit (never null): an explicit null is a parse error for workers.
        if let Some(bundle) = snapshot_dir {
            js["snapshot_dir"] = serde_json::Value::String(bundle.to_string_lossy().into_owned());
        }
        let path = dir.join("spec.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&js)?)?;
        Ok(path)
    }

    /// Hermetic spawn: the worker inherits NOTHING (see SpawnConfig).
    fn boot_worker(
        &self,
        dir: &Path,
        overlay: &Path,
        spec: &SandboxSpec,
        snapshot_dir: Option<&Path>,
    ) -> Result<LiveWorker> {
        let gpu_worker = self.cfg.vmm_bin.with_file_name("ahvm-vmm-gpu");
        let worker = if spec.desktop_gpu {
            if !gpu_worker.is_file() {
                return Err(Error::InvalidState(
                    "GPU worker is not installed; upgrade the host runtime".into(),
                ));
            }
            let accessible = std::fs::read_dir("/dev/dri").is_ok_and(|entries| {
                entries.flatten().any(|entry| {
                    entry.file_name().to_string_lossy().starts_with("renderD")
                        && std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(entry.path())
                            .is_ok()
                })
            });
            if !accessible {
                return Err(Error::InvalidState("GPU desktop requires an accessible /dev/dri/renderD* device; install or upgrade the host on a machine with a supported GPU".into()));
            }
            &gpu_worker
        } else {
            &self.cfg.vmm_bin
        };
        let spec_path = self.write_worker_spec(dir, overlay, spec, snapshot_dir)?;
        // Stale bridge sockets from a previous worker would steal connects.
        for stale in ["c.sock", "f.sock", "control.sock"] {
            let _ = std::fs::remove_file(sock_dir(dir).join(stale));
        }
        let mut env = vec![
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            "HOME=/root".to_string(),
            "LANG=C.UTF-8".to_string(),
            format!("LD_LIBRARY_PATH={}", self.cfg.lib_path),
        ];
        if spec.desktop_gpu {
            let gpu = worker.parent().unwrap().parent().unwrap().join("gpu");
            env.retain(|item| !item.starts_with("LD_LIBRARY_PATH="));
            env.push(format!(
                "LD_LIBRARY_PATH={}/lib:{}",
                gpu.display(),
                self.cfg.lib_path
            ));
            env.push(format!(
                "__EGL_VENDOR_LIBRARY_FILENAMES={}/egl.json",
                gpu.display()
            ));
            env.push(format!("LIBGL_DRIVERS_PATH={}/dri", gpu.display()));
            env.push(format!(
                "MESA_SHADER_CACHE_DIR={}",
                dir.join("mesa-cache").display()
            ));
        }
        let group = self
            .cfg
            .resources
            .as_ref()
            .map(|r| r.prepare(&spec.name, spec.cpus, spec.memory_mb))
            .transpose()?;
        if let Some(net) = &self.networks {
            net.prepare(dir, group.as_deref())?;
        }
        let result = spawn_worker_cfg(&SpawnConfig {
            cgroup: group.as_deref(),
            vmm_binary: worker.as_os_str(),
            spec_arg: &spec_path,
            state_path: &dir.join("state.json"),
            hermetic: true,
            env: &env,
            stderr_log: Some(&dir.join("vmm.log")),
        })
        .map_err(Error::Io);
        match &result {
            Ok(w) => {
                if let Some(net) = &self.networks {
                    net.attach(dir, w.record.clone());
                }
            }
            Err(_) => {
                if let Some(net) = &self.networks {
                    let _ = net.remove(dir);
                }
            }
        }
        result
    }

    fn ready(&self, dir: &Path) -> Result<()> {
        wait_ready(&forge_sock(dir), self.cfg.ready_timeout)?;
        if self.networks.is_some() {
            let argv = vec!["/bin/sh".into(), "-ec".into(),
                "ip link set lo up; ip link set eth0 up; ip addr replace 100.64.0.2/24 dev eth0; ip route replace default via 100.64.0.1; printf 'nameserver 100.64.0.1\\n' > /etc/resolv.conf".into()];
            let r = rpc_exec(&forge_sock(dir), &argv, Duration::from_secs(15))?;
            if r.exit_code != 0 {
                return Err(Error::Control(
                    "guest network setup failed (requires ip and /bin/sh)".into(),
                ));
            }
        }
        Ok(())
    }

    /// Online snapshot: PAUSE, SNAPSHOT into a fresh generation dir,
    /// freeze the overlay, write the sidecar, atomically publish, RESUME.
    /// The VM stays live (fork/create_snapshot path). A failed or
    /// interrupted snapshot never touches the previous recovery point:
    /// only a fully-written generation is published by rename.
    /// Returns the bundle dir.
    fn snapshot_live(&self, dir: &Path, rec: &SandboxRecord, snapshot_id: &str) -> Result<PathBuf> {
        if rec.spec.desktop {
            return Err(Error::InvalidState(
                "desktop snapshots are not supported".into(),
            ));
        }
        validate_snapshot_id(snapshot_id)?;
        let bundle = dir.join("bundle");
        let gen = dir.join("bundle.new");
        let _ = std::fs::remove_dir_all(&gen);
        let ctl = control_sock(dir);
        let snap = (|| -> Result<()> {
            let reply = send_ctl(&ctl, "PAUSE")?;
            if !reply.starts_with("OK") {
                return Err(Error::Control(format!("PAUSE refused: {reply}")));
            }
            // A 4-GiB checkpoint can exceed the five-second control RPC
            // budget on a healthy disk. Keep the operation reserved while it
            // writes, rather than treating slow progress as a failed snapshot.
            let reply = crate::worker::send_ctl_with_timeout(
                &ctl,
                &format!("SNAPSHOT {}", gen.display()),
                Duration::from_secs(300),
            )?;
            if !reply.starts_with("OK") {
                return Err(Error::Control(format!("SNAPSHOT refused: {reply}")));
            }
            // Freeze the disk while vCPUs are paused: libkrun's checkpoint
            // is RAM-only, so the overlay copy IS the disk snapshot.
            let root_bytes = std::fs::copy(dir.join("root.qcow2"), gen.join("root.qcow2"))?;
            if root_bytes == 0 {
                return Err(Error::Control("frozen overlay is empty".to_string()));
            }
            // Flush what we wrote before publishing the generation.
            std::fs::File::open(gen.join("root.qcow2"))?.sync_all()?;
            let manifest = SnapshotManifest::new(
                snapshot_id,
                crate::Compat {
                    arch: std::env::consts::ARCH.to_string(),
                    vmm: crate::VmmId {
                        name: KRUCIBLE_VMM_NAME.into(),
                        version: KRUCIBLE_VMM_VERSION.into(),
                    },
                    kernel_digest: KRUCIBLE_KERNEL_DIGEST.into(),
                    mem_mib: rec.spec.memory_mb,
                    vcpus: rec.spec.cpus,
                    device_layout_ver: crate::DEVICE_LAYOUT_VER,
                },
                crate::Artifacts {
                    memory_bytes: std::fs::metadata(gen.join("memory.img"))
                        .map(|m| m.len())
                        .unwrap_or(0),
                    root_delta_bytes: root_bytes,
                },
            );
            let sidecar = manifest.write_to(&gen)?;
            std::fs::File::open(sidecar)?.sync_all()?;
            publish_bundle(&gen, &bundle)?;
            Ok(())
        })();
        // Even a lost PAUSE reply may have paused the guest. Verify/resume
        // after every attempt. If that cannot be confirmed, do not advertise
        // Running merely because the process survived; start() can retry it.
        if let Err(error) = recover_control(dir) {
            let mut inner = self.lock();
            if let Some(live) = inner.sandboxes.get_mut(&rec.info.id) {
                live.needs_resume = true;
                live.record.info.state = State::Failed;
                let _ = self.persist_record(dir, &live.record);
            }
            return Err(error);
        }
        match snap {
            Ok(()) => Ok(bundle),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&gen);
                Err(e)
            }
        }
    }

    fn persist_record(&self, dir: &Path, record: &SandboxRecord) -> Result<()> {
        let tmp = dir.join("sandbox.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(record)?)?;
        std::fs::rename(&tmp, dir.join("sandbox.json"))?;
        Ok(())
    }

    /// The sandbox must exist with a live worker; returns its directory.
    /// Mirrors the mock's Running gate so both backends agree on InvalidState.
    fn live_dir(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        let inner = self.lock();
        let rec = inner
            .sandboxes
            .get(id)
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
        if rec.record.info.state != State::Running || rec.worker.is_none() {
            return Err(Error::InvalidState(format!(
                "sandbox {id} is not running (state {:?})",
                rec.record.info.state
            )));
        }
        Ok(rec.dir.clone())
    }

    /// Copy a frozen bundle root into place and cold-boot it (compat-gated).
    /// Caller holds no lock; returns the owned worker on success.
    fn boot_from_bundle(
        &self,
        dir: &Path,
        spec: &SandboxSpec,
        bundle: &Path,
    ) -> Result<LiveWorker> {
        let back = SnapshotManifest::read_from(bundle)?;
        back.check_compat(&self.host_caps())?;
        std::fs::copy(bundle.join("root.qcow2"), dir.join("root.qcow2"))?;
        let worker = self.boot_worker(dir, &dir.join("root.qcow2"), spec, Some(bundle))?;
        if let Err(e) = self.ready(dir) {
            let mut w = worker;
            let _ = w.terminate();
            return Err(e);
        }
        Ok(worker)
    }
}

fn validate_snapshot_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        return Err(Error::InvalidState(format!("bad snapshot id {id:?}")));
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::InvalidState(format!("bad snapshot id {id:?}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Backend trait.
// ---------------------------------------------------------------------------

impl KrucibleBackend {
    /// [`Backend::create_snapshot`] with reservations already held
    /// (see [`OpGuard`]; `fork` holds both sides across the two calls).
    fn snapshot_to_registry(&self, id: &str, snapshot_id: &str) -> Result<SnapshotManifest> {
        let (dir, record) = {
            let inner = self.lock();
            let rec = inner
                .sandboxes
                .get(id)
                .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
            if rec.record.info.state != State::Running || rec.worker.is_none() {
                return Err(Error::InvalidState(format!(
                    "sandbox {id} is not running (state {:?})",
                    rec.record.info.state
                )));
            }
            (rec.dir.clone(), rec.record.clone())
        };
        let bundle = self.snapshot_live(&dir, &record, snapshot_id)?;
        // Registry copy: the live bundle keeps evolving with the sandbox;
        // a named snapshot is immutable (fork/restore read it later).
        // Copy aside and rename into place so a crash never leaves a
        // half-written registry entry behind.
        let reg = self.cfg.data_dir.join("snapshots").join(snapshot_id);
        if reg.exists() {
            return Err(Error::Conflict(format!("snapshot {snapshot_id} exists")));
        }
        let tmp = self
            .cfg
            .data_dir
            .join("snapshots")
            .join(format!("{snapshot_id}.tmp"));
        let _ = std::fs::remove_dir_all(&tmp);
        let published = (|| -> Result<SnapshotManifest> {
            copy_dir(&bundle, &tmp)?;
            sync_tree(&tmp)?;
            std::fs::rename(&tmp, &reg)?;
            sync_dir(reg.parent().expect("snapshot registry"))?;
            sync_dir(&self.cfg.data_dir)?;
            SnapshotManifest::read_from(&reg)
        })();
        if published.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        let manifest = published?;
        self.lock().snapshots.insert(snapshot_id.to_string(), reg);
        Ok(manifest)
    }

    /// [`Backend::restore`] with the reservation already held.
    fn restore_inner(&self, snapshot: &SnapshotManifest, new_id: &str) -> Result<SandboxInfo> {
        let inner = self.lock();
        if inner.sandboxes.contains_key(new_id) {
            return Err(Error::Conflict(format!("sandbox {new_id} already exists")));
        }
        let bundle = inner
            .snapshots
            .get(&snapshot.snapshot_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("snapshot {}", snapshot.snapshot_id)))?;
        drop(inner);
        // The STORED bundle is authoritative: a caller-supplied manifest is
        // only a handle (snapshot_id lookup). Compatibility, sizing, and
        // the boot itself all derive from what is on disk, so altered or
        // stale caller metadata can never reach the worker.
        let stored = SnapshotManifest::read_from(&bundle)?;
        if stored.snapshot_id != snapshot.snapshot_id {
            return Err(Error::InvalidState(format!(
                "snapshot id mismatch: registry has {:?}, caller passed {:?}",
                stored.snapshot_id, snapshot.snapshot_id
            )));
        }
        stored.check_compat(&self.host_caps())?;
        // Spec derives from the stored manifest: sizing is part of the
        // compat gate, so a passing gate implies a bootable shape.
        let spec = SandboxSpec {
            name: new_id.to_string(),
            cpus: stored.compat.vcpus,
            memory_mb: stored.compat.mem_mib,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            desktop: false,
            desktop_gpu: false,
            extra_env: HashMap::new(),
        };
        let dir = self.cfg.data_dir.join(new_id);
        std::fs::create_dir_all(&dir)?;
        let octet = {
            let mut inner = self.lock();
            let octet = inner.next_ip_octet;
            inner.next_ip_octet = octet.wrapping_add(1).max(2);
            octet
        };

        let boot = (|| -> Result<LiveWorker> {
            std::fs::copy(bundle.join("root.qcow2"), dir.join("root.qcow2"))?;
            let worker = self.boot_worker(&dir, &dir.join("root.qcow2"), &spec, Some(&bundle))?;
            if let Err(e) = self.ready(&dir) {
                let mut w = worker;
                let _ = w.terminate();
                return Err(e);
            }
            Ok(worker)
        })();
        let mut inner = self.lock();
        match boot {
            Ok(worker) => {
                let info = SandboxInfo {
                    id: new_id.to_string(),
                    name: new_id.to_string(),
                    state: State::Running,
                    thermal: Thermal::Warm,
                    ip: if self.networks.is_some() {
                        "100.64.0.2".into()
                    } else {
                        format!("10.42.0.{octet}")
                    },
                };
                let record = SandboxRecord {
                    spec,
                    info: info.clone(),
                    backing: self.cfg.base_image.clone(),
                };
                if let Err(e) = self.persist_record(&dir, &record) {
                    let mut w = worker;
                    let _ = w.terminate();
                    return Err(e);
                }
                inner.sandboxes.insert(
                    new_id.to_string(),
                    LiveRec {
                        needs_resume: false,
                        record,
                        dir,
                        worker: Some(WorkerHandle::Owned(worker)),
                    },
                );
                Ok(info)
            }
            Err(e) => {
                if let Some(net) = &self.networks {
                    let _ = net.remove(&dir);
                }
                let _ = std::fs::remove_dir_all(&dir);
                if let Some(resources) = &self.cfg.resources {
                    let _ = resources.remove(new_id);
                }
                Err(e)
            }
        }
    }
}

impl KrucibleBackend {
    /// Session request/response plumbing shared by the session_* trait
    /// methods (inherent so the trait surface stays exactly the Backend
    /// seam; see also `snapshot_to_registry` / `restore_inner`).
    fn session_rpc(
        &self,
        id: &str,
        body: serde_json::Value,
        op: &str,
    ) -> Result<serde_json::Value> {
        let dir = self.live_dir(id)?;
        let v = forge_call(
            &forge_sock(&dir),
            FrameType::SessionReq,
            body,
            FrameType::SessionResp,
            self.cfg.exec_timeout,
        )?;
        require_op(&v, op)?;
        Ok(v)
    }
}

/// The forge response must be the operation we asked for (internally-tagged
/// enums serialize as `{"op": "<snake>", ...}`); anything else is a
/// backend/guest version skew, never silently accepted.
fn require_op(v: &serde_json::Value, op: &str) -> Result<()> {
    if v["op"].as_str() == Some(op) {
        Ok(())
    } else {
        Err(Error::Control(format!(
            "forge replied op {:?}, want {op:?}",
            v["op"].as_str()
        )))
    }
}

impl Backend for KrucibleBackend {
    fn capabilities(&self) -> Capabilities {
        BackendKind::Krucible.capabilities()
    }

    fn snapshot_manifest(&self, snapshot_id: &str) -> Result<SnapshotManifest> {
        let inner = self.lock();
        let bundle = inner
            .snapshots
            .get(snapshot_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("snapshot {snapshot_id}")))?;
        drop(inner);
        SnapshotManifest::read_from(&bundle)
    }

    fn create(&self, spec: &SandboxSpec) -> Result<SandboxInfo> {
        validate_id(&spec.name)?;
        // Custom kernels are a real worker feature, but the backend has no
        // story for them yet (sidecar digest, compat gate, restore sizing
        // all assume the bundled libkrunfw kernel). Reject loudly instead
        // of silently booting the wrong kernel.
        if spec.kernel_image.is_some() {
            return Err(Error::InvalidState(
                "custom kernel_image is not supported by the krucible backend yet".to_string(),
            ));
        }
        let _guard = OpGuard::take(self, &spec.name)?;
        let mut inner = self.lock();
        if inner.sandboxes.contains_key(&spec.name) {
            return Err(Error::Conflict(format!("sandbox {} exists", spec.name)));
        }
        let backing = match &spec.root_image {
            Some(p) => PathBuf::from(p),
            None => self.cfg.base_image.clone(),
        };
        if !backing.is_file() {
            return Err(Error::InvalidState(format!(
                "root image missing: {}",
                backing.display()
            )));
        }
        let dir = self.cfg.data_dir.join(&spec.name);
        std::fs::create_dir_all(&dir)?;
        // Take-operate-reinsert is unnecessary here (new record), but keep
        // worker boot outside any await: drop the guard across spawn+ready.
        let octet = inner.next_ip_octet;
        inner.next_ip_octet = octet.wrapping_add(1).max(2);
        drop(inner);

        let boot = (|| -> Result<LiveWorker> {
            self.create_overlay(&dir.join("root.qcow2"), &backing)?;
            let worker = self.boot_worker(&dir, &dir.join("root.qcow2"), spec, None)?;
            if let Err(e) = self.ready(&dir) {
                let mut w = worker;
                let _ = w.terminate();
                return Err(e);
            }
            Ok(worker)
        })();
        let mut inner = self.lock();
        match boot {
            Ok(worker) => {
                let info = SandboxInfo {
                    id: spec.name.clone(),
                    name: spec.name.clone(),
                    state: State::Running,
                    thermal: Thermal::Hot,
                    ip: if self.networks.is_some() {
                        "100.64.0.2".into()
                    } else {
                        format!("10.42.0.{octet}")
                    },
                };
                let record = SandboxRecord {
                    spec: spec.clone(),
                    info: info.clone(),
                    backing,
                };
                if let Err(e) = self.persist_record(&dir, &record) {
                    let mut w = worker;
                    let _ = w.terminate();
                    return Err(e);
                }
                inner.sandboxes.insert(
                    spec.name.clone(),
                    LiveRec {
                        needs_resume: false,
                        record,
                        dir,
                        worker: Some(WorkerHandle::Owned(worker)),
                    },
                );
                Ok(info)
            }
            Err(e) => {
                if let Some(net) = &self.networks {
                    let _ = net.remove(&dir);
                }
                let _ = std::fs::remove_dir_all(&dir);
                if let Some(resources) = &self.cfg.resources {
                    let _ = resources.remove(&spec.name);
                }
                Err(e)
            }
        }
    }

    fn destroy(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let _guard = OpGuard::take(self, id)?;
        let rec = {
            let mut inner = self.lock();
            inner
                .sandboxes
                .remove(id)
                .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))
        }?;
        if let Some(mut handle) = rec.worker {
            handle.terminate()?;
        }
        if let Some(net) = &self.networks {
            net.remove(&rec.dir)?;
        }
        std::fs::remove_dir_all(&rec.dir)?;
        if let Some(resources) = &self.cfg.resources {
            resources.remove(id)?;
        }
        Ok(())
    }

    /// Start a stopped/failed sandbox (compat-gated restore from its
    /// bundle, fresh boot otherwise). Already-running is Ok.
    ///
    /// Contract: liveness is reconciled best-effort at call time. A worker
    /// terminated out-of-band (SIGKILL/admin/OOM) dies asynchronously: a
    /// `start()` issued in the microseconds between the kill and actual
    /// death is indistinguishable from alive, and correctly reports Ok
    /// without rebooting. Callers that kill out-of-band must first observe
    /// the death (`status()` → `Failed`) and only then call `start()`.
    /// In-band paths (`stop`/`destroy`) are synchronous and race-free.
    fn start(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let _guard = OpGuard::take(self, id)?;
        let recovery_dir = {
            let mut inner = self.lock();
            let rec = inner
                .sandboxes
                .get_mut(id)
                .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
            (rec.needs_resume && rec.worker.as_mut().is_some_and(|worker| worker.alive()))
                .then(|| rec.dir.clone())
        };
        if let Some(dir) = recovery_dir {
            recover_control(&dir)?;
            let mut inner = self.lock();
            let rec = inner.sandboxes.get_mut(id).expect("reserved sandbox");
            rec.needs_resume = false;
            rec.record.info.state = State::Running;
            self.persist_record(&dir, &rec.record)?;
            return Ok(());
        }

        // Snapshot the decision inputs under lock, operate unlocked.
        // Liveness is reconciled here (not trusted from cache): a worker
        // that died behind a cached Running state reboots below instead
        // of reporting a success that is not true.
        enum Plan {
            Restore {
                dir: PathBuf,
                spec: SandboxSpec,
                bundle: PathBuf,
            },
            Fresh {
                dir: PathBuf,
                spec: SandboxSpec,
            },
        }
        let (plan, failed) = {
            let mut inner = self.lock();
            let rec = inner
                .sandboxes
                .get_mut(id)
                .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
            let alive = match rec.worker.as_mut() {
                Some(h) => h.alive(),
                None => false,
            };
            if !alive {
                // Drop the dead handle (owned children are reaped by
                // alive()) so the boot paths below start clean.
                rec.worker = None;
            }
            // Adoption recovery was checked above. An already-running
            // worker must never be double-booted.
            if alive {
                return Ok(());
            }
            if !alive && rec.record.info.state == State::Running {
                rec.record.info.state = State::Failed;
            }
            let plan = if rec.dir.join("bundle").join("manifest.json").is_file() {
                Plan::Restore {
                    dir: rec.dir.clone(),
                    spec: rec.record.spec.clone(),
                    bundle: rec.dir.join("bundle"),
                }
            } else {
                Plan::Fresh {
                    dir: rec.dir.clone(),
                    spec: rec.record.spec.clone(),
                }
            };
            let failed = if rec.record.info.state == State::Failed && !alive {
                Some((rec.dir.clone(), rec.record.clone()))
            } else {
                None
            };
            (plan, failed)
        };
        if let Some((dir, record)) = failed {
            let _ = self.persist_record(&dir, &record);
        }
        match plan {
            Plan::Restore { dir, spec, bundle } => {
                let worker = self.boot_from_bundle(&dir, &spec, &bundle)?;
                let mut inner = self.lock();
                if let Some(rec) = inner.sandboxes.get_mut(id) {
                    rec.record.info.state = State::Running;
                    rec.record.info.thermal = Thermal::Warm;
                    rec.worker = Some(WorkerHandle::Owned(worker));
                    rec.needs_resume = false;
                    let record = rec.record.clone();
                    self.persist_record(&dir, &record)?;
                }
                Ok(())
            }
            Plan::Fresh { dir, spec } => {
                let worker = self.boot_worker(&dir, &dir.join("root.qcow2"), &spec, None)?;
                if let Err(e) = self.ready(&dir) {
                    let mut w = worker;
                    let _ = w.terminate();
                    return Err(e);
                }
                let mut inner = self.lock();
                if let Some(rec) = inner.sandboxes.get_mut(id) {
                    rec.record.info.state = State::Running;
                    rec.record.info.thermal = Thermal::Hot;
                    rec.worker = Some(WorkerHandle::Owned(worker));
                    rec.needs_resume = false;
                    let record = rec.record.clone();
                    self.persist_record(&dir, &record)?;
                }
                Ok(())
            }
        }
    }

    fn stop(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let _guard = OpGuard::take(self, id)?;
        let (dir, has_worker) = {
            let inner = self.lock();
            let rec = inner
                .sandboxes
                .get(id)
                .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
            (rec.dir.clone(), rec.worker.is_some())
        };
        if !has_worker {
            // Already stopped: reconcile state and succeed (mock parity).
            let mut inner = self.lock();
            if let Some(rec) = inner.sandboxes.get_mut(id) {
                rec.record.info.state = State::Stopped;
                rec.record.info.thermal = Thermal::Cold;
                let record = rec.record.clone();
                self.persist_record(&dir, &record)?;
            }
            return Ok(());
        }
        // Online snapshot (leaves the guest running), then kill the worker:
        // the bundle + frozen root on disk ARE the stopped sandbox.
        let record = {
            let inner = self.lock();
            inner
                .sandboxes
                .get(id)
                .map(|r| r.record.clone())
                .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?
        };
        if record.spec.desktop {
            // No GPU/RAM checkpoint: flush guest disk writes before terminating.
            let result = rpc_exec(
                &forge_sock(&dir),
                &["/bin/sync".into()],
                Duration::from_secs(15),
            )?;
            if result.exit_code != 0 {
                return Err(Error::Control("desktop disk sync failed".into()));
            }
        } else {
            self.snapshot_live(&dir, &record, &format!("stop-{id}"))?;
        }
        let worker = {
            let mut inner = self.lock();
            inner.sandboxes.get_mut(id).and_then(|r| r.worker.take())
        };
        if let Some(mut handle) = worker {
            handle.terminate()?;
        }
        if let Some(net) = &self.networks {
            net.remove(&dir)?;
        }
        let _ = std::fs::remove_file(dir.join("state.json"));
        let mut inner = self.lock();
        if let Some(rec) = inner.sandboxes.get_mut(id) {
            rec.record.info.state = State::Stopped;
            rec.record.info.thermal = Thermal::Cold;
            let record = rec.record.clone();
            self.persist_record(&dir, &record)?;
        }
        Ok(())
    }

    fn status(&self, id: &str) -> Result<SandboxInfo> {
        validate_id(id)?;
        let mut inner = self.lock();
        let rec = inner
            .sandboxes
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
        let alive = match rec.worker.as_mut() {
            Some(h) => h.alive(),
            None => false,
        };
        if rec.worker.is_some() && !alive {
            // Worker died behind our back: keep the record for forensics,
            // surface Failed (never silently Running).
            rec.worker = None;
            rec.record.info.state = State::Failed;
            let record = rec.record.clone();
            let dir = rec.dir.clone();
            drop(inner);
            let _ = self.persist_record(&dir, &record);
            return Ok(record.info);
        }
        if alive && !rec.needs_resume && rec.record.info.state == State::Failed {
            // Healed: liveness is ground truth (see start()).
            rec.record.info.state = State::Running;
            let record = rec.record.clone();
            let dir = rec.dir.clone();
            drop(inner);
            let _ = self.persist_record(&dir, &record);
            return Ok(record.info);
        }
        Ok(rec.record.info.clone())
    }

    fn list(&self) -> Result<Vec<SandboxInfo>> {
        // Reconcile like status() but without failing the whole listing on
        // one bad record: collect ids first, then query one by one.
        let ids: Vec<String> = {
            let inner = self.lock();
            let mut ids: Vec<String> = inner.sandboxes.keys().cloned().collect();
            ids.sort();
            ids
        };
        ids.iter().map(|id| self.status(id)).collect()
    }

    fn desktop_connect(&self, id: &str) -> Result<UnixStream> {
        validate_id(id)?;
        {
            let inner = self.lock();
            let rec = inner
                .sandboxes
                .get(id)
                .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
            if !rec.record.spec.desktop {
                return Err(Error::InvalidState("sandbox is not desktop-enabled".into()));
            }
        }
        let dir = self.live_dir(id)?;
        Ok(UnixStream::connect(sock_dir(&dir).join("f.sock"))?)
    }

    fn preview_connect(&self, id: &str, port: u16) -> Result<UnixStream> {
        if self
            .lock()
            .sandboxes
            .get(id)
            .is_some_and(|rec| rec.record.spec.desktop)
        {
            return Err(Error::InvalidState(
                "previews are not yet supported for desktop VMs".into(),
            ));
        }
        if port == 0 || self.cfg.network.is_none() {
            return Err(Error::InvalidState(
                "previews require managed networking and a nonzero port".into(),
            ));
        }
        let dir = self.live_dir(id)?;
        let mut c = connect_rpc(&forge_sock(&dir), Duration::from_secs(5))?;
        c.set_read_timeout(Some(Duration::from_secs(6)))?;
        c.set_write_timeout(Some(Duration::from_secs(6)))?;
        write_frame(
            &mut c,
            &Frame {
                msg_type: FrameType::ForwardReq,
                payload: serde_json::to_vec(&serde_json::json!({"port": port}))?,
            },
        )
        .map_err(|e| Error::Control(e.to_string()))?;
        // Read precisely one frame, without buffering service bytes after it.
        let response = read_frame(&mut c).map_err(|e| Error::Control(e.to_string()))?;
        if response.msg_type != FrameType::ForwardResp {
            return Err(Error::Control("guest preview connection refused".into()));
        }
        c.set_read_timeout(None)?;
        c.set_write_timeout(None)?;
        Ok(c)
    }

    fn exec(&self, id: &str, argv: &[String]) -> Result<ExecResult> {
        let dir = self.live_dir(id)?;
        rpc_exec(&forge_sock(&dir), argv, self.cfg.exec_timeout)
    }

    fn file_read(&self, id: &str, path: &str, offset: u64, limit: u64) -> Result<FileChunk> {
        let dir = self.live_dir(id)?;
        let v = forge_call(
            &forge_sock(&dir),
            FrameType::FileReq,
            serde_json::json!({ "op": "read", "path": path, "offset": offset, "limit": limit }),
            FrameType::FileResp,
            self.cfg.exec_timeout,
        )?;
        require_op(&v, "read")?;
        Ok(FileChunk {
            data: forge_b64(&v, "data_b64")?,
            eof: v["eof"].as_bool().unwrap_or(true),
        })
    }

    fn file_upload(&self, id: &str, path: &str, input: &mut dyn std::io::Read) -> Result<u64> {
        let dir = self.live_dir(id)?;
        let mut conn = connect_rpc(&forge_sock(&dir), CONNECT_BUDGET)?;
        conn.set_read_timeout(Some(Duration::from_secs(30)))?;
        conn.set_write_timeout(Some(Duration::from_secs(30)))?;
        write_frame(
            &mut conn,
            &Frame {
                msg_type: FrameType::FileReq,
                payload: serde_json::to_vec(&serde_json::json!({"op":"upload","path":path}))?,
            },
        )
        .map_err(|e| Error::Control(e.to_string()))?;
        let reply = read_frame(&mut conn).map_err(|e| Error::Control(e.to_string()))?;
        let ready: serde_json::Value =
            serde_json::from_slice(&reply.payload).map_err(|e| Error::Control(e.to_string()))?;
        if reply.msg_type != FrameType::FileResp || ready["op"] != "upload_ready" {
            return Err(Error::Control(format!("upload unavailable: {ready}")));
        }
        let mut chunk = [0u8; 65536];
        let mut total = 0u64;
        loop {
            let n = match input.read(&mut chunk) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => other?,
            };
            if n == 0 {
                break;
            }
            write_frame(
                &mut conn,
                &Frame {
                    msg_type: FrameType::FileData,
                    payload: chunk[..n].to_vec(),
                },
            )
            .map_err(|e| Error::Control(e.to_string()))?;
            total = total
                .checked_add(n as u64)
                .ok_or_else(|| Error::Control("upload size overflow".into()))?;
        }
        write_frame(
            &mut conn,
            &Frame {
                msg_type: FrameType::FileCommit,
                payload: serde_json::to_vec(&total)?,
            },
        )
        .map_err(|e| Error::Control(e.to_string()))?;
        let reply = read_frame(&mut conn).map_err(|e| Error::Control(e.to_string()))?;
        let response: serde_json::Value =
            serde_json::from_slice(&reply.payload).map_err(|e| Error::Control(e.to_string()))?;
        if reply.msg_type != FrameType::FileResp
            || response["op"] != "write"
            || response["bytes"].as_u64() != Some(total)
        {
            return Err(Error::Control(format!("upload failed: {response}")));
        }
        Ok(total)
    }

    fn file_write(&self, id: &str, path: &str, data: &[u8]) -> Result<u64> {
        let dir = self.live_dir(id)?;
        let v = forge_call(
            &forge_sock(&dir),
            FrameType::FileReq,
            serde_json::json!({ "op": "write", "path": path, "data_b64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data) }),
            FrameType::FileResp,
            self.cfg.exec_timeout,
        )?;
        require_op(&v, "write")?;
        v["bytes"]
            .as_u64()
            .ok_or_else(|| Error::Control("file write reply lacks bytes".to_string()))
    }

    fn file_list(&self, id: &str, path: &str, offset: u64, limit: u64) -> Result<DirListing> {
        let dir = self.live_dir(id)?;
        let v = forge_call(
            &forge_sock(&dir),
            FrameType::FileReq,
            serde_json::json!({ "op": "list", "path": path, "offset": offset, "limit": limit }),
            FrameType::FileResp,
            self.cfg.exec_timeout,
        )?;
        require_op(&v, "list")?;
        let entries = v["entries"]
            .as_array()
            .ok_or_else(|| Error::Control("file list reply lacks entries".to_string()))?
            .iter()
            .map(|e| {
                Ok(crate::DirEntry {
                    name: e["name"]
                        .as_str()
                        .ok_or_else(|| Error::Control("dir entry lacks name".to_string()))?
                        .to_string(),
                    is_dir: e["is_dir"].as_bool().unwrap_or(false),
                    size: e["size"].as_u64().unwrap_or(0),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(DirListing {
            entries,
            next_offset: v["next_offset"].as_u64(),
        })
    }

    fn session_create(&self, id: &str, argv: &[String], pty: bool) -> Result<String> {
        let dir = self.live_dir(id)?;
        let v = forge_call(
            &forge_sock(&dir),
            FrameType::SessionReq,
            serde_json::json!({ "op": "create", "argv": argv, "pty": pty }),
            FrameType::SessionResp,
            self.cfg.exec_timeout,
        )?;
        require_op(&v, "started")?;
        v["session_id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Control("session create reply lacks session_id".to_string()))
    }

    fn session_read(
        &self,
        id: &str,
        session_id: &str,
        from_seq: u64,
        budget: Duration,
    ) -> Result<SessionChunk> {
        self.read_session_output(id, session_id, from_seq, budget, false)
    }

    fn session_poll(
        &self,
        id: &str,
        session_id: &str,
        from_seq: u64,
        budget: Duration,
    ) -> Result<SessionChunk> {
        self.read_session_output(id, session_id, from_seq, budget, true)
    }

    fn session_input(&self, id: &str, session_id: &str, data: &[u8]) -> Result<u64> {
        let v = self.session_rpc(
            id,
            serde_json::json!({ "op": "input", "session_id": session_id, "data_b64": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, data) }),
            "input_acked",
        )?;
        // Forge replies InputAcked{bytes}; accept the field or fall back
        // to the sent length when an older forge omits it.
        Ok(v["bytes"].as_u64().unwrap_or(data.len() as u64))
    }

    fn session_kill(&self, id: &str, session_id: &str) -> Result<()> {
        self.session_rpc(
            id,
            serde_json::json!({ "op": "kill", "session_id": session_id }),
            "killed",
        )?;
        Ok(())
    }

    fn session_delete(&self, id: &str, session_id: &str) -> Result<()> {
        self.session_rpc(
            id,
            serde_json::json!({ "op": "delete", "session_id": session_id }),
            "deleted",
        )?;
        Ok(())
    }

    fn session_list(&self, id: &str) -> Result<Vec<SessionInfo>> {
        let v = self.session_rpc(id, serde_json::json!({ "op": "list" }), "listed")?;
        let sessions = v["sessions"]
            .as_array()
            .ok_or_else(|| Error::Control("session list reply lacks sessions".to_string()))?;
        sessions
            .iter()
            .map(|s| {
                Ok(SessionInfo {
                    id: s["id"]
                        .as_str()
                        .ok_or_else(|| Error::Control("session lacks id".to_string()))?
                        .to_string(),
                    argv: s["argv"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default(),
                    running: s["running"].as_bool().unwrap_or(false),
                    started_at: s["started_at"].as_i64().unwrap_or(0),
                })
            })
            .collect()
    }

    fn session_resize(&self, id: &str, session_id: &str, rows: u16, cols: u16) -> Result<()> {
        self.session_rpc(
            id,
            serde_json::json!({ "op": "resize", "session_id": session_id, "rows": rows, "cols": cols }),
            "resized",
        )?;
        Ok(())
    }

    fn create_snapshot(&self, id: &str, snapshot_id: &str) -> Result<SnapshotManifest> {
        validate_id(id)?;
        validate_snapshot_id(snapshot_id)?;
        let _guard = OpGuard::take(self, id)?;
        // The registry entry is shared across sandboxes: serialize on it
        // too, so two sandboxes snapshotting the same id cannot interleave
        // copies into one registry dir.
        let _snap_guard = OpGuard::take(self, &format!("snap:{snapshot_id}"))?;
        self.snapshot_to_registry(id, snapshot_id)
    }

    fn restore(&self, snapshot: &SnapshotManifest, new_id: &str) -> Result<SandboxInfo> {
        validate_id(new_id)?;
        let _guard = OpGuard::take(self, new_id)?;
        self.restore_inner(snapshot, new_id)
    }

    fn fork(&self, id: &str, new_id: &str) -> Result<SandboxInfo> {
        if !self.capabilities().fork {
            return Err(Error::InvalidState(
                "backend does not support fork".to_string(),
            ));
        }
        validate_id(id)?;
        validate_id(new_id)?;
        // Hold both sides for the whole fork: the source cannot be
        // stopped/destroyed mid-snapshot and the target id cannot be
        // claimed between the snapshot and the restore.
        let _src = OpGuard::take(self, id)?;
        let _dst = OpGuard::take(self, new_id)?;
        // Live snapshot under an internal id, then restore it hot.
        let internal = format!("fork-{new_id}");
        let _snap = OpGuard::take(self, &format!("snap:{internal}"))?;
        let manifest = self.snapshot_to_registry(id, &internal)?;
        let mut info = self.restore_inner(&manifest, new_id)?;
        info.thermal = Thermal::Hot;
        let mut inner = self.lock();
        if let Some(rec) = inner.sandboxes.get_mut(new_id) {
            rec.record.info = info.clone();
            let record = rec.record.clone();
            let dir = rec.dir.clone();
            drop(inner);
            self.persist_record(&dir, &record)?;
            return Ok(info);
        }
        Ok(info)
    }
}

/// Remove crash debris from a previous failed snapshot publish or
/// registry copy: `<name>.tmp` registry dirs, per-sandbox `bundle.new`,
/// `bundle.old`, and `sandbox.json.tmp`. Never touches live bundles or
/// records. An interrupted swap restores `bundle.old` before cleanup.
fn sweep_debris(data_dir: &Path) -> Result<()> {
    let snaps = data_dir.join("snapshots");
    if snaps.is_dir() {
        for entry in std::fs::read_dir(&snaps)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".tmp"))
            {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    for entry in std::fs::read_dir(data_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entry.file_name() == "snapshots" {
            continue;
        }
        recover_bundle(&entry.path().join("bundle"))?;
        for junk in ["bundle.new", "sandbox.json.tmp"] {
            let p = entry.path().join(junk);
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    Ok(())
}

/// Recover the last committed generation if publication stopped between renames.
/// Sync the recovered name before discarding either generation.
fn recover_bundle(bundle: &Path) -> Result<()> {
    let old = bundle.with_extension("old");
    if old.exists() {
        if !bundle.exists() {
            std::fs::rename(&old, bundle)?;
        }
        sync_dir(bundle.parent().expect("bundle parent"))?;
        if old.exists() {
            std::fs::remove_dir_all(old)?;
        }
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Flush every artifact and directory before publishing its generation.
fn sync_tree(dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path())?;
        } else {
            std::fs::File::open(entry.path())?.sync_all()?;
        }
    }
    sync_dir(dir)
}

/// Publish a durable generation. Startup recovers `bundle.old` if interrupted
/// between renames; the old generation is removed only after the new name is durable.
fn publish_bundle(new_dir: &Path, bundle: &Path) -> Result<()> {
    recover_bundle(bundle)?;
    sync_tree(new_dir)?;
    let parent = bundle.parent().expect("bundle parent");
    let old_dir = bundle.with_extension("old");
    if bundle.exists() {
        std::fs::rename(bundle, &old_dir)?;
        if let Err(e) = sync_dir(parent) {
            // Restore the visible recovery point even on an fsync failure.
            recover_bundle(bundle)?;
            return Err(e);
        }
    }
    if let Err(e) = std::fs::rename(new_dir, bundle) {
        recover_bundle(bundle)?;
        return Err(e.into());
    }
    sync_dir(parent)?;
    if old_dir.exists() {
        std::fs::remove_dir_all(old_dir)?;
        sync_dir(parent)?;
    }
    Ok(())
}

/// Recursive directory copy (snapshot registry freezes). Small bundles
/// only (one sandbox snapshot); no hardlink tricks to reason about.
fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

impl KrucibleBackend {
    fn read_session_output(
        &self,
        id: &str,
        session_id: &str,
        from_seq: u64,
        budget: Duration,
        first_chunk: bool,
    ) -> Result<SessionChunk> {
        let dir = self.live_dir(id)?;
        let sock = forge_sock(&dir);
        let mut c = connect_rpc(&sock, CONNECT_BUDGET)?;
        c.set_read_timeout(Some(budget)).map_err(Error::Io)?;
        // Bound the guest side slightly below our own budget so the forge
        // attach always closes first: otherwise every idle poll would leak
        // a guest thread holding a dead connection (see Attach timeout_ms).
        let guest_ms = budget
            .saturating_sub(Duration::from_millis(500))
            .as_millis() as u64;
        let body = serde_json::json!({ "op": "attach", "session_id": session_id, "from_seq": from_seq, "timeout_ms": guest_ms });
        let payload = serde_json::to_vec(&body)?;
        write_frame(
            &mut c,
            &Frame {
                msg_type: FrameType::SessionReq,
                payload,
            },
        )
        .map_err(|e| Error::Control(format!("session attach write: {e}")))?;
        // Drain SessionData frames until EOF or the budget (read timeout)
        // runs out; a timeout returns whatever arrived (eof: false).
        // Resume cursors come from the frames (seq + bytes), never from
        // client-side byte counting: scrollback eviction makes counting
        // wrong and silently duplicated.
        let mut r = BufReader::new(c.try_clone().map_err(Error::Io)?);
        let mut out = Vec::new();
        let mut exit_code = None;
        let mut next_seq = from_seq;
        let mut truncated = false;
        let deadline = Instant::now() + budget;
        loop {
            if Instant::now() > deadline {
                return Ok(SessionChunk {
                    data: out,
                    eof: false,
                    exit_code,
                    next_seq,
                    truncated,
                });
            }
            let f = match read_frame(&mut r) {
                Ok(f) => f,
                Err(_) => {
                    return Ok(SessionChunk {
                        data: out,
                        eof: false,
                        exit_code,
                        next_seq,
                        truncated,
                    })
                }
            };
            if f.msg_type == FrameType::Error {
                let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap_or_default();
                let msg = v["message"].as_str().unwrap_or("forge error").to_string();
                if msg.starts_with("no such session") {
                    return Err(Error::NotFound(msg));
                }
                return Err(Error::Control(msg));
            }
            if f.msg_type != FrameType::SessionData {
                return Err(Error::Control(format!(
                    "want SessionData, got {:?}",
                    f.msg_type
                )));
            }
            let v: serde_json::Value = serde_json::from_slice(&f.payload)
                .map_err(|e| Error::Control(format!("session data JSON: {e}")))?;
            let bytes = forge_b64(&v, "data_b64")?;
            let seq = v["seq"].as_u64().unwrap_or(next_seq);
            if seq > next_seq {
                truncated = true;
            }
            if v["truncated"].as_bool().unwrap_or(false) {
                truncated = true;
            }
            next_seq = seq.saturating_add(bytes.len() as u64);
            out.extend_from_slice(&bytes);
            exit_code = v["exit_code"].as_i64().map(|c| c as i32).or(exit_code);
            let eof = v["eof"].as_bool().unwrap_or(false);
            if eof || (first_chunk && !out.is_empty()) {
                return Ok(SessionChunk {
                    data: out,
                    eof,
                    exit_code,
                    next_seq,
                    truncated,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VmmId;

    fn cfg(dir: &Path) -> KrucibleConfig {
        KrucibleConfig {
            vmm_bin: dir.join("vmm"),
            base_image: dir.join("base.ext4"),
            data_dir: dir.join("data"),
            lib_path: "/tmp/kvm".to_string(),
            ready_timeout: Duration::from_secs(1),
            exec_timeout: Duration::from_secs(1),
            network: None,
            resources: None,
        }
    }

    fn spec_named(name: &str) -> SandboxSpec {
        SandboxSpec {
            name: name.to_string(),
            cpus: 1,
            memory_mb: 512,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            desktop: false,
            desktop_gpu: false,
            extra_env: HashMap::new(),
        }
    }

    fn write_record(dir: &Path, id: &str, state: State) {
        let spec = SandboxSpec {
            name: id.to_string(),
            cpus: 1,
            memory_mb: 512,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            desktop: false,
            desktop_gpu: false,
            extra_env: HashMap::new(),
        };
        let rec = SandboxRecord {
            spec,
            info: SandboxInfo {
                id: id.to_string(),
                name: id.to_string(),
                state,
                thermal: Thermal::Cold,
                ip: "10.42.0.9".to_string(),
            },
            backing: PathBuf::from("/tmp/kvm/rust-guest.ext4"),
        };
        let d = dir.join("data").join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("sandbox.json"),
            serde_json::to_vec_pretty(&rec).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn desktop_worker_defaults_to_software_with_optional_gpu() {
        let dir = crate::test_scratch("desktop-spec");
        std::fs::write(dir.join("vmm"), "x").unwrap();
        std::fs::write(dir.join("base.ext4"), "x").unwrap();
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        let mut spec = spec_named("desktop");
        // Persisted records from before desktop support keep ordinary behavior.
        let mut legacy = serde_json::to_value(&spec).unwrap();
        legacy.as_object_mut().unwrap().remove("desktop");
        assert!(
            !serde_json::from_value::<SandboxSpec>(legacy)
                .unwrap()
                .desktop
        );
        spec.desktop = true;
        let path = be
            .write_worker_spec(&dir, &dir.join("root.qcow2"), &spec, None)
            .unwrap();
        let software: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(software["gpu"], false);
        assert!(software.get("control_socket_uds").is_none());
        for enabled in [false, true] {
            spec.desktop = enabled;
            spec.desktop_gpu = enabled;
            let path = be
                .write_worker_spec(&dir, &dir.join("root.qcow2"), &spec, None)
                .unwrap();
            let js: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(
                js.get("gpu").and_then(|v| v.as_bool()).unwrap_or(false),
                enabled
            );
            assert_eq!(js.get("control_socket_uds").is_some(), !enabled);
            assert!(js.get("snapshot_dir").is_none());
        }
    }

    #[test]
    fn config_validation_rejects_missing_files() {
        let dir = crate::test_scratch("krucible-cfg");
        assert!(KrucibleBackend::open(cfg(&dir)).is_err());
    }

    #[test]
    fn ids_and_snapshot_ids_are_validated_without_touching_disk() {
        let dir = crate::test_scratch("krucible-ids");
        std::fs::write(dir.join("vmm"), "x").unwrap();
        std::fs::write(dir.join("base.ext4"), "x").unwrap();
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        let spec = SandboxSpec {
            name: "../evil".to_string(),
            cpus: 1,
            memory_mb: 512,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            desktop: false,
            desktop_gpu: false,
            extra_env: HashMap::new(),
        };
        assert!(matches!(be.create(&spec), Err(Error::InvalidState(_))));
        assert!(matches!(be.status("nope"), Err(Error::NotFound(_))));
        assert!(matches!(be.destroy("nope"), Err(Error::NotFound(_))));
        assert!(matches!(
            be.exec("nope", &["true".to_string()]),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(be.start("nope"), Err(Error::NotFound(_))));
        assert!(matches!(be.stop("nope"), Err(Error::NotFound(_))));
        assert!(matches!(
            be.create_snapshot("nope", "s"),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(be.fork("nope", "x"), Err(Error::NotFound(_))));

        // Custom kernels are explicitly rejected (no silent fallback).
        let mut kspec = spec_named("kexec");
        kspec.kernel_image = Some("/tmp/kvm/custom-kernel".to_string());
        assert!(matches!(be.create(&kspec), Err(Error::InvalidState(_))));
        // Registry miss (not compat) when the snapshot id is unknown.
        let snap = SnapshotManifest::new(
            "ghost",
            crate::Compat {
                arch: std::env::consts::ARCH.to_string(),
                vmm: crate::VmmId {
                    name: KRUCIBLE_VMM_NAME.into(),
                    version: KRUCIBLE_VMM_VERSION.into(),
                },
                kernel_digest: KRUCIBLE_KERNEL_DIGEST.into(),
                mem_mib: 512,
                vcpus: 1,
                device_layout_ver: crate::DEVICE_LAYOUT_VER,
            },
            crate::Artifacts {
                memory_bytes: 1,
                root_delta_bytes: 1,
            },
        );
        assert!(matches!(be.restore(&snap, "new"), Err(Error::NotFound(_))));
        // The STORED bundle is authoritative: a compatible caller
        // manifest for a tampered registry entry must still refuse as
        // incompatible (caller metadata never reaches the worker).
        let reg = dir.join("data").join("snapshots").join("tampered");
        let mut stored_bad = snap.clone();
        stored_bad.snapshot_id = "tampered".to_string();
        stored_bad.compat.vmm = VmmId {
            name: "firecracker".to_string(),
            version: "1.0".to_string(),
        };
        stored_bad.write_to(&reg).unwrap();
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        let mut caller = snap.clone();
        caller.snapshot_id = "tampered".to_string();
        assert!(matches!(
            be.restore(&caller, "new2"),
            Err(Error::Incompatible(_))
        ));
        // Registry key and stored manifest id must agree, else InvalidState
        // (never a boot from the wrong bundle).
        let reg2 = dir.join("data").join("snapshots").join("mismatch");
        let mut stored_other = snap.clone();
        stored_other.snapshot_id = "nope".to_string();
        stored_other.write_to(&reg2).unwrap();
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        let mut caller2 = snap.clone();
        caller2.snapshot_id = "mismatch".to_string();
        assert!(matches!(
            be.restore(&caller2, "new3"),
            Err(Error::InvalidState(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lost_pause_reply_requires_confirmed_resume_before_reporting_running() {
        let dir = crate::test_scratch("lost-pause-reply");
        std::fs::write(dir.join("vmm"), "x").unwrap();
        std::fs::write(dir.join("base.ext4"), "x").unwrap();
        write_record(&dir, "vm", State::Running);
        let live = dir.join("data/vm");
        Worker {
            id: "vm".into(),
            pid: std::process::id(),
            sock_dir: live.clone(),
            state_path: live.join("state.json"),
            starttime: crate::process_starttime(std::process::id()),
        }
        .persist()
        .unwrap();
        std::fs::create_dir_all(sock_dir(&live)).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(control_sock(&live)).unwrap();
        let control = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            for (command, reply) in [
                ("STATUS", "OK running\n"),
                ("PAUSE", ""),
                ("STATUS", "OK paused\n"),
                ("RESUME", "ERR resume unavailable\n"),
                ("STATUS", "OK paused\n"),
                ("RESUME", "OK running\n"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                std::io::BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                assert_eq!(line.trim(), command);
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        assert!(be.stop("vm").is_err());
        assert_eq!(be.status("vm").unwrap().state, State::Failed);
        be.start("vm").unwrap();
        assert_eq!(be.status("vm").unwrap().state, State::Running);
        control.join().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_adoption_probe_stays_failed_until_start_recovers_control() {
        let dir = crate::test_scratch("adoption-probe-retry");
        std::fs::write(dir.join("vmm"), "x").unwrap();
        std::fs::write(dir.join("base.ext4"), "x").unwrap();
        write_record(&dir, "vm", State::Running);
        let live = dir.join("data/vm");
        Worker {
            id: "vm".into(),
            pid: std::process::id(),
            sock_dir: live.clone(),
            state_path: live.join("state.json"),
            starttime: crate::process_starttime(std::process::id()),
        }
        .persist()
        .unwrap();
        std::fs::create_dir_all(sock_dir(&live)).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(control_sock(&live)).unwrap();
        let control = std::thread::spawn(move || {
            use std::io::{BufRead, Write};
            for (command, reply) in [
                ("STATUS", "ERR temporarily unavailable\n"),
                ("STATUS", "OK paused\n"),
                ("RESUME", "OK running\n"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                std::io::BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                assert_eq!(line.trim(), command);
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        assert_eq!(be.status("vm").unwrap().state, State::Failed);
        assert_eq!(be.status("vm").unwrap().state, State::Failed);
        be.start("vm").unwrap();
        assert_eq!(be.status("vm").unwrap().state, State::Running);
        control.join().unwrap();
    }

    #[test]
    fn open_adopts_live_pids_and_marks_dead_failed() {
        let dir = crate::test_scratch("krucible-recover");
        std::fs::write(dir.join("vmm"), "x").unwrap();
        std::fs::write(dir.join("base.ext4"), "x").unwrap();
        // Dead worker record: pid that cannot exist.
        write_record(&dir, "dead-vm", State::Running);
        let dead = dir.join("data").join("dead-vm");
        let worker = Worker {
            id: "dead-vm".to_string(),
            pid: 999_999_999,
            sock_dir: dead.clone(),
            state_path: dead.join("state.json"),
            starttime: None,
        };
        worker.persist().unwrap();
        // Live record: our own test pid is alive by definition, adopted
        // only because the recorded starttime matches.
        write_record(&dir, "live-vm", State::Running);
        let live = dir.join("data").join("live-vm");
        let worker = Worker {
            id: "live-vm".to_string(),
            pid: std::process::id(),
            sock_dir: live.clone(),
            state_path: live.join("state.json"),
            starttime: crate::process_starttime(std::process::id()),
        };
        worker.persist().unwrap();
        // PID-reuse simulation: our own (live) pid with a WRONG starttime
        // must NOT be adopted — destroy must never signal it.
        write_record(&dir, "reused-vm", State::Running);
        let reused = dir.join("data").join("reused-vm");
        let worker = Worker {
            id: "reused-vm".to_string(),
            pid: std::process::id(),
            sock_dir: reused.clone(),
            state_path: reused.join("state.json"),
            starttime: Some(
                crate::process_starttime(std::process::id())
                    .unwrap_or(0)
                    .wrapping_add(1_000_000),
            ),
        };
        worker.persist().unwrap();

        #[cfg(not(target_os = "linux"))]
        {
            assert!(matches!(
                KrucibleBackend::open(cfg(&dir)),
                Err(Error::InvalidState(_))
            ));
            std::fs::remove_file(live.join("state.json")).unwrap();
        }
        #[cfg(target_os = "linux")]
        let control = {
            std::fs::create_dir_all(sock_dir(&live)).unwrap();
            let listener = std::os::unix::net::UnixListener::bind(control_sock(&live)).unwrap();
            std::thread::spawn(move || {
                use std::io::{BufRead, Write};
                for (command, reply) in [("STATUS", "OK paused\n"), ("RESUME", "OK running\n")] {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut line = String::new();
                    std::io::BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap();
                    assert_eq!(line.trim(), command);
                    stream.write_all(reply.as_bytes()).unwrap();
                }
            })
        };
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        assert_eq!(be.status("dead-vm").unwrap().state, State::Failed);
        #[cfg(target_os = "linux")]
        control.join().unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(be.status("live-vm").unwrap().state, State::Running);
        assert_eq!(
            be.status("reused-vm").unwrap().state,
            State::Failed,
            "pid with mismatched starttime must not be adopted"
        );
        // Destroying the adopted live record must NOT signal our own pid:
        // remove its state.json first so destroy takes the no-worker path.
        // (Adopted terminate sends SIGTERM; never point it at yourself.)
        let _ = std::fs::remove_file(live.join("state.json"));
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        be.destroy("live-vm").unwrap();
        assert!(matches!(be.status("live-vm"), Err(Error::NotFound(_))));
        be.destroy("dead-vm").unwrap();
        // The reused-pid record has no adopted worker (identity mismatch),
        // so destroy only removes files — our own pid is never signalled.
        be.destroy("reused-vm").unwrap();
        assert!(matches!(be.status("reused-vm"), Err(Error::NotFound(_))));
    }

    #[test]
    fn publish_bundle_swaps_generations_atomically() {
        let dir = crate::test_scratch("publish");
        let bundle = dir.join("bundle");
        // First publish (no previous bundle to preserve).
        let gen1 = dir.join("g1");
        std::fs::create_dir_all(gen1.join("sub")).unwrap();
        std::fs::write(gen1.join("v"), b"one").unwrap();
        publish_bundle(&gen1, &bundle).unwrap();
        assert_eq!(std::fs::read(bundle.join("v")).unwrap(), b"one");
        // Second publish replaces; the old generation never lingers.
        let gen2 = dir.join("g2");
        std::fs::create_dir_all(&gen2).unwrap();
        std::fs::write(gen2.join("v"), b"two").unwrap();
        publish_bundle(&gen2, &bundle).unwrap();
        assert_eq!(std::fs::read(bundle.join("v")).unwrap(), b"two");
        assert!(!dir.join("bundle.old").exists());
        assert!(!gen2.exists(), "generation consumed by rename");
    }
    #[test]
    fn reservation_released_while_map_is_locked() {
        let dir = crate::test_scratch("reservation-map");
        std::fs::write(dir.join("vmm"), "x").unwrap();
        std::fs::write(dir.join("base.ext4"), "x").unwrap();
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        let guard = OpGuard::take(&be, "vm").unwrap();
        assert!(matches!(OpGuard::take(&be, "vm"), Err(Error::Conflict(_))));
        let _map = be.lock();
        drop(guard);
        assert!(OpGuard::take(&be, "vm").is_ok());
    }

    #[test]
    fn reservation_released_on_unwind() {
        let dir = crate::test_scratch("reservation-unwind");
        std::fs::write(dir.join("vmm"), "x").unwrap();
        std::fs::write(dir.join("base.ext4"), "x").unwrap();
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        let result = std::panic::catch_unwind(|| {
            let _guard = OpGuard::take(&be, "vm").unwrap();
            panic!("operation failed");
        });
        assert!(result.is_err());
        assert!(OpGuard::take(&be, "vm").is_ok());
    }

    #[test]
    fn reopen_recovers_each_publication_crash_boundary() {
        // Before first rename, between renames, after second rename.
        for stage in 0..3 {
            let dir = crate::test_scratch(&format!("publish-crash-{stage}"));
            std::fs::write(dir.join("vmm"), "x").unwrap();
            std::fs::write(dir.join("base.ext4"), "x").unwrap();
            let vm = dir.join("data/vm");
            std::fs::create_dir_all(vm.join("bundle")).unwrap();
            std::fs::create_dir_all(vm.join("bundle.new")).unwrap();
            std::fs::write(vm.join("bundle/v"), "previous").unwrap();
            std::fs::write(vm.join("bundle.new/v"), "next").unwrap();
            if stage >= 1 {
                std::fs::rename(vm.join("bundle"), vm.join("bundle.old")).unwrap();
            }
            if stage >= 2 {
                std::fs::rename(vm.join("bundle.new"), vm.join("bundle")).unwrap();
            }
            for _ in 0..2 {
                let _be = KrucibleBackend::open(cfg(&dir)).unwrap();
                assert_eq!(
                    std::fs::read_to_string(vm.join("bundle/v")).unwrap(),
                    if stage == 2 { "next" } else { "previous" }
                );
                assert!(!vm.join("bundle.old").exists());
                assert!(!vm.join("bundle.new").exists());
            }
        }
    }

    #[test]
    fn failed_publish_preserves_previous_bundle() {
        let dir = crate::test_scratch("publish-failure");
        let bundle = dir.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::write(bundle.join("v"), "previous").unwrap();
        assert!(publish_bundle(&dir.join("missing"), &bundle).is_err());
        assert_eq!(
            std::fs::read_to_string(bundle.join("v")).unwrap(),
            "previous"
        );
    }

    #[test]
    fn unknown_live_identity_refuses_adoption() {
        let dir = crate::test_scratch("unknown-identity");
        std::fs::write(dir.join("vmm"), "x").unwrap();
        std::fs::write(dir.join("base.ext4"), "x").unwrap();
        write_record(&dir, "vm", State::Running);
        let vm = dir.join("data/vm");
        Worker {
            id: "vm".into(),
            pid: std::process::id(),
            sock_dir: vm.clone(),
            state_path: vm.join("state.json"),
            starttime: None,
        }
        .persist()
        .unwrap();
        assert!(matches!(
            KrucibleBackend::open(cfg(&dir)),
            Err(Error::InvalidState(_))
        ));
        assert!(vm.join("state.json").exists());
    }
}
