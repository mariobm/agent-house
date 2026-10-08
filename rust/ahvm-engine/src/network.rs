//! Independent network processes, supervised without holding the sandbox map.
use crate::krucible::WorkerHandle;
use crate::{is_alive, spawn_worker_cfg, Error, Result, SpawnConfig, Worker};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub worker_broker: Option<crate::WorkerBrokerConfig>,
    pub lib_path: String,
    pub netd_bin: PathBuf,
    pub resolver: Ipv4Addr,
    /// Per-direction virtual Ethernet bytes/second; None leaves self-hosted links unlimited.
    pub bandwidth_bytes_per_sec: Option<u64>,
    /// Host-owned exact TCP destination grants, keyed by sandbox id.
    pub private_access: std::collections::BTreeMap<String, Vec<std::net::SocketAddrV4>>,
}

#[derive(Debug)]
struct Entry {
    cgroup: Option<PathBuf>,
    bandwidth_bytes_per_sec: Option<u64>,
    worker: Option<WorkerHandle>,
    vm: Option<Worker>,
    retry: Instant,
    backoff: RestartBackoff,
    running_since: Instant,
}

#[derive(Debug, Default)]
struct RestartBackoff {
    failures: u32,
}

impl RestartBackoff {
    fn failed(&mut self) -> Duration {
        let delay = Duration::from_secs(1u64 << self.failures.min(6));
        self.failures = self.failures.saturating_add(1);
        delay.min(Duration::from_secs(60))
    }

    fn healthy(&mut self, uptime: Duration) {
        // Merely creating the socket is not recovery: rapid crash loops keep
        // their backoff until the replacement has remained alive for a while.
        if uptime >= Duration::from_secs(30) {
            self.failures = 0;
        }
    }
}

#[derive(Debug)]
struct Core {
    cfg: NetworkConfig,
    entries: HashMap<PathBuf, Entry>,
}

impl Drop for Core {
    fn drop(&mut self) {
        for (_, entry) in self.entries.drain() {
            if let Some(WorkerHandle::Owned(w)) = entry.worker {
                w.reap_in_background();
            }
        }
    }
}

