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
    host_caps, is_alive, process_starttime, send_ctl, spawn_worker_cfg, terminate_adopted,
    Backend, BackendKind, Capabilities, Error, ExecResult, LiveWorker, Result, SandboxInfo,
    SandboxSpec, SnapshotManifest, SpawnConfig, State, Thermal, Worker, MAX_EXEC_OUTPUT,
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
}

impl KrucibleConfig {
    pub fn new(
        vmm_bin: PathBuf,
        base_image: PathBuf,
        data_dir: PathBuf,
        lib_path: String,
    ) -> Self {
        Self {
            vmm_bin,
            base_image,
            data_dir,
            lib_path,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            exec_timeout: DEFAULT_EXEC_TIMEOUT,
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
enum WorkerHandle {
    Owned(LiveWorker),
    Adopted(Worker),
}

impl WorkerHandle {

    /// True while the worker process exists and is not a zombie.
    /// Reaps an owned child that already exited (no zombie left behind).
    /// Adopted pids additionally require identity verification: a reused
    /// pid is NOT our worker, however live it looks.
    fn alive(&mut self) -> bool {
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
    /// Adopted: verify identity, SIGTERM, then bounded pid poll with
    /// reuse checks — a pid that changed hands mid-kill is left alone:
    /// our goal (no worker for this sandbox) is already achieved.
    fn terminate(&mut self) -> Result<()> {
        match self {
            WorkerHandle::Owned(w) => w.terminate().map_err(Error::Io),
            WorkerHandle::Adopted(w) => {
                if !is_alive(w.pid) || !verified(w) {
                    // Already gone, or the pid belongs to someone else now.
                    return Ok(());
                }
                terminate_adopted(w.pid)?;
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
    /// Sandbox ids with a mutating operation in flight (create / restore /
    /// start / stop / destroy / fork / snapshot). A second operation on the
    /// same id fails fast with Conflict instead of racing on overlays,
    /// sockets, and records. Cross-sandbox operations still run concurrently.
    busy: HashSet<String>,
}

impl Inner {
    fn try_reserve(&mut self, id: &str) -> Result<()> {
        if self.busy.contains(id) {
            return Err(Error::Conflict(format!(
                "sandbox {id}: operation already in progress"
            )));
        }
        self.busy.insert(id.to_string());
        Ok(())
    }

    fn release(&mut self, id: &str) {
        self.busy.remove(id);
    }
}

/// Holds one sandbox's reservation until the operation completes.
/// Drop releases even on panic paths (via `try_lock`: never block in Drop).
struct OpGuard<'a> {
    be: &'a KrucibleBackend,
    id: String,
}

impl<'a> OpGuard<'a> {
    fn take(be: &'a KrucibleBackend, id: &str) -> Result<Self> {
        be.lock().try_reserve(id)?;
        Ok(Self {
            be,
            id: id.to_string(),
        })
    }
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.be.inner.try_lock() {
            inner.release(&self.id);
        }
    }
}

/// Process identity for adoption: a recorded starttime must match the live
/// pid, defeating PID reuse. Legacy records (`None`) fall back to plain
/// liveness — documented, and only pre-identity records take that path.
fn verified(w: &Worker) -> bool {
    match w.starttime {
        Some(t) => process_starttime(w.pid) == Some(t),
        None => true,
    }
}

/// Real krucible backend. Synchronous and blocking (the daemon dispatches
/// off its async core); internally a single map lock with take-operate-
/// reinsert discipline so long worker waits never hold the lock.
#[derive(Debug)]
pub struct KrucibleBackend {
    cfg: KrucibleConfig,
    inner: Mutex<Inner>,
}

impl KrucibleBackend {
    /// Open (or create) `cfg.data_dir`, adopting live workers from
    /// `state.json` records. Dead pids surface as `Failed`.
    pub fn open(cfg: KrucibleConfig) -> Result<Self> {
        cfg.validate()?;
        std::fs::create_dir_all(&cfg.data_dir)?;
        // Crash hygiene: a failed snapshot publish or registry copy may
        // leave `<name>.tmp` / `bundle.new` / `bundle.old` behind. The live
        // bundle is only ever swapped by rename, so these are always safe
        // to drop — and dropping them keeps restores from ever seeing a
        // half-written generation.
        sweep_debris(&cfg.data_dir)?;
        let mut inner = Inner {
            sandboxes: HashMap::new(),
            snapshots: HashMap::new(),
            next_ip_octet: 2,
            busy: HashSet::new(),
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
                Ok(w) if is_alive(w.pid) && verified(&w) => {
                    Some(WorkerHandle::Adopted(w))
                }
                _ => None,
            };
            let mut info = record.info.clone();
            if worker.is_some() {
                info.state = State::Running;
            } else if info.state == State::Running {
                info.state = State::Failed;
            }
            inner.next_ip_octet = inner.next_ip_octet.max(2);
            inner.sandboxes.insert(
                id,
                LiveRec {
                    record: SandboxRecord { info, ..record },
                    dir,
                    worker,
                },
            );
        }
        Ok(Self {
            cfg,
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("krucible mutex poisoned")
    }

    fn host_caps(&self) -> crate::HostCaps {
        host_caps(KRUCIBLE_VMM_NAME, KRUCIBLE_VMM_VERSION, KRUCIBLE_KERNEL_DIGEST)
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
                c.set_write_timeout(Some(RPC_TIMEOUT))
                    .map_err(Error::Io)?;
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
    let mut c = connect_rpc(sock, CONNECT_BUDGET)?;
    // The reply may legitimately take the whole execution budget.
    c.set_read_timeout(Some(exec_timeout)).map_err(Error::Io)?;
    let body = serde_json::json!({ "argv": argv }).to_string().into_bytes();
    write_frame(&mut c, &Frame { msg_type: FrameType::ExecReq, payload: body })
        .map_err(|e| Error::Control(format!("exec write: {e}")))?;
    let mut r = BufReader::new(c.try_clone().map_err(Error::Io)?);
    let f = read_frame(&mut r).map_err(|e| Error::Control(format!("exec read: {e}")))?;
    if f.msg_type != FrameType::ExecResp {
        return Err(Error::Control(format!(
            "want ExecResp, got {:?}",
            f.msg_type
        )));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&f.payload).map_err(|e| Error::Control(format!("exec JSON: {e}")))?;
    let decode = |k: &str| -> Result<String> {
        let s = v[k].as_str().ok_or_else(|| Error::Control(format!("exec reply lacks {k}")))?;
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            s,
        )
        .map_err(|e| Error::Control(format!("exec {k} not base64: {e}")))?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
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
    let (stdout, t1) = cap(decode("stdout_b64")?);
    let (stderr, t2) = cap(decode("stderr_b64")?);
    Ok(ExecResult {
        exit_code: exit_code as i32,
        stdout,
        stderr,
        truncated: guest_truncated || t1 || t2,
    })
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
        // Omit (never null): an explicit null is a parse error for workers.
        if let Some(bundle) = snapshot_dir {
            js["snapshot_dir"] =
                serde_json::Value::String(bundle.to_string_lossy().into_owned());
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
        let spec_path = self.write_worker_spec(dir, overlay, spec, snapshot_dir)?;
        // Stale bridge sockets from a previous worker would steal connects.
        for stale in ["c.sock", "f.sock", "control.sock"] {
            let _ = std::fs::remove_file(sock_dir(dir).join(stale));
        }
        let env = vec![
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
            "HOME=/root".to_string(),
            "LANG=C.UTF-8".to_string(),
            format!("LD_LIBRARY_PATH={}", self.cfg.lib_path),
        ];
        spawn_worker_cfg(&SpawnConfig {
            vmm_binary: self.cfg.vmm_bin.as_os_str(),
            spec_arg: &spec_path,
            state_path: &dir.join("state.json"),
            hermetic: true,
            env: &env,
            stderr_log: Some(&dir.join("vmm.log")),
        })
        .map_err(Error::Io)
    }

    /// Online snapshot: PAUSE, SNAPSHOT into a fresh generation dir,
    /// freeze the overlay, write the sidecar, atomically publish, RESUME.
    /// The VM stays live (fork/create_snapshot path). A failed or
    /// interrupted snapshot never touches the previous recovery point:
    /// only a fully-written generation is published by rename.
    /// Returns the bundle dir.
    fn snapshot_live(&self, dir: &Path, rec: &SandboxRecord, snapshot_id: &str) -> Result<PathBuf> {
        validate_snapshot_id(snapshot_id)?;
        let bundle = dir.join("bundle");
        let gen = dir.join("bundle.new");
        let _ = std::fs::remove_dir_all(&gen);
        let ctl = control_sock(dir);
        let reply = send_ctl(&ctl, "PAUSE")?;
        if !reply.starts_with("OK") {
            return Err(Error::Control(format!("PAUSE refused: {reply}")));
        }
        let snap = (|| -> Result<()> {
            let reply = send_ctl(&ctl, &format!("SNAPSHOT {}", gen.display()))?;
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
        // A failed snapshot must not leave the guest frozen.
        match snap {
            Ok(()) => {
                let reply = send_ctl(&ctl, "RESUME")?;
                if !reply.starts_with("OK") {
                    return Err(Error::Control(format!("RESUME refused: {reply}")));
                }
                Ok(bundle)
            }
            Err(e) => {
                let _ = send_ctl(&ctl, "RESUME");
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
        if let Err(e) = wait_ready(&forge_sock(dir), self.cfg.ready_timeout) {
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
        let tmp = self.cfg.data_dir.join("snapshots").join(format!("{snapshot_id}.tmp"));
        let _ = std::fs::remove_dir_all(&tmp);
        let published = (|| -> Result<SnapshotManifest> {
            copy_dir(&bundle, &tmp)?;
            std::fs::rename(&tmp, &reg)?;
            SnapshotManifest::read_from(&reg)
        })();
        if published.is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        let manifest = published?;
        self.lock()
            .snapshots
            .insert(snapshot_id.to_string(), reg);
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
            if let Err(e) = wait_ready(&forge_sock(&dir), self.cfg.ready_timeout) {
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
                    ip: format!("10.42.0.{octet}"),
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
                        record,
                        dir,
                        worker: Some(WorkerHandle::Owned(worker)),
                    },
                );
                Ok(info)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                Err(e)
            }
        }
    }
}

impl Backend for KrucibleBackend {
    fn capabilities(&self) -> Capabilities {
        BackendKind::Krucible.capabilities()
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
            if let Err(e) = wait_ready(&forge_sock(&dir), self.cfg.ready_timeout) {
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
                    ip: format!("10.42.0.{octet}"),
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
                        record,
                        dir,
                        worker: Some(WorkerHandle::Owned(worker)),
                    },
                );
                Ok(info)
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
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
        std::fs::remove_dir_all(&rec.dir)?;
        Ok(())
    }

    fn start(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        let _guard = OpGuard::take(self, id)?;
        // Snapshot the decision inputs under lock, operate unlocked.
        // Liveness is reconciled here (not trusted from cache): a worker
        // that died behind a cached Running state reboots below instead
        // of reporting a success that is not true.
        enum Plan {
            Restore { dir: PathBuf, spec: SandboxSpec, bundle: PathBuf },
            Fresh { dir: PathBuf, spec: SandboxSpec },
        }
        let plan = {
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
                if rec.record.info.state == State::Running {
                    rec.record.info.state = State::Failed;
                    let record = rec.record.clone();
                    let dir = rec.dir.clone();
                    drop(inner);
                    let _ = self.persist_record(&dir, &record);
                }
            } else if rec.record.info.state == State::Running {
                return Ok(());
            }
            // Re-acquire after the persist above (if any).
            let inner = self.lock();
            let rec = inner
                .sandboxes
                .get(id)
                .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
            if rec.dir.join("bundle").join("manifest.json").is_file() {
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
            }
        };
        match plan {
            Plan::Restore { dir, spec, bundle } => {
                let worker = self.boot_from_bundle(&dir, &spec, &bundle)?;
                let mut inner = self.lock();
                if let Some(rec) = inner.sandboxes.get_mut(id) {
                    rec.record.info.state = State::Running;
                    rec.record.info.thermal = Thermal::Warm;
                    rec.worker = Some(WorkerHandle::Owned(worker));
                    let record = rec.record.clone();
                    self.persist_record(&dir, &record)?;
                }
                Ok(())
            }
            Plan::Fresh { dir, spec } => {
                let worker = self.boot_worker(&dir, &dir.join("root.qcow2"), &spec, None)?;
                if let Err(e) = wait_ready(&forge_sock(&dir), self.cfg.ready_timeout) {
                    let mut w = worker;
                    let _ = w.terminate();
                    return Err(e);
                }
                let mut inner = self.lock();
                if let Some(rec) = inner.sandboxes.get_mut(id) {
                    rec.record.info.state = State::Running;
                    rec.record.info.thermal = Thermal::Hot;
                    rec.worker = Some(WorkerHandle::Owned(worker));
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
            inner.sandboxes.get(id).map(|r| r.record.clone()).ok_or_else(|| {
                Error::NotFound(format!("sandbox {id}"))
            })?
        };
        self.snapshot_live(&dir, &record, &format!("stop-{id}"))?;
        let worker = {
            let mut inner = self.lock();
            inner.sandboxes.get_mut(id).and_then(|r| r.worker.take())
        };
        if let Some(mut handle) = worker {
            handle.terminate()?;
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

    fn exec(&self, id: &str, argv: &[String]) -> Result<ExecResult> {
        validate_id(id)?;
        let dir = {
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
            rec.dir.clone()
        };
        rpc_exec(&forge_sock(&dir), argv, self.cfg.exec_timeout)
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
/// records (publish swaps by rename, so debris is always unreferenced).
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
        for junk in ["bundle.new", "bundle.old", "sandbox.json.tmp"] {
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

/// Atomically publish a freshly-written bundle generation: rename the
/// previous bundle aside (if any), move the new generation into place,
/// then drop the old one. A crash before the second rename leaves the
/// previous recovery point intact; a crash between renames leaves NO
/// bundle (restore treats that as absent, never as torn).
fn publish_bundle(new_dir: &Path, bundle: &Path) -> Result<()> {
    let mut old_name = bundle.as_os_str().to_owned();
    old_name.push(".old");
    let old_dir = PathBuf::from(old_name);
    let _ = std::fs::remove_dir_all(&old_dir);
    if bundle.exists() {
        std::fs::rename(bundle, &old_dir)?;
    }
    let published = std::fs::rename(new_dir, bundle);
    if published.is_err() {
        // Best effort: put the previous generation back.
        if old_dir.exists() && !bundle.exists() {
            let _ = std::fs::rename(&old_dir, bundle);
        }
        published?;
    }
    let _ = std::fs::remove_dir_all(&old_dir);
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
            extra_env: HashMap::new(),
        };
        assert!(matches!(
            be.create(&spec),
            Err(Error::InvalidState(_))
        ));
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
        assert!(matches!(
            be.create(&kspec),
            Err(Error::InvalidState(_))
        ));
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
            starttime: Some(crate::process_starttime(std::process::id()).unwrap_or(0).wrapping_add(1_000_000)),
        };
        worker.persist().unwrap();

        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        assert_eq!(be.status("dead-vm").unwrap().state, State::Failed);
        assert_eq!(be.status("live-vm").unwrap().state, State::Running);
        assert_eq!(
            be.status("reused-vm").unwrap().state,
            State::Failed,
            "pid with mismatched starttime must not be adopted"
        );
        // Destroying the adopted live record must NOT signal our own pid:
        // remove its state.json first so destroy takes the no-worker path.
        // (Adopted terminate sends SIGTERM; never point it at yourself.)
        std::fs::remove_file(live.join("state.json")).unwrap();
        let be = KrucibleBackend::open(cfg(&dir)).unwrap();
        be.destroy("live-vm").unwrap();
        assert!(matches!(be.status("live-vm"), Err(Error::NotFound(_))));
        be.destroy("dead-vm").unwrap();
        // The reused-pid record has no adopted worker (identity mismatch),
        // so destroy only removes files — our own pid is never signalled.
        be.destroy("reused-vm").unwrap();
        assert!(matches!(
            be.status("reused-vm"),
            Err(Error::NotFound(_))
        ));
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
}
