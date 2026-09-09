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
    pub netd_bin: PathBuf,
    pub resolver: Ipv4Addr,
}

#[derive(Debug)]
struct Entry {
    worker: Option<WorkerHandle>,
    vm: Option<Worker>,
    retry: Instant,
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

fn worker_alive(w: &mut WorkerHandle) -> bool {
    match w {
        WorkerHandle::Owned(w) => w.owns_child() && matches!(w.try_reap(), Ok(None)),
        WorkerHandle::Adopted(w) => alive(w),
    }
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
        if !cfg.netd_bin.is_file() {
            return Err(Error::InvalidState("network binary missing".into()));
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
                let cfg = core.cfg.clone();
                core.entries.retain(|dir, entry| {
                    if entry.vm.as_ref().is_some_and(|w| !alive(w)) {
                        if let Some(w) = &mut entry.worker {
                            let _ = w.terminate();
                        }
                        return false;
                    }
                    if entry.worker.as_mut().is_some_and(worker_alive) {
                        return true;
                    }
                    if Instant::now() >= entry.retry {
                        entry.retry = Instant::now() + Duration::from_secs(1);
                        match launch(&cfg, dir) {
                            Ok(w) => entry.worker = Some(w),
                            Err(e) => eprintln!("netd: restart {}: {e}", dir.display()),
                        }
                    }
                    true
                });
            })?;
        Ok(Self(shared))
    }

    pub(crate) fn prepare(&self, dir: &Path) -> Result<()> {
        let mut core = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // Replacing the association is serialized with the monitor: a previous
        // dead VM must not remove the new VM's network process during boot.
        if let Some(entry) = core.entries.get_mut(dir) {
            entry.vm = None;
            if entry.worker.as_mut().is_some_and(worker_alive) {
                return Ok(());
            }
        }
        let worker = match Worker::load(dir.join("net-state.json")) {
            Ok(w) if alive(&w) => {
                let saved: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(dir.join("net.json"))?)?;
                if saved["resolver"].as_str() != Some(core.cfg.resolver.to_string().as_str()) {
                    return Err(Error::InvalidState(
                        "stop VMs before changing their DNS resolver".into(),
                    ));
                }
                WorkerHandle::Adopted(w)
            }
            Ok(w) if is_alive(w.pid) && w.starttime.is_none() => {
                return Err(Error::InvalidState(
                    "cannot adopt netd without process identity".into(),
                ));
            }
            Ok(_) => launch(&core.cfg, dir)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => launch(&core.cfg, dir)?,
            Err(e) => return Err(e.into()),
        };
        core.entries.insert(
            dir.to_owned(),
            Entry {
                worker: Some(worker),
                vm: None,
                retry: Instant::now(),
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
            if alive(&w) {
                WorkerHandle::Adopted(w).terminate()?;
            }
        }
        let _ = std::fs::remove_file(dir.join("net-state.json"));
        let _ = std::fs::remove_file(dir.join("sock/net.sock"));
        Ok(())
    }
}

fn launch(cfg: &NetworkConfig, dir: &Path) -> Result<WorkerHandle> {
    let sock = dir.join("sock");
    std::fs::create_dir_all(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o700))?;
    let socket = sock.join("net.sock");
    match std::fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let spec = dir.join("net.json");
    std::fs::write(
        &spec,
        serde_json::to_vec(&serde_json::json!({
            "socket": socket, "resolver": cfg.resolver,
        }))?,
    )?;
    let mut worker = spawn_worker_cfg(&SpawnConfig {
        vmm_binary: cfg.netd_bin.as_os_str(),
        spec_arg: &spec,
        state_path: &dir.join("net-state.json"),
        hermetic: true,
        env: &["PATH=/usr/bin:/bin".into()],
        stderr_log: Some(&dir.join("netd.log")),
    })?;
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
    fn fixture() -> (PathBuf, NetworkConfig) {
        let dir = crate::test_scratch("managed-net");
        let bin = dir.join("fake-netd");
        std::fs::write(&bin, "#!/usr/bin/python3\nimport json,socket,sys,time\nc=json.load(open(sys.argv[1])); s=socket.socket(socket.AF_UNIX); s.bind(c['socket']); s.listen()\nwhile True: time.sleep(1)\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        (
            dir,
            NetworkConfig {
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
    fn restart_adoption_and_reaping_preserve_process_identity() {
        let (root, cfg) = fixture();
        let dir = root.join("sandbox");
        let networks = Networks::new(cfg.clone()).unwrap();
        networks.prepare(&dir).unwrap();
        let vm = Worker {
            id: "test-vm".into(),
            pid: std::process::id(),
            starttime: process_starttime(std::process::id()),
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
        let networks = Networks::new(cfg).unwrap();
        networks.prepare(&dir).unwrap();
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