pub(crate) fn cleanup_orphan(dir: &Path) -> Result<()> {
    for name in ["state.json", "net-state.json"] {
        match Worker::load(dir.join(name)) {
            Ok(w) if alive(&w) => WorkerHandle::Adopted(w).terminate()?,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct Networks(Arc<Mutex<Core>>);

fn alive(w: &Worker) -> bool {
    // Used only in legacy fixtures. Production observation routes through the
    // root-owned broker identity and treats errors as unknown, never death.
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", w.pid)) else {
        return false;
    };
    let Some((_, fields)) = stat.rsplit_once(')') else {
        return false;
    };
    let fields: Vec<_> = fields.split_whitespace().collect();
    fields.first().is_some_and(|s| *s != "Z" && *s != "X")
        && w.starttime
            .is_some_and(|t| fields.get(19).and_then(|v| v.parse::<u64>().ok()) == Some(t))
}

fn worker_alive(w: &mut WorkerHandle) -> Result<bool> {
    w.alive()
}

impl Networks {
    pub(crate) fn new(cfg: NetworkConfig) -> Result<Self> {
        if !cfg!(target_os = "linux") {
            return Err(Error::InvalidState(
                "managed networking currently requires Linux process identity".into(),
            ));
        }
        if cfg.resolver.is_unspecified()
            || cfg.resolver.is_multicast()
            || cfg.resolver.is_broadcast()
        {
            return Err(Error::InvalidState(
                "DNS resolver must be a unicast IPv4 address".into(),
            ));
        }
        if cfg
            .bandwidth_bytes_per_sec
            .is_some_and(|v| !(65536..=1_000_000_000).contains(&v))
        {
            return Err(Error::InvalidState(
                "bandwidth must be 65536..=1000000000 bytes per second".into(),
            ));
        }
        if cfg.private_access.len() > 4096
            || cfg.private_access.iter().any(|(id, rules)| {
                id.is_empty()
                    || id.contains('/')
                    || rules.len() > 64
                    || rules
                        .iter()
                        .any(|endpoint| !ahvm_proto::valid_private_endpoint(*endpoint))
            })
        {
            return Err(Error::InvalidState("invalid private-access policy".into()));
        }
        if !cfg.netd_bin.is_file() {
            return Err(Error::InvalidState("network binary missing".into()));
        }
        if let Some(broker) = &cfg.worker_broker {
            let reply = broker.request(&crate::BrokerRequest {
                action: crate::BrokerAction::Check,
                id: String::new(),
                role: crate::WorkerRole::Netd,
                worker: None,
            })?;
            if reply.netd_bin != Some(cfg.netd_bin.canonicalize()?) {
                return Err(Error::InvalidState(
                    "network broker binary differs from daemon configuration".into(),
                ));
            }
        }
        let shared = Arc::new(Mutex::new(Core {
            cfg,
            entries: HashMap::new(),
        }));
        let weak = Arc::downgrade(&shared);
        std::thread::Builder::new()
            .name("netd-supervisor".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(250));
                let Some(core) = weak.upgrade() else { break };
                let mut core = core.lock().unwrap_or_else(|e| e.into_inner());
                let mut cfg = core.cfg.clone();
                core.entries.retain(|dir, entry| {
                    cfg.bandwidth_bytes_per_sec = entry.bandwidth_bytes_per_sec;
                    let vm_alive = entry.vm.as_ref().map(Worker::verified_alive).transpose();
                    if vm_alive.is_err() {
                        return true;
                    }
                    if vm_alive.ok().flatten() == Some(false) {
                        if let Some(w) = &mut entry.worker {
                            if w.terminate().is_err() {
                                return true;
                            }
                        }
                        return false;
                    }
                    let live = entry.worker.as_mut().map(worker_alive).transpose();
                    if live.is_err() {
                        return true;
                    }
                    if live.ok().flatten() == Some(true) {
                        entry.backoff.healthy(entry.running_since.elapsed());
                        return true;
                    }
                    if entry.worker.take().is_some() {
                        let delay = entry.backoff.failed();
                        entry.retry = Instant::now() + delay;
                        eprintln!(
                            "netd: worker exited for {}; retry in {}s; see netd.log",
                            dir.display(),
                            delay.as_secs()
                        );
                    }
                    if Instant::now() >= entry.retry {
                        match launch(&cfg, dir, entry.cgroup.as_deref()) {
                            Ok(w) => {
                                entry.worker = Some(w);
                                entry.running_since = Instant::now();
                                eprintln!("netd: restarted {}", dir.display());
                            }
                            Err(e) => {
                                let delay = entry.backoff.failed();
                                entry.retry = Instant::now() + delay;
                                eprintln!(
                                    "netd: restart {} failed: {e}; retry in {}s",
                                    dir.display(),
                                    delay.as_secs()
                                );
                            }
                        }
                    }
                    true
                });
            })?;
        Ok(Self(shared))
    }

    pub(crate) fn prepare(
        &self,
        dir: &Path,
        cgroup: Option<&Path>,
        bandwidth: Option<u64>,
    ) -> Result<()> {
        let mut core = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut cfg = core.cfg.clone();
        if let Some(bytes) = bandwidth {
            if bytes != 0 && !(65536..=1_000_000_000).contains(&bytes) {
                return Err(Error::InvalidState("invalid bandwidth".into()));
            }
            cfg.bandwidth_bytes_per_sec = (bytes != 0).then_some(bytes);
        }
        // Even when netd is dead, do not attach a new checksum-validating gateway
        // to an old live VMM which negotiated checksum/GSO offloads.
        let vm_alive = match Worker::load(dir.join("state.json")) {
            Ok(w) => w.verified_alive()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        if vm_alive {
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.join("net.json"))?)?;
            if saved["ethernet_contract"].as_u64() != Some(1) {
                return Err(Error::InvalidState(
                    "recreate legacy networked VMs before upgrading Ethernet contract".into(),
                ));
            }
        }
        // Replacing the association is serialized with the monitor: a previous
        // dead VM must not remove the new VM's network process during boot.
        if let Some(entry) = core.entries.get_mut(dir) {
            entry.vm = None;
            if entry
                .worker
                .as_mut()
                .map(worker_alive)
                .transpose()?
                .unwrap_or(false)
            {
                return Ok(());
            }
        }
        let worker = match Worker::load(dir.join("net-state.json")) {
            Ok(w) if w.isolation.is_some() => {
                let broker = cfg.worker_broker.as_ref().ok_or_else(|| {
                    Error::InvalidState(
                        "isolated network worker requires its configured broker".into(),
                    )
                })?;
                if w.isolation
                    .as_ref()
                    .is_none_or(|i| i.socket != broker.socket || i.role != crate::WorkerRole::Netd)
                {
                    return Err(Error::InvalidState(
                        "network broker adoption configuration mismatch".into(),
                    ));
                }
                if broker.status(&w)? {
                    WorkerHandle::Adopted(w)
                } else {
                    launch(&cfg, dir, cgroup)?
                }
            }
            Ok(w) if cfg.worker_broker.is_some() && alive(&w) => {
                return Err(Error::InvalidState(
                    "stop legacy gateway before enabling per-launch identities".into(),
                ));
            }
            Ok(w) if alive(&w) => {
                let saved: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(dir.join("net.json"))?)?;
                let saved_rules: Vec<std::net::SocketAddrV4> = serde_json::from_value(
                    saved
                        .get("private_access")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )?;
                if saved["bandwidth_bytes_per_sec"].as_u64() != cfg.bandwidth_bytes_per_sec {
                    return Err(Error::InvalidState(
                        "stop VMs before changing bandwidth limits".into(),
                    ));
                }
                if saved_rules != rules_for(&core.cfg, dir) {
                    return Err(Error::InvalidState(
                        "stop VMs before changing private access".into(),
                    ));
                }
                if saved["resolver"].as_str() != Some(core.cfg.resolver.to_string().as_str()) {
                    return Err(Error::InvalidState(
                        "stop VMs before changing their DNS resolver".into(),
                    ));
                }
                if let Some(group) = cgroup {
                    crate::resources::verify_member(group, w.pid)?;
                }
                WorkerHandle::Adopted(w)
            }
            Ok(w) if is_alive(w.pid) && w.starttime.is_none() => {
                return Err(Error::InvalidState(
                    "cannot adopt netd without process identity".into(),
                ));
            }
            Ok(_) => launch(&cfg, dir, cgroup)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => launch(&cfg, dir, cgroup)?,
            Err(e) => return Err(e.into()),
        };
        core.entries.insert(
            dir.to_owned(),
            Entry {
                cgroup: cgroup.map(Path::to_owned),
                bandwidth_bytes_per_sec: cfg.bandwidth_bytes_per_sec,
                worker: Some(worker),
                vm: None,
                retry: Instant::now(),
                backoff: RestartBackoff::default(),
                running_since: Instant::now(),
            },
        );
        Ok(())
    }

    pub(crate) fn attach(&self, dir: &Path, vm: Worker) {
        if let Some(entry) = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get_mut(dir)
        {
            entry.vm = Some(vm);
        }
    }

    pub(crate) fn remove(&self, dir: &Path) -> Result<()> {
        let mut core = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut entry) = core.entries.remove(dir) {
            if let Some(w) = &mut entry.worker {
                w.terminate()?;
            }
        } else if let Ok(w) = Worker::load(dir.join("net-state.json")) {
            if w.verified_alive()? {
                WorkerHandle::Adopted(w).terminate()?;
            }
        }
        let _ = std::fs::remove_file(dir.join("net-state.json"));
        let _ = std::fs::remove_file(dir.join("net/net.sock"));
        Ok(())
    }
}

