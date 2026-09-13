//! Linux host volume supervisor. Private root-only protocol, independent workers.
//! Installation is opt-in; quotas and remote GC must precede cloud activation.
use crate::{
    cache::CachedStore,
    indexed::IndexedVolume,
    owned::OwnedDisk,
    s3::{Config as S3Config, S3Store},
    ObjectStore,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, BufRead, Read, Write},
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
mod host;
#[cfg(test)]
mod tests;
mod worker;
use host::*;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    client_uid: u32,
    #[serde(default)]
    image_roots: Vec<PathBuf>,
    #[serde(default)]
    socket_dir: Option<PathBuf>,
    root: PathBuf,
    engine_root: PathBuf,
    credentials: PathBuf,
    nbd_client: PathBuf,
    devices: Vec<PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Record {
    id: String,
    sandbox: PathBuf,
    image: PathBuf,
    image_hash: String,
    device: PathBuf,
    prepared: bool,
    deleted: bool,
    desired: bool,
    worker: Option<Process>,
    client: Option<Process>,
    vm: Option<Process>,
}
#[derive(Debug)]
struct Entry {
    record: Record,
    failures: u32,
    retry_at: Instant,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    operation: String,
    volume_id: String,
    sandbox_dir: PathBuf,
    #[serde(default)]
    image: Option<PathBuf>,
}
#[derive(Debug)]
struct Service {
    executable: PathBuf,
    config: Config,
    entries: Mutex<BTreeMap<String, Arc<Mutex<Entry>>>>,
    _locks: Vec<Lock>,
}
fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Service {
    fn open(config: Config) -> Result<Arc<Self>> {
        private_dir(&config.root)?;
        if config.root.join("record.json").exists() {
            return Err("legacy qualification root requires explicit migration".into());
        }
        File::open(config.root.parent().ok_or("missing state parent")?)?.sync_all()?;
        private_dir(&config.root.join("s"))?;
        if config.root.as_os_str().len() > 70 {
            return Err("service root too long for Unix sockets".into());
        }
        if config.client_uid != 0 && config.socket_dir.is_none() {
            return Err("non-root client requires separate socket_dir".into());
        }
        if let Some(path) = &config.socket_dir {
            use std::os::unix::fs::MetadataExt;
            if !path.is_absolute() || path.as_os_str().len() > 90 {
                return Err("invalid socket directory".into());
            }
            match fs::create_dir(path) {
                Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o711))?,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.into()),
            }
            let meta = fs::symlink_metadata(path)?;
            if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
                return Err("socket directory must be controlled by root".into());
            }
        }
        if config.client_uid != 0 && config.image_roots.is_empty() {
            return Err("non-root client requires image_roots".into());
        }
        for image_root in &config.image_roots {
            if image_root.canonicalize()? != *image_root || !image_root.is_dir() {
                return Err("invalid image root".into());
            }
            trusted_image_path(image_root)?;
        }
        let mut locks = vec![Lock::take(&config.root.join("service.lock"))?];
        if config.engine_root.canonicalize()? != config.engine_root
            || !config.nbd_client.is_absolute()
            || !config.credentials.is_absolute()
            || config.devices.is_empty()
            || config.devices.len() > 32
        {
            return Err("invalid service configuration".into());
        }
        let _ = S3Config::from_file(&config.credentials)?;
        let mut unique = std::collections::BTreeSet::new();
        for dev in &config.devices {
            let name = dev
                .to_str()
                .unwrap_or("")
                .strip_prefix("/dev/nbd")
                .unwrap_or("");
            if name.is_empty()
                || !name.bytes().all(|b| b.is_ascii_digit())
                || !fs::symlink_metadata(dev)?.file_type().is_block_device()
                || !unique.insert(dev.clone())
            {
                return Err("invalid or repeated NBD device".into());
            }
            locks.push(Lock::take(&Path::new("/run/lock").join(format!(
                "ahvm-volume-{}.lock",
                dev.file_name().unwrap().to_string_lossy()
            )))?);
        }
        let volumes = config.root.join("volumes");
        private_dir(&volumes)?;
        File::open(&config.root)?.sync_all()?;
        let mut entries = BTreeMap::new();
        let mut assigned = std::collections::BTreeSet::new();
        for item in fs::read_dir(&volumes)? {
            let item = item?;
            let id = item.file_name().to_string_lossy().to_string();
            if !valid_id(&id) || !item.file_type()?.is_dir() {
                return Err("invalid volume directory".into());
            }
            // Directory is fsynced before its record. Empty crash debris has no
            // external side effects and is safe to remove while holding the lock.
            if !item.path().join("record.json").exists() {
                let debris: Vec<_> =
                    fs::read_dir(item.path())?.collect::<std::result::Result<_, _>>()?;
                if debris.iter().all(|d| {
                    d.file_name() == "record.tmp" && d.file_type().is_ok_and(|t| t.is_file())
                }) {
                    for d in debris {
                        fs::remove_file(d.path())?;
                    }
                    fs::remove_dir(item.path())?;
                    continue;
                }
                return Err("unaccounted volume directory".into());
            }
            let r: Record = read(&item.path().join("record.json"))?;
            if r.id != id
                || !config.devices.contains(&r.device)
                || r.sandbox.parent() != Some(config.engine_root.as_path())
            {
                return Err("invalid persisted volume binding".into());
            }
            if (!r.deleted || r.worker.is_some() || r.client.is_some())
                && !assigned.insert(r.device.clone())
            {
                return Err("duplicate device assignment".into());
            }
            entries.insert(
                id,
                Arc::new(Mutex::new(Entry {
                    record: r,
                    failures: 0,
                    retry_at: Instant::now(),
                })),
            );
        }
        // Never take an already attached pool device absent from our records.
        for dev in &config.devices {
            if nbd_pid(dev)?.is_some() && !assigned.contains(dev) {
                return Err("unrecorded NBD attachment".into());
            }
        }
        Ok(Arc::new(Self {
            executable: std::env::current_exe()?,
            config,
            entries: Mutex::new(entries),
            _locks: locks,
        }))
    }
    fn dir(&self, r: &Record) -> PathBuf {
        self.config.root.join("volumes").join(&r.id)
    }
    fn socket(&self, r: &Record) -> PathBuf {
        self.config
            .root
            .join("s")
            .join(r.device.file_name().unwrap())
            .with_extension("sock")
    }
    fn persist(&self, r: &Record) -> Result<()> {
        save(&self.dir(r).join("record.json"), r)
    }
    fn binding(&self, id: &str, dir: &Path) -> Result<()> {
        if dir.parent() != Some(self.config.engine_root.as_path()) || dir.canonicalize()? != dir {
            return Err("sandbox outside configured engine root".into());
        }
        let record: serde_json::Value = read(&dir.join("sandbox.json"))?;
        if record["info"]["storage"]["volume_id"] != id
            || record["info"]["storage"]["mode"] != "replicated"
        {
            return Err("sandbox volume binding mismatch".into());
        }
        Ok(())
    }
    fn entry(&self, q: &Request) -> Result<Arc<Mutex<Entry>>> {
        if q.version != 1 || !valid_id(&q.volume_id) {
            return Err("invalid request".into());
        }
        // Deletion can be retried after engine record removal, but only from
        // the original sandbox path and only for an already tombstoned record.
        let mut map = self.entries.lock().map_err(|_| "registry poisoned")?;
        if let Some(e) = map.get(&q.volume_id) {
            return Ok(e.clone());
        }
        if q.operation != "prepare" || map.len() >= 1024 {
            return Err("unknown volume or record limit".into());
        }
        self.binding(&q.volume_id, &q.sandbox_dir)?;
        let image = q.image.as_ref().ok_or("missing image")?.canonicalize()?;
        if self.config.client_uid != 0 || !self.config.image_roots.is_empty() {
            if !self
                .config
                .image_roots
                .iter()
                .any(|root| image.starts_with(root))
            {
                return Err("image outside configured roots".into());
            }
            trusted_image_path(&image)?;
        }
        let mut used = std::collections::BTreeSet::new();
        for id in map.keys() {
            let r: Record = read(
                &self
                    .config
                    .root
                    .join("volumes")
                    .join(id)
                    .join("record.json"),
            )?;
            if !r.deleted || r.worker.is_some() || r.client.is_some() {
                used.insert(r.device);
            }
        }
        let device = self
            .config
            .devices
            .iter()
            .find(|d| !used.contains(*d))
            .ok_or("volume device slots exhausted")?
            .clone();
        if nbd_pid(&device)?.is_some() {
            return Err("device in use".into());
        }
        let r = Record {
            id: q.volume_id.clone(),
            sandbox: q.sandbox_dir.clone(),
            image,
            image_hash: String::new(),
            device,
            prepared: false,
            deleted: false,
            desired: false,
            worker: None,
            client: None,
            vm: None,
        };
        private_dir(&self.dir(&r))?;
        self.persist(&r)?;
        File::open(self.config.root.join("volumes"))?.sync_all()?;
        let e = Arc::new(Mutex::new(Entry {
            record: r,
            failures: 0,
            retry_at: Instant::now(),
        }));
        map.insert(q.volume_id.clone(), e.clone());
        Ok(e)
    }
    fn store(&self) -> Result<Arc<dyn ObjectStore>> {
        Ok(Arc::new(CachedStore::new(
            Arc::new(S3Store::with_timeout(
                S3Config::from_file(&self.config.credentials)?,
                Duration::from_secs(3),
            )?),
            64 * 1024 * 1024,
        )?))
    }
    fn prepare(&self, r: &mut Record) -> Result<()> {
        if r.prepared {
            return Ok(());
        }
        use sha2::{Digest, Sha256};
        let mut file = File::open(&r.image)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        if !metadata.is_file()
            || size == 0
            || size > 64 * 1024 * 1024 * 1024
            || !size.is_multiple_of(crate::CHUNK_BYTES as u64)
        {
            return Err("invalid raw image size/type".into());
        }
        let mut hasher = Sha256::new();
        let mut bytes = vec![0; 8 * 1024 * 1024];
        loop {
            let n = file.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            hasher.update(&bytes[..n]);
        }
        let hash = format!("{:x}", hasher.finalize());
        if r.image_hash.is_empty() {
            r.image_hash = hash;
            self.persist(r)?;
        } else if r.image_hash != hash {
            return Err("import source changed".into());
        }
        let store = self.store()?;
        let mut disk = match store.head(&r.id)? {
            None => Some(IndexedVolume::create_import(store.clone(), &r.id, size)?),
            Some(h) => {
                let v: serde_json::Value = serde_json::from_slice(&h.manifest)?;
                if v["format"] == 4 || v["ready"] == true {
                    None
                } else {
                    Some(IndexedVolume::resume_import(store.clone(), &r.id, size)?)
                }
            }
        };
        if let Some(disk) = &mut disk {
            use std::io::{Seek, SeekFrom};
            file.seek(SeekFrom::Start(0))?;
            let mut check = Sha256::new();
            let mut at = 0;
            while at < size {
                let n = bytes.len().min((size - at) as usize);
                file.read_exact(&mut bytes[..n])?;
                check.update(&bytes[..n]);
                disk.write(at, &bytes[..n])?;
                disk.commit()?;
                at += n as u64;
            }
            if format!("{:x}", check.finalize()) != r.image_hash {
                return Err("image changed during import".into());
            }
            disk.finish_import()?;
        }
        OwnedDisk::enroll(store, &r.id)?;
        r.prepared = true;
        self.persist(r)
    }
    fn control(&self, r: &Record, op: &str) -> Result<crate::local::Status> {
        let worker = r.worker.as_ref().ok_or("worker unavailable")?;
        if !worker.alive()? {
            return Err("worker unavailable".into());
        }
        let mut conn = UnixStream::connect(self.socket(r).with_extension("control"))?;
        let peer = rustix::net::sockopt::socket_peercred(&conn)?;
        if peer.pid.as_raw_pid() as u32 != worker.pid
            || peer.uid.as_raw() != 0
            || !worker.alive()?
        {
            return Err("worker peer mismatch".into());
        }
        conn.set_read_timeout(Some(Duration::from_secs(if op == "sync" {
            300
        } else {
            2
        })))?;
        conn.set_write_timeout(Some(Duration::from_secs(2)))?;
        writeln!(conn, "{op}")?;
        let mut data = Vec::new();
        conn.take(4097).read_to_end(&mut data)?;
        if data.len() > 4096 {
            return Err("oversized worker reply".into());
        }
        Ok(serde_json::from_slice(&data).map_err(|_| "invalid worker reply")?)
    }
    fn inspect(&self, r: &Record) -> Result<crate::local::Status> {
        if nbd_pid(&r.device)? != r.client.as_ref().map(|p| p.pid) || r.client.is_none() {
            return Err("attachment unavailable".into());
        }
        let s = self.control(r, "status")?;
        if s.local_failed {
            return Err("local disk failed".into());
        }
        Ok(s)
    }
    fn check_vm_uid(&self, p: &Process) -> Result<()> {
        let status = fs::read_to_string(format!("/proc/{}/status", p.pid))?;
        let uid = status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(2))
            .ok_or("missing VM uid")?
            .parse::<u32>()?;
        if uid != self.config.client_uid {
            return Err("VM belongs to another host user".into());
        }
        Ok(())
    }
    fn vm(&self, r: &Record) -> Result<Option<Process>> {
        if let Some(vm) = &r.vm {
            if vm.alive()? {
                self.check_vm_uid(vm)?;
                return Ok(Some(vm.clone()));
            }
        }
        let path = r.sandbox.join("state.json");
        if !path.exists() {
            return Ok(None);
        }
        self.binding(&r.id, &r.sandbox)?;
        let v: serde_json::Value = read(&path)?;
        let pid = u32::try_from(v["pid"].as_u64().ok_or("missing VM pid")?)?;
        let Some(p) = Process::read(pid)? else {
            return Ok(None);
        };
        if v["starttime"].as_u64() != Some(p.start) {
            return Ok(None);
        }
        let spec: serde_json::Value = read(&r.sandbox.join("spec.json"))?;
        if spec["root_disk"].as_str() != r.device.to_str() || spec["root_disk_format"] != "raw" {
            return Err("VM disk binding mismatch".into());
        }
        self.check_vm_uid(&p)?;
        if !consumers(&r.device)?.contains(&pid) {
            return Ok(None);
        }
        Ok(Some(p))
    }
    fn detach(&self, r: &mut Record) -> Result<()> {
        if self.vm(r)?.is_some() {
            return Err("VM still running".into());
        }
        unused(&r.device)?;
        if let Some(pid) = nbd_pid(&r.device)? {
            if r.client.as_ref().map(|p| p.pid) != Some(pid) {
                return Err("foreign attachment".into());
            }
            self.command(&["-d".as_ref(), r.device.as_os_str()], None)?;
            let end = Instant::now() + Duration::from_secs(10);
            while nbd_pid(&r.device)?.is_some() {
                if Instant::now() >= end {
                    return Err("disconnect incomplete".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        if let Some(p) = &r.worker {
            p.stop()?;
        }
        if r.deleted {
            std::os::unix::fs::chown(&r.device, Some(0), None)?;
        }
        r.worker = None;
        r.client = None;
        r.vm = None;
        self.persist(r)
    }
    fn command(&self, args: &[&std::ffi::OsStr], mut record: Option<&mut Record>) -> Result<()> {
        let mut child = ChildGuard::new(
            Command::new(&self.executable)
                .arg("_attach")
                .arg(&self.config.nbd_client)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let p = Process::read(child.id())?.ok_or("child exited")?;
        let result = (|| {
            if let Some(r) = record.as_mut() {
                r.client = Some(p.clone());
                self.persist(r)?;
            }
            child
                .stdin
                .take()
                .ok_or("missing launch pipe")?
                .write_all(b"G")?;
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(s) = child.try_wait()? {
                    return if s.success() {
                        Ok(())
                    } else {
                        Err("NBD command failed".into())
                    };
                }
                if Instant::now() >= end {
                    return Err("NBD command timeout".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
        })();
        if result.is_err() {
            let _ = p.stop();
        }
        let _ = child.wait();
        result
    }
    fn attach(&self, r: &mut Record) -> Result<crate::local::Status> {
        if let Ok(status) = self.inspect(r) {
            return Ok(status);
        }
        self.detach(r)?;
        // Only the configured trusted engine UID may open this raw device.
        fs::set_permissions(&r.device, fs::Permissions::from_mode(0o600))?;
        std::os::unix::fs::chown(&r.device, Some(self.config.client_uid), None)?;
        let dir = self.dir(r);
        private_dir(&dir.join("owner"))?;
        for name in ["disk.sock", "disk.control"] {
            let p = if name == "disk.sock" {
                self.socket(r)
            } else {
                self.socket(r).with_extension("control")
            };
            if p.exists() {
                fs::remove_file(p)?;
            }
        }
        let mut child = ChildGuard::new(
            Command::new(&self.executable)
                .arg("_worker")
                .arg(&self.config.credentials)
                .arg(&r.id)
                .arg(&dir)
                .arg(self.socket(r))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let p = Process::read(child.id())?.ok_or("worker exited")?;
        r.worker = Some(p.clone());
        if let Err(e) = self.persist(r) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        let start = child
            .stdin
            .take()
            .ok_or("missing launch pipe")?
            .write_all(b"G");
        drop(child);
        start?;
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            if self.control(r, "status").is_ok() {
                break;
            }
            if !p.alive()? || Instant::now() >= end {
                return Err("storage worker not ready".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
        let device = r.device.clone();
        let socket = self.socket(r);
        self.command(
            &[
                "-unix".as_ref(),
                socket.as_os_str(),
                device.as_os_str(),
                "-timeout".as_ref(),
                "120".as_ref(),
            ],
            Some(r),
        )?;
        if nbd_pid(&device)? != r.client.as_ref().map(|p| p.pid) {
            return Err("NBD client identity mismatch".into());
        }
        fs::write(
            Path::new("/sys/block")
                .join(device.file_name().unwrap())
                .join("queue/max_sectors_kb"),
            "1024",
        )?;
        self.inspect(r)
    }
    fn request(&self, q: Request) -> Result<serde_json::Value> {
        let entry = self.entry(&q)?;
        let mut e = entry.try_lock().map_err(|_| "volume busy")?;
        let r = &mut e.record;
        if q.sandbox_dir != r.sandbox {
            return Err("sandbox binding mismatch".into());
        }
        if !r.deleted {
            self.binding(&r.id, &r.sandbox)?;
        }
        let mut status = None;
        if q.operation == "delete" {
            r.deleted = true;
            r.desired = false;
            self.persist(r)?;
            self.detach(r)?;
        } else {
            if r.deleted {
                return Err("volume deleted".into());
            }
            match q.operation.as_str() {
                "prepare" => {
                    if q.image.as_ref().is_none_or(|p| p != &r.image) {
                        return Err("source mismatch".into());
                    }
                    self.prepare(r)?;
                }
                "attach" => {
                    if !r.prepared {
                        return Err("not prepared".into());
                    }
                    if self.vm(r)?.is_some() {
                        return Err("VM already bound".into());
                    }
                    r.desired = true;
                    self.persist(r)?;
                    status = Some(self.attach(r)?);
                }
                "bind" => {
                    self.inspect(r)?;
                    r.vm = Some(self.vm(r)?.ok_or("VM not ready for binding")?);
                    self.persist(r)?;
                }
                "inspect" | "status" => {
                    status = Some(self.inspect(r)?);
                }
                "sync" => {
                    if !r.prepared {
                        return Err("not prepared".into());
                    }
                    self.attach(r)?;
                    status = Some(self.control(r, "sync")?);
                    if !r.desired {
                        self.detach(r)?;
                    }
                }
                "detach" => {
                    if self.vm(r)?.is_some() {
                        return Err("VM still running".into());
                    }
                    r.desired = false;
                    self.persist(r)?;
                    self.detach(r)?;
                }
                _ => return Err("unknown operation".into()),
            }
        }
        e.failures = 0;
        e.retry_at = Instant::now();
        let r = &e.record;
        Ok(
            serde_json::json!({"ok":true,"volume_id":r.id,"device":if r.worker.is_some(){Some(&r.device)}else{None},"status":status}),
        )
    }
    fn recover(&self, e: &mut Entry) -> Result<()> {
        let r = &mut e.record;
        if r.deleted || !r.desired {
            if r.worker.is_some() || r.client.is_some() {
                self.detach(r)?;
            }
            return Ok(());
        }
        if self.inspect(r).is_ok() {
            return Ok(());
        }
        if r.worker
            .as_ref()
            .is_some_and(|p| p.alive().unwrap_or(false))
            && e.failures < 2
        {
            return Err("worker health check failed; retry before fencing VM".into());
        }
        // Never resume a VM against a replacement connection: terminate its
        // verified identity and prove all consumers gone before reconnecting.
        if let Some(vm) = self.vm(r)? {
            vm.stop()?;
        }
        self.detach(r)?;
        self.attach(r)?;
        Ok(())
    }
}

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "_worker") {
        return worker::run(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "_attach") {
        let mut go = [0];
        io::stdin().read_exact(&mut go)?;
        if go != *b"G" {
            return Err("launch cancelled".into());
        }
        use std::os::unix::process::CommandExt;
        let program = args.get(1).ok_or("missing command")?;
        return Err(Command::new(program).args(&args[2..]).exec().into());
    }
    if args.len() != 1 || !rustix::process::geteuid().is_root() {
        return Err("usage (root): ahvm-volumed CONFIG.json".into());
    }
    let config: Config = read(Path::new(&args[0]))?;
    let service = Service::open(config)?;
    let socket = service
        .config
        .socket_dir
        .as_ref()
        .unwrap_or(&service.config.root)
        .join("service.sock");
    if socket.exists() {
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    std::os::unix::fs::chown(&socket, Some(service.config.client_uid), None)?;
    let supervisor = service.clone();
    thread::spawn(move || loop {
        let entries: Vec<_> = supervisor
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        for entry in entries {
            let Ok(mut e) = entry.try_lock() else {
                continue;
            };
            if (!e.record.desired && e.record.worker.is_none() && e.record.client.is_none())
                || Instant::now() < e.retry_at
            {
                continue;
            }
            // One recovery task per volume; a slow R2 request never stalls the sweep.
            let service = supervisor.clone();
            let entry = entry.clone();
            e.retry_at = Instant::now() + Duration::from_secs(60);
            let snapshot = e.record.clone();
            drop(e);
            thread::spawn(move || {
                // Health probes do not hold the operation mutex. Revalidate the
                // snapshot before any destructive recovery or cleanup.
                let healthy = snapshot.desired && service.inspect(&snapshot).is_ok();
                let Ok(mut e) = entry.try_lock() else { return };
                if e.record != snapshot {
                    e.retry_at = Instant::now() + Duration::from_secs(2);
                    return;
                }
                let result = if healthy {
                    Ok(())
                } else {
                    service.recover(&mut e)
                };
                match result {
                    Ok(()) => {
                        e.failures = 0;
                        e.retry_at = Instant::now() + Duration::from_secs(2);
                    }
                    Err(error) => {
                        eprintln!("volume {} recovery: {error}", e.record.id);
                        e.failures = e.failures.saturating_add(1);
                        e.retry_at = Instant::now()
                            + Duration::from_secs((1u64 << e.failures.min(6)).min(60));
                    }
                }
            });
        }
        thread::sleep(Duration::from_secs(1));
    });
    let active = Arc::new(AtomicUsize::new(0));
    for connection in listener.incoming() {
        let mut conn = connection?;
        let uid = rustix::net::sockopt::socket_peercred(&conn)?.uid.as_raw();
        if uid != 0 && uid != service.config.client_uid {
            continue;
        }
        if active.fetch_add(1, Ordering::SeqCst) >= 16 {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let active = active.clone();
        let service = service.clone();
        thread::spawn(move || {
            struct Admission(Arc<AtomicUsize>);
            impl Drop for Admission {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            let _admission = Admission(active);
            let _ = conn.set_read_timeout(Some(Duration::from_secs(3)));
            let _ = conn.set_write_timeout(Some(Duration::from_secs(3)));
            let mut line = String::new();
            let reply = (|| -> Result<serde_json::Value> {
                io::BufReader::new((&mut conn).take(4097)).read_line(&mut line)?;
                if line.len() > 4096 || !line.ends_with('\n') {
                    return Err("invalid request".into());
                }
                let q: Request = serde_json::from_str(&line)?;
                let id = q.volume_id.clone();
                Ok(service.request(q).unwrap_or_else(|error| {
                    eprintln!("volume {id}: {error}");
                    serde_json::json!({"ok":false,"volume_id":id})
                }))
            })();
            let value = reply.unwrap_or_else(|_| serde_json::json!({"ok":false,"volume_id":""}));
            let _ = writeln!(conn, "{value}");
        });
    }
    Ok(())
}
