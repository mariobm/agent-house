//! Narrow Linux launch/stop broker. No arbitrary commands, IDs or environment.
#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod kernel;
#[cfg(target_os = "linux")]
mod launcher;

#[cfg(target_os = "linux")]
mod broker {
    use super::{
        config::{self, Config},
        kernel, launcher,
    };
    use ahvm_engine::{
        BrokerAction, BrokerReply, BrokerRequest, Worker, WorkerIdentity, WorkerRole,
    };
    use serde::{Deserialize, Serialize};
    use std::{
        collections::BTreeMap,
        fs,
        io::{self, BufRead, BufReader, Read, Write},
        os::{
            fd::AsRawFd,
            linux::net::SocketAddrExt,
            unix::{
                fs::{MetadataExt, PermissionsExt},
                net::{SocketAddr, UnixDatagram, UnixListener, UnixStream},
                process::CommandExt,
            },
        },
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        intent: bool,
        worker: Worker,
        root_disk: Option<PathBuf>,
    }
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Registry {
        config: Config,
        next_identity: u32,
        entries: BTreeMap<String, Entry>,
    }
    struct Broker {
        cfg: Config,
        registry: Registry,
        children: BTreeMap<String, Child>,
        _lock: fs::File,
    }
    fn key(id: &str, role: WorkerRole) -> String {
        format!("{id}/{}", role.name())
    }
    fn group(cfg: &Config, id: &str, role: WorkerRole) -> PathBuf {
        cfg.cgroup_root.join(format!("vm-{id}")).join(role.name())
    }
    fn persist(cfg: &Config, registry: &Registry) -> io::Result<()> {
        let temporary = cfg.state_dir.join("registry.new");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer(&mut file, registry).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(temporary, cfg.state_dir.join("registry.json"))?;
        fs::File::open(&cfg.state_dir)?.sync_all()
    }
    fn members(path: &Path) -> io::Result<Vec<u32>> {
        if path.exists() {
            for entry in fs::read_dir(path)? {
                if entry?.file_type()?.is_dir() {
                    return Err(io::Error::other(
                        "unexpected nested role cgroup; explicit administrator drain required",
                    ));
                }
            }
        }
        match fs::read_to_string(path.join("cgroup.procs")) {
            Ok(text) => text
                .split_whitespace()
                .map(|s| s.parse().map_err(io::Error::other))
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }
    fn empty(path: &Path) -> io::Result<bool> {
        match fs::read_to_string(path.join("cgroup.events")) {
            Ok(text) => Ok(text.lines().any(|line| line == "populated 0")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error),
        }
    }
    fn process_ids(pid: u32) -> io::Result<(Vec<u32>, Vec<u32>)> {
        let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
        let ids = |name: &str| -> io::Result<Vec<u32>> {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .ok_or_else(|| io::Error::other("missing process credentials"))?
                .split_whitespace()
                .map(|s| s.parse().map_err(io::Error::other))
                .collect()
        };
        Ok((ids("Uid:")?, ids("Gid:")?))
    }
    fn matches(worker: &Worker) -> io::Result<bool> {
        let identity = worker
            .isolation
            .as_ref()
            .ok_or_else(|| io::Error::other("missing root worker identity"))?;
        if !ahvm_engine::is_alive(worker.pid)
            || worker.starttime != ahvm_engine::process_starttime(worker.pid)
        {
            return Ok(false);
        }
        let (uids, gids) = process_ids(worker.pid)?;
        Ok(uids.len() == 4
            && gids.len() == 4
            && uids.iter().all(|u| *u == identity.uid)
            && gids.iter().all(|g| *g == identity.gid))
    }
    fn signal_pid(pid: u32, expected_uid: u32, expected_start: Option<u64>) -> io::Result<()> {
        use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};
        let pid_value = Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("invalid pid"))?;
        let fd = match pidfd_open(pid_value, PidfdFlags::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if expected_start.is_some_and(|s| ahvm_engine::process_starttime(pid) != Some(s)) {
            return Ok(());
        }
        let (uids, _) = match process_ids(pid) {
            Ok(ids) => ids,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if uids.len() != 4 || uids.iter().any(|uid| *uid != expected_uid) {
            return Err(io::Error::other(
                "refusing to signal a process outside assigned worker identity",
            ));
        }
        match pidfd_send_signal(&fd, Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    impl Broker {
        fn compact(&mut self) -> io::Result<usize> {
            let mut retired = Vec::new();
            for (key, entry) in &self.registry.entries {
                let role = entry.worker.isolation.as_ref().unwrap().role;
                // Stopped existing VMs retain daemon state records referencing
                // this identity. Only deleted VM names are safe to forget.
                let deleted = match fs::symlink_metadata(self.cfg.data_dir.join(&entry.worker.id)) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    Err(error) => return Err(error),
                    Ok(_) => false,
                };
                let path = group(&self.cfg, &entry.worker.id, role);
                if deleted
                    && !entry.intent
                    && !matches(&entry.worker)?
                    && members(&path)?.is_empty()
                    && empty(&path)?
                {
                    retired.push((key.clone(), entry.worker.id.clone(), role));
                }
            }
            for (key, id, role) in &retired {
                self.drain(id, *role)?;
                self.registry.entries.remove(key);
            }
            persist(&self.cfg, &self.registry)?;
            Ok(retired.len())
        }
        fn open(cfg: Config) -> io::Result<Self> {
            ahvm_engine::WorkerSandbox::check_support()?;
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(cfg.state_dir.join("broker.lock"))?;
            lock.try_lock()
                .map_err(|_| io::Error::other("worker broker already controls this registry"))?;
            let path = cfg.state_dir.join("registry.json");
            let marker = cfg.state_dir.join("initialized");
            config::root_owned(&std::env::current_exe()?)?;
            let mut registry = match fs::read(&path) {
                Ok(bytes) => {
                    config::root_owned(&path)?;
                    serde_json::from_slice::<Registry>(&bytes).map_err(io::Error::other)?
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && !marker.exists() => {
                    // A first installation is empty. Never silently recreate a
                    // missing ledger after its initialized marker was written.
                    for entry in fs::read_dir(&cfg.cgroup_root)? {
                        let entry = entry?;
                        if entry.file_name().to_string_lossy().starts_with("vm-")
                            && entry.file_type()?.is_dir()
                        {
                            if !empty(&entry.path())? {
                                return Err(io::Error::other("cannot initialize registry while a legacy/unknown VM cgroup is populated"));
                            }
                            for role in [WorkerRole::Vmm, WorkerRole::Netd] {
                                if !members(&entry.path().join(role.name()))?.is_empty()
                                    || !empty(&entry.path().join(role.name()))?
                                {
                                    return Err(io::Error::other("cannot initialize identity ledger while unknown workers survive"));
                                }
                            }
                        }
                    }
                    let r = Registry {
                        config: cfg.clone(),
                        next_identity: 0,
                        entries: BTreeMap::new(),
                    };
                    persist(&cfg, &r)?;
                    fs::File::create(&marker)?.sync_all()?;
                    fs::File::open(&cfg.state_dir)?.sync_all()?;
                    r
                }
                Err(error) => {
                    let message = format!("root identity registry unavailable; restore it, never reset allocations: {error}");
                    return Err(io::Error::other(message));
                }
            };
            let old = &registry.config;
            if cfg.uid_base != old.uid_base
                || cfg.gid_base != old.gid_base
                || cfg.state_dir != old.state_dir
                || cfg.daemon_uid != old.daemon_uid
                || cfg.daemon_gid != old.daemon_gid
                || cfg.identity_count < old.identity_count
                || registry.next_identity > cfg.identity_count
            {
                return Err(io::Error::other("identity range/ledger/daemon changes require a separate deployment; reserved capacity may only increase"));
            }
            let mut old_config = old.clone();
            old_config.identity_count = cfg.identity_count;
            if old_config != cfg {
                for entry in registry.entries.values() {
                    let role = entry.worker.isolation.as_ref().unwrap().role;
                    if ahvm_engine::is_alive(entry.worker.pid)
                        && entry.worker.starttime
                            == ahvm_engine::process_starttime(entry.worker.pid)
                        || !empty(&group(old, &entry.worker.id, role))?
                    {
                        return Err(io::Error::other("stop all workers before changing broker mount/device/library configuration"));
                    }
                }
            }
            registry.config = cfg.clone();
            persist(&cfg, &registry)?;
            // Interrupted intent cannot be assumed dead. Refuse unknown child
            // state rather than launching duplicates or killing unrelated PIDs.
            for entry in registry.entries.values_mut() {
                let role = entry.worker.isolation.as_ref().unwrap().role;
                if entry.intent
                    && (ahvm_engine::is_alive(entry.worker.pid)
                        && entry.worker.starttime
                            == ahvm_engine::process_starttime(entry.worker.pid)
                        || !members(&group(&cfg, &entry.worker.id, role))?.is_empty()
                        || !empty(&group(&cfg, &entry.worker.id, role))?)
                {
                    return Err(io::Error::other("interrupted launch still has a process; administrator must inspect/drain recorded role before broker restart"));
                }
                entry.intent = false;
            }
            persist(&cfg, &registry)?;
            // Probe user/mount namespaces and the actual data filesystem's
            // idmap support before advertising an upgraded healthy runtime.
            let mut probe = Command::new(std::env::current_exe()?)
                .arg("--preflight")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?;
            serde_json::to_writer(probe.stdin.take().unwrap(), &cfg).map_err(io::Error::other)?;
            let output = probe.wait_with_output()?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "worker namespace/idmap preflight: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(Self {
                cfg,
                registry,
                children: BTreeMap::new(),
                _lock: lock,
            })
        }
        fn reply(&self) -> BrokerReply {
            BrokerReply {
                data_dir: self.cfg.data_dir.clone(),
                vmm_bin: self.cfg.vmm_bin.clone(),
                netd_bin: self.cfg.netd_bin.clone(),
                cgroup_root: self.cfg.cgroup_root.clone(),
                lib_path: self.cfg.lib_path.clone(),
                ..Default::default()
            }
        }
        fn launch(&mut self, id: &str, role: WorkerRole) -> io::Result<Worker> {
            config::id(id)?;
            let key = key(id, role);
            if !self.registry.entries.contains_key(&key) && self.registry.entries.len() >= 16384 {
                return Err(io::Error::other("root worker record limit reached; stop broker and compact deleted/drained VM records without resetting allocations"));
            }
            let path = group(&self.cfg, id, role);
            if let Some(entry) = self.registry.entries.get(&key) {
                if entry.intent {
                    if ahvm_engine::is_alive(entry.worker.pid)
                        && entry.worker.starttime
                            == ahvm_engine::process_starttime(entry.worker.pid)
                        || !empty(&path)?
                    {
                        return Err(io::Error::other(
                            "unfinished live launch intent; explicit broker recovery required",
                        ));
                    }
                    self.registry.entries.get_mut(&key).unwrap().intent = false;
                    persist(&self.cfg, &self.registry)?;
                }
            }
            if let Some(entry) = self.registry.entries.get(&key) {
                if matches(&entry.worker)? {
                    return Err(io::Error::other("worker already running"));
                }
                self.drain(id, role)?;
            }
            if !members(&path)?.is_empty() || !empty(&path)? {
                return Err(io::Error::other(
                    "role cgroup is not empty; refusing identity reuse",
                ));
            }
            if self.registry.next_identity >= self.cfg.identity_count {
                return Err(io::Error::other("reserved worker identities exhausted; extend reserved range without resetting registry"));
            }
            let slot = self.registry.next_identity;
            let uid = self.cfg.uid_base + slot;
            let gid = self.cfg.gid_base + slot;
            let generation = u64::from(slot) + 1;
            let plan = launcher::plan(&self.cfg, id, role, uid, gid, generation)?;
            // Role subgroups isolate restart/kill, parent limits still aggregate.
            let parent = path.parent().unwrap();
            if !parent.is_dir() {
                return Err(io::Error::other(
                    "daemon must admit VM resources before launch",
                ));
            }
            fs::write(parent.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
            fs::create_dir_all(&path)?;
            let mut worker = Worker {
                id: id.into(),
                pid: 0,
                sock_dir: self.cfg.data_dir.join(id),
                state_path: self.cfg.data_dir.join(id).join(if role == WorkerRole::Vmm {
                    "state.json"
                } else {
                    "net-state.json"
                }),
                starttime: None,
                isolation: Some(WorkerIdentity {
                    socket: self.cfg.socket.clone(),
                    role,
                    uid,
                    gid,
                    generation,
                }),
            };
            self.registry.next_identity += 1;
            self.registry.entries.insert(
                key.clone(),
                Entry {
                    intent: true,
                    worker: worker.clone(),
                    root_disk: plan
                        .spec
                        .get("root_disk")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from),
                },
            );
            persist(&self.cfg, &self.registry)?; // allocation consumed BEFORE spawn
            let log_path = self.cfg.data_dir.join(id).join(if role == WorkerRole::Vmm {
                "vmm.log"
            } else {
                "netd.log"
            });
            let log_fd = kernel::open(&log_path, libc::O_WRONLY | libc::O_CREAT, 0o600)?;
            let log = fs::File::from(log_fd);
            let meta = log.metadata()?;
            if !meta.is_file()
                || meta.nlink() != 1
                || (meta.uid() != 0 && meta.uid() != self.cfg.daemon_uid)
            {
                return Err(io::Error::other("worker log is not a private regular file"));
            }
            log.set_len(0)?;
            kernel::chown_fd(log.as_raw_fd(), self.cfg.daemon_uid, self.cfg.daemon_gid)?;
            let (ack_read, ack_write) = kernel::pipe()?;
            let ack_raw = ack_write.as_raw_fd();
            let mut command = Command::new(std::env::current_exe()?);
            command
                .arg("--launch")
                .arg(ack_raw.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(log)
                .env_clear();
            // Only fcntl in pre_exec (async-signal-safe). All namespace, mount,
            // parsing and credential work occurs after exec in one thread.
            #[allow(unsafe_code)]
            unsafe {
                command.pre_exec(move || kernel::inherit(ack_raw));
            }
            let mut child = command.spawn()?;
            drop(ack_write);
            worker.pid = child.id();
            worker.starttime = ahvm_engine::process_starttime(worker.pid);
            self.registry.entries.get_mut(&key).unwrap().worker = worker.clone();
            // Child blocks on plan stdin until its exact PID is durable.
            let result = (|| {
                persist(&self.cfg, &self.registry)?;
                let mut input = child
                    .stdin
                    .take()
                    .ok_or_else(|| io::Error::other("launcher stdin missing"))?;
                serde_json::to_writer(&mut input, &plan).map_err(io::Error::other)?;
                drop(input);
                if !kernel::poll(ack_read.as_raw_fd(), 10000)? {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "worker launcher exceeded10s",
                    ));
                }
                let mut message = String::new();
                fs::File::from(ack_read)
                    .take(4097)
                    .read_to_string(&mut message)?;
                if !message.is_empty() {
                    return Err(io::Error::other(format!("worker launcher: {message}")));
                }
                if !matches(&worker)? {
                    return Err(io::Error::other(
                        "worker exec did not establish assigned host credentials",
                    ));
                }
                self.registry.entries.get_mut(&key).unwrap().intent = false;
                persist(&self.cfg, &self.registry)
            })();
            if let Err(error) = result {
                let _ = child.kill();
                let _ = child.wait();
                self.registry.entries.get_mut(&key).unwrap().intent = false;
                let _ = self.drain(id, role);
                let _ = persist(&self.cfg, &self.registry);
                return Err(error);
            }
            self.children.insert(key, child);
            Ok(worker)
        }
        fn drain(&mut self, id: &str, role: WorkerRole) -> io::Result<()> {
            let key = key(id, role);
            let worker = &self
                .registry
                .entries
                .get(&key)
                .ok_or_else(|| io::Error::other("root broker record missing"))?
                .worker;
            let uid = worker.isolation.as_ref().unwrap().uid;
            let path = group(&self.cfg, id, role);
            // A live recorded worker outside its assigned role group is not a
            // safe empty-group proof, even if an administrator moved it.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let pids = members(&path)?;
                if pids.is_empty() && empty(&path)? {
                    if matches(worker)? {
                        return Err(io::Error::other(
                            "worker survived outside its assigned role cgroup",
                        ));
                    }
                    break;
                }
                // Tasks exiting/reaping can leave populated1 briefly after
                // cgroup.procs is empty. Nested role groups were rejected
                // above; retain the bound and wait for the kernel's proof.
                for pid in pids {
                    signal_pid(
                        pid,
                        uid,
                        if pid == worker.pid {
                            worker.starttime
                        } else {
                            ahvm_engine::process_starttime(pid)
                        },
                    )?;
                }
                if Instant::now() > deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "worker role cgroup did not drain",
                    ));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if let Some(mut child) = self.children.remove(&key) {
                let _ = child.wait();
            }
            let identity = worker.isolation.as_ref().unwrap();
            let jail =
                self.cfg
                    .jail_dir
                    .join(format!("{}-{}-{}", id, role.name(), identity.generation));
            match fs::symlink_metadata(&jail) {
                Ok(meta) if meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o077 == 0 => {
                    fs::remove_dir(jail)?
                }
                Ok(_) => {
                    return Err(io::Error::other(
                        "refusing cleanup of unexpected jail backing inode",
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            Ok(())
        }
        fn request(&mut self, request: BrokerRequest) -> io::Result<BrokerReply> {
            let mut reply = self.reply();
            if matches!(request.action, BrokerAction::Check) {
                if !request.id.is_empty() || request.worker.is_some() {
                    return Err(io::Error::other("invalid broker check request"));
                }
                return Ok(reply);
            }
            config::id(&request.id)?;
            if matches!(request.action, BrokerAction::Launch) {
                if request.worker.is_some() {
                    return Err(io::Error::other("launch cannot select process identity"));
                }
                reply.worker = Some(self.launch(&request.id, request.role)?);
                reply.alive = true;
                return Ok(reply);
            }
            let key = key(&request.id, request.role);
            let entry = self
                .registry
                .entries
                .get(&key)
                .ok_or_else(|| io::Error::other("root broker worker record missing"))?;
            if entry.intent || request.worker.as_ref() != Some(&entry.worker) {
                return Err(io::Error::other(
                    "daemon worker record does not match root-owned identity",
                ));
            }
            if matches!(request.action, BrokerAction::Inspect) {
                reply.root_disk = entry.root_disk.clone();
            }
            if matches!(request.action, BrokerAction::Stop) || !matches(&entry.worker)? {
                self.drain(&request.id, request.role)?;
            } else {
                let pids = members(&group(&self.cfg, &request.id, request.role))?;
                if !pids.contains(&entry.worker.pid) {
                    return Err(io::Error::other("worker is outside recorded role cgroup"));
                }
                reply.alive = true;
            }
            Ok(reply)
        }
        fn maintain(&mut self) {
            let finished: Vec<_> = self
                .children
                .iter_mut()
                .filter_map(|(key, child)| child.try_wait().ok().flatten().map(|_| key.clone()))
                .collect();
            for key in finished {
                if let Some(entry) = self.registry.entries.get(&key) {
                    let id = entry.worker.id.clone();
                    let role = entry.worker.isolation.as_ref().unwrap().role;
                    if let Err(error) = self.drain(&id, role) {
                        eprintln!("worker-broker: could not drain exited {key}: {error}");
                    }
                }
            }
        }
    }

    fn notify_ready() -> io::Result<()> {
        if let Some(address) = std::env::var_os("NOTIFY_SOCKET") {
            let text = address.to_string_lossy();
            let socket = UnixDatagram::unbound()?;
            if let Some(name) = text.strip_prefix('@') {
                socket.connect_addr(&SocketAddr::from_abstract_name(name)?)?;
            } else {
                socket.connect(Path::new(&address))?;
            }
            socket.send(b"READY=1")?;
        }
        Ok(())
    }

    pub fn serve(path: &Path) -> io::Result<()> {
        let cfg = Config::load(path)?;
        let mut broker = Broker::open(cfg)?;
        match fs::symlink_metadata(&broker.cfg.socket) {
            Ok(meta) => {
                use std::os::unix::fs::FileTypeExt;
                if !meta.file_type().is_socket() || UnixStream::connect(&broker.cfg.socket).is_ok()
                {
                    return Err(io::Error::other(
                        "worker broker socket already active or not a socket",
                    ));
                }
                fs::remove_file(&broker.cfg.socket)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&broker.cfg.socket)?;
        fs::set_permissions(&broker.cfg.socket, fs::Permissions::from_mode(0o600))?;
        std::os::unix::fs::chown(
            &broker.cfg.socket,
            Some(broker.cfg.daemon_uid),
            Some(broker.cfg.daemon_gid),
        )?;
        listener.set_nonblocking(true)?;
        notify_ready()?;
        loop {
            broker.maintain();
            if !kernel::poll(listener.as_raw_fd(), 100)? {
                continue;
            }
            let (mut socket, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            };
            let uid = rustix::net::sockopt::socket_peercred(&socket)?.uid.as_raw();
            if uid != broker.cfg.daemon_uid && uid != 0 {
                continue;
            }
            socket.set_read_timeout(Some(Duration::from_millis(500)))?;
            socket.set_write_timeout(Some(Duration::from_millis(500)))?;
            let result = (|| {
                let mut line = String::new();
                BufReader::new((&socket).take(16385)).read_line(&mut line)?;
                if line.len() > 16384 || !line.ends_with('\n') {
                    return Err(io::Error::other("worker request exceeded framing bound"));
                }
                let request: BrokerRequest =
                    serde_json::from_str(&line).map_err(io::Error::other)?;
                if uid == 0
                    && !matches!(request.action, BrokerAction::Inspect | BrokerAction::Check)
                    || uid != 0 && matches!(request.action, BrokerAction::Inspect)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "broker role action is not authorized for this peer",
                    ));
                }
                broker.request(request)
            })();
            let reply = match result {
                Ok(reply) => reply,
                Err(error) => BrokerReply {
                    error: Some(error.to_string()),
                    ..broker.reply()
                },
            };
            let _ = serde_json::to_writer(&mut socket, &reply);
            let _ = socket.write_all(b"\n");
        }
    }
    pub fn compact(path: &Path) -> io::Result<()> {
        let mut broker = Broker::open(Config::load(path)?)?;
        let count = broker.compact()?;
        println!(
            "pruned_records={count}; next_identity={} (unchanged)",
            broker.registry.next_identity
        );
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn main() {
    use std::{
        fs,
        io::{Read, Write},
        os::fd::FromRawFd,
        path::Path,
    };
    if rustix::process::geteuid().as_raw() != 0 {
        eprintln!("worker-broker: root service required; this binary is never setuid");
        std::process::exit(1);
    }
    let args: Vec<_> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("--idmap") if args.len() == 2 => (|| {
            kernel::new_user_namespace()?;
            std::io::stdout().write_all(b"R")?;
            std::io::stdout().flush()?;
            let mut input = [0];
            let _ = std::io::stdin().read(&mut input);
            Ok(())
        })(),
        Some("--preflight") if args.len() == 2 => (|| {
            let mut input = Vec::new();
            std::io::stdin().take(65537).read_to_end(&mut input)?;
            let cfg: config::Config =
                serde_json::from_slice(&input).map_err(std::io::Error::other)?;
            let ns = launcher::mapping(cfg.uid_base, cfg.gid_base, cfg.daemon_uid, cfg.daemon_gid)?;
            kernel::private_mount_namespace().map_err(|e| {
                std::io::Error::other(format!("preflight private mount namespace: {e}"))
            })?;
            let root = cfg.jail_dir.join("preflight");
            fs::create_dir_all(&root)?;
            kernel::tmpfs(&root)?;
            let source = kernel::open(&cfg.data_dir, libc::O_PATH, 0)?;
            let target = root.join("data");
            fs::create_dir(&target)?;
            kernel::bind(&source, &target, Some(&ns), false, false).map_err(|e| {
                std::io::Error::other(format!(
                    "preflight idmapped bind of actual data filesystem: {e}"
                ))
            })?;
            Ok(())
        })(),
        Some("--launch") if args.len() == 3 => (|| {
            let ack: i32 = args[2].parse().map_err(std::io::Error::other)?;
            let mut input = Vec::new();
            std::io::stdin()
                .take(2 * 1024 * 1024 + 1)
                .read_to_end(&mut input)?;
            if input.len() > 2 * 1024 * 1024 {
                return Err(std::io::Error::other("internal worker plan too large"));
            }
            let plan: launcher::Plan =
                serde_json::from_slice(&input).map_err(std::io::Error::other)?;
            let result = launcher::execute(plan, ack);
            if let Err(error) = &result {
                // Only this raw inherited pipe survives setup errors. No RAII
                // owner exists in this exec; exit immediately after reporting.
                #[allow(unsafe_code)]
                let mut pipe = unsafe { fs::File::from_raw_fd(ack) };
                let _ = write!(pipe, "{error}");
            }
            result
        })(),
        Some("--compact") if args.len() == 3 => broker::compact(Path::new(&args[2])),
        Some(path) if args.len() == 2 => broker::serve(Path::new(path)),
        _ => Err(std::io::Error::other(
            "usage: ahvm-worker-broker ROOT_CONFIG.json",
        )),
    };
    if let Err(error) = result {
        eprintln!("worker-broker: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("worker-broker requires Linux");
    std::process::exit(1);
}