fn rules_for(cfg: &NetworkConfig, dir: &Path) -> Vec<std::net::SocketAddrV4> {
    dir.file_name()
        .and_then(|s| s.to_str())
        .and_then(|id| cfg.private_access.get(id))
        .cloned()
        .unwrap_or_default()
}

fn launch(cfg: &NetworkConfig, dir: &Path, cgroup: Option<&Path>) -> Result<WorkerHandle> {
    let sock = dir.join("net");
    std::fs::create_dir_all(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o700))?;
    let socket = sock.join("net.sock");
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let spec = dir.join("net.json");
    let mut read_only: Vec<PathBuf> = cfg
        .lib_path
        .split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    read_only.push(spec.clone());
    for path in ["/lib", "/lib64", "/usr/lib", "/usr/lib64", "/etc/localtime"] {
        if Path::new(path).exists() {
            read_only.push(path.into());
        }
    }
    let policy = crate::WorkerSandbox {
        read_only,
        read_write: vec![sock.clone(), "/dev/urandom".into()],
        unix_connect: Vec::new(),
    };
    std::fs::write(
        &spec,
        serde_json::to_vec(&serde_json::json!({
            "ethernet_contract": 1, "socket": socket, "resolver": cfg.resolver, "private_access": rules_for(cfg, dir),
            "worker_sandbox": policy,
            "bandwidth_bytes_per_sec": cfg.bandwidth_bytes_per_sec,
        }))?,
    )?;
    let mut worker = if let Some(broker) = &cfg.worker_broker {
        broker.launch(dir, crate::WorkerRole::Netd)?
    } else {
        spawn_worker_cfg(&SpawnConfig {
            cgroup,
            vmm_binary: cfg.netd_bin.as_os_str(),
            spec_arg: &spec,
            state_path: &dir.join("net-state.json"),
            hermetic: true,
            env: &["PATH=/usr/bin:/bin".into()],
            stderr_log: Some(&dir.join("netd.log")),
        })?
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if worker.try_reap()?.is_some() || Instant::now() >= deadline {
            worker.terminate()?;
            return Err(Error::Control(format!(
                "netd {} not ready; see netd.log",
                dir.display()
            )));
        }
        if socket.exists() {
            return Ok(WorkerHandle::Owned(worker));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::process_starttime;
    fn fixture(label: &str) -> (PathBuf, NetworkConfig) {
        let dir = crate::test_scratch(label);
        let bin = dir.join("fake-netd");
        std::fs::write(&bin, "#!/usr/bin/python3\nimport json,socket,sys,time\nc=json.load(open(sys.argv[1])); s=socket.socket(socket.AF_UNIX); s.bind(c['socket']); s.listen()\nwhile True: time.sleep(1)\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        (
            dir,
            NetworkConfig {
                worker_broker: None,
                lib_path: String::new(),
                private_access: Default::default(),
                bandwidth_bytes_per_sec: None,
                netd_bin: bin,
                resolver: Ipv4Addr::LOCALHOST,
            },
        )
    }
    fn until(mut f: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    #[test]
    fn refuses_legacy_live_vm_even_when_gateway_is_missing() {
        let (dir, cfg) = fixture("legacy-net");
        let vm = Worker {
            id: "legacy".into(),
            pid: std::process::id(),
            starttime: process_starttime(std::process::id()),
            isolation: None,
            sock_dir: dir.clone(),
            state_path: dir.join("state.json"),
        };
        std::fs::write(dir.join("state.json"), serde_json::to_vec(&vm).unwrap()).unwrap();
        std::fs::write(dir.join("net.json"), b"{}").unwrap();
        let networks = Networks::new(cfg).unwrap();
        assert!(
            matches!(networks.prepare(&dir, None, None), Err(Error::InvalidState(s)) if s.contains("legacy"))
        );
        assert!(!dir.join("net-state.json").exists());
        drop(networks);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn restart_adoption_and_reaping_preserve_process_identity() {
        let (root, cfg) = fixture("managed-net");
        let dir = root.join("sandbox");
        let networks = Networks::new(cfg.clone()).unwrap();
        networks.prepare(&dir, None, None).unwrap();
        let vm = Worker {
            id: "test-vm".into(),
            pid: std::process::id(),
            starttime: process_starttime(std::process::id()),
            isolation: None,
            sock_dir: dir.clone(),
            state_path: dir.join("vm-state.json"),
        };
        networks.attach(&dir, vm.clone());
        let old = Worker::load(dir.join("net-state.json")).unwrap();
        networks
            .0
            .lock()
            .unwrap()
            .entries
            .get_mut(&dir)
            .unwrap()
            .worker
            .as_mut()
            .unwrap()
            .terminate()
            .unwrap();
        until(|| {
            Worker::load(dir.join("net-state.json")).is_ok_and(|w| w.pid != old.pid && alive(&w))
        });
        let replacement = Worker::load(dir.join("net-state.json")).unwrap();
        drop(networks);
        let mut changed = cfg.clone();
        changed.bandwidth_bytes_per_sec = Some(1024 * 1024);
        let refused = Networks::new(changed).unwrap();
        assert!(
            matches!(refused.prepare(&dir, None, None), Err(Error::InvalidState(s)) if s.contains("bandwidth"))
        );
        assert!(alive(&replacement));
        drop(refused);
        let networks = Networks::new(cfg).unwrap();
        networks.prepare(&dir, None, None).unwrap();
        networks.attach(&dir, vm);
        assert_eq!(
            Worker::load(dir.join("net-state.json")).unwrap().pid,
            replacement.pid
        );
        networks.remove(&dir).unwrap();
        until(|| !Path::new(&format!("/proc/{}", replacement.pid)).exists());
        // A reused PID record must not be treated as our live network worker.
        let mut reused = replacement;
        reused.pid = std::process::id();
        reused.starttime = Some(0);
        assert!(!alive(&reused));
        drop(networks);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn restart_backoff_caps_and_requires_stable_recovery() {
    let mut backoff = RestartBackoff::default();
    for seconds in [1, 2, 4, 8, 16, 32, 60, 60] {
        assert_eq!(backoff.failed(), Duration::from_secs(seconds));
        backoff.healthy(Duration::from_secs(29));
    }
    backoff.healthy(Duration::from_secs(30));
    assert_eq!(backoff.failed(), Duration::from_secs(1));
}
