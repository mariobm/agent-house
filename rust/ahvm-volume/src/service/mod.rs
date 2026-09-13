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
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
mod accounting;
mod export;
mod host;
mod resources;
use accounting::{Limits, Usage, CACHE_BYTES};
#[cfg(test)]
mod tests;
mod warm;
mod worker;
use host::*;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    client_uid: u32,
    #[serde(default)]
    resources: Option<resources::Resources>,
    limits: Limits,
    #[serde(default)]
    image_roots: Vec<PathBuf>,
    #[serde(default = "local_base_default")]
    local_base_reads: bool,
    #[serde(default)]
    socket_dir: Option<PathBuf>,
    root: PathBuf,
    engine_root: PathBuf,
    credentials: PathBuf,
    nbd_client: PathBuf,
    devices: Vec<PathBuf>,
}
fn local_base_default() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Record {
    id: String,
    sandbox: PathBuf,
    image: PathBuf,
    image_hash: String,
    /// Reserved before import; never inferred from mutable source on restart.
    logical_bytes: u64,
    device: PathBuf,
    /// Cold records retain this historical path but do not own that device.
    #[serde(default)]
    evicted: bool,
    prepared: bool,
    deleted: bool,
    #[serde(default)]
    reclaimed: bool,
    #[serde(default)]
    gc_after: Option<String>,
    #[serde(default)]
    gc_eligible: bool,
    #[serde(default)]
    gc_last_completed: Option<u64>,
    desired: bool,
    worker: Option<Process>,
    client: Option<Process>,
    vm: Option<Process>,
}
#[derive(Debug)]
struct Entry {
    record: Record,
    failures: u32,
    mark: Option<crate::owned::OfflineMark>,
    retry_at: Instant,
}
#[derive(Debug)]
struct Slot {
    operation: Mutex<Entry>,
    cancellation: AtomicU64,
    collecting: AtomicBool,
}
impl Slot {
    fn new(entry: Entry) -> Self {
        Self {
            operation: Mutex::new(entry),
            cancellation: AtomicU64::new(0),
            collecting: AtomicBool::new(false),
        }
    }
}
struct CollectionGuard<'a>(&'a AtomicBool);
impl Drop for CollectionGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
impl Slot {
    fn collection(&self) -> CollectionGuard<'_> {
        self.collecting.store(true, Ordering::SeqCst);
        CollectionGuard(&self.collecting)
    }
    fn foreground(&self, mutating: bool) -> Result<std::sync::MutexGuard<'_, Entry>> {
        match self.operation.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err("volume poisoned".into()),
            Err(std::sync::TryLockError::WouldBlock) => (),
        }
        if !mutating {
            // Live guest input/output performs health inspections concurrently.
            // A brief collision with another inspection is not a disk failure.
            // Stay within the status RPC's three-second timeout, and do not
            // cancel or wait behind offline collection just for a read.
            let end = Instant::now() + Duration::from_secs(2);
            while !self.collecting.load(Ordering::SeqCst) && Instant::now() < end {
                match self.operation.try_lock() {
                    Ok(guard) => return Ok(guard),
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err("volume poisoned".into())
                    }
                    Err(std::sync::TryLockError::WouldBlock) => {
                        thread::sleep(Duration::from_millis(1))
                    }
                }
            }
            return Err("volume busy".into());
        }
        self.cancellation.fetch_add(1, Ordering::SeqCst);
        if !self.collecting.load(Ordering::SeqCst) {
            return Err("volume busy".into());
        }
        // A collector yields between bounded store requests. Briefly wait so a
        // normal start does not fail just because maintenance was in progress.
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            match self.operation.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(std::sync::TryLockError::Poisoned(_)) => return Err("volume poisoned".into()),
                Err(std::sync::TryLockError::WouldBlock) => {
                    thread::sleep(Duration::from_millis(10))
                }
            }
        }
        Err("volume busy after collection cancellation".into())
    }
}
impl std::ops::Deref for Slot {
    type Target = Mutex<Entry>;
    fn deref(&self) -> &Self::Target {
        &self.operation
    }
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
    #[serde(default)]
    logical_bytes: Option<u64>,
}
// Device, inode, size, mtime/ns, ctime/ns of a trusted image file.
type ImageIdentity = (u64, u64, u64, i64, i64, i64, i64);
#[derive(Debug)]
struct Service {
    executable: PathBuf,
    admission_failed: AtomicBool,
    reclamation: Mutex<()>,
    imports: Mutex<BTreeMap<ImageIdentity, String>>,
    config: Config,
    entries: Mutex<BTreeMap<String, Arc<Slot>>>,
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
        config.limits.validate()?;
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
        if let Some(resources) = &config.resources {
            resources.validate(config.devices.len())?;
        }
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
        let mut usage = Usage::default();
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
            if let Some(resources) = &config.resources {
                for process in [&r.worker, &r.client].into_iter().flatten() {
                    resources.verify(&r.id, process)?;
                }
            }
            if !r.reclaimed {
                usage.add_residency(r.logical_bytes, !r.evicted)?;
            }
            if (r.reclaimed
                && (!r.deleted
                    || r.desired
                    || r.worker.is_some()
                    || r.client.is_some()
                    || r.vm.is_some()))
                || (r.evicted
                    && ((!r.prepared && !r.deleted)
                        || r.desired
                        || r.worker.is_some()
                        || r.client.is_some()
                        || r.vm.is_some()))
                || r.gc_after.as_deref().is_some_and(|h| !valid_id(h))
                || r.id != id
                || !config.devices.contains(&r.device)
                || r.sandbox.parent() != Some(config.engine_root.as_path())
            {
                return Err("invalid persisted volume binding".into());
            }
            if ((!r.deleted && !r.evicted) || r.worker.is_some() || r.client.is_some())
                && !assigned.insert(r.device.clone())
            {
                return Err("duplicate device assignment".into());
            }
            entries.insert(
                id,
                Arc::new(Slot::new(Entry {
                    record: r,
                    failures: 0,
                    mark: None,
                    retry_at: Instant::now(),
                })),
            );
        }
        // Lowering a budget must not interrupt existing disks. Validate persisted
        // sizes but enforce the new budget only on admission.
        // Never take an already attached pool device absent from our records.
        for dev in &config.devices {
            if nbd_pid(dev)?.is_some() && !assigned.contains(dev) {
                return Err("unrecorded NBD attachment".into());
            }
        }
        Ok(Arc::new(Self {
            executable: std::env::current_exe()?,
            admission_failed: AtomicBool::new(false),
            reclamation: Mutex::new(()),
            imports: Mutex::new(BTreeMap::new()),
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
    fn entry(&self, q: &Request) -> Result<Arc<Slot>> {
        if q.version != 1 || !valid_id(&q.volume_id) {
            return Err("invalid request".into());
        }
        // Deletion can be retried after engine record removal, but only from
        // the original sandbox path and only for an already tombstoned record.
        let mut map = self.entries.lock().map_err(|_| "registry poisoned")?;
        if matches!(q.operation.as_str(), "prepare" | "retire")
            && self.admission_failed.load(Ordering::SeqCst)
        {
            return Err("reservation persistence failed; restart required before importing".into());
        }
        if let Some(e) = map.get(&q.volume_id) {
            return Ok(e.clone());
        }
        if !matches!(q.operation.as_str(), "prepare" | "retire") || map.len() >= 1024 {
            return Err("unknown volume or record limit".into());
        }
        if q.operation == "retire" {
            // A ledger may precede service registration. Persist retirement
            // intent without importing an image or borrowing an active device.
            let bytes = q.logical_bytes.ok_or("missing retirement sizing")?;
            let mut checked = Usage::default();
            checked.add_residency(bytes, false)?;
            if q.sandbox_dir.parent() != Some(self.config.engine_root.as_path()) {
                return Err("sandbox outside configured engine root".into());
            }
            let r = Record {
                id: q.volume_id.clone(),
                sandbox: q.sandbox_dir.clone(),
                image: PathBuf::new(),
                image_hash: String::new(),
                logical_bytes: bytes,
                device: self
                    .config
                    .devices
                    .first()
                    .ok_or("empty device pool")?
                    .clone(),
                evicted: true,
                prepared: false,
                deleted: true,
                reclaimed: false,
                gc_after: None,
                gc_eligible: false,
                gc_last_completed: None,
                desired: false,
                worker: None,
                client: None,
                vm: None,
            };
            private_dir(&self.dir(&r))?;
            let entry = Arc::new(Slot::new(Entry {
                record: r.clone(),
                failures: 0,
                mark: None,
                retry_at: Instant::now(),
            }));
            map.insert(r.id.clone(), entry.clone());
            if let Err(error) = (|| -> Result<()> {
                self.persist(&r)?;
                File::open(self.config.root.join("volumes"))?.sync_all()?;
                Ok(())
            })() {
                self.admission_failed.store(true, Ordering::SeqCst);
                return Err(error);
            }
            return Ok(entry);
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
        let metadata = fs::metadata(&image)?;
        let logical_bytes = metadata.len();
        if q.logical_bytes
            .is_some_and(|expected| expected != logical_bytes)
        {
            return Err("admitted image sizing mismatch".into());
        }
        if !metadata.is_file() {
            return Err("image must be a regular file".into());
        }
        let mut usage = Usage::default();
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
            if !r.reclaimed {
                usage.add_residency(r.logical_bytes, !r.evicted)?;
            }
            if (!r.deleted && !r.evicted) || r.worker.is_some() || r.client.is_some() {
                used.insert(r.device);
            }
        }
        self.config.limits.admit(&usage, logical_bytes)?;
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
            logical_bytes,
            device,
            evicted: false,
            prepared: false,
            deleted: false,
            reclaimed: false,
            gc_after: None,
            gc_eligible: false,
            gc_last_completed: None,
            desired: false,
            worker: None,
            client: None,
            vm: None,
        };
        private_dir(&self.dir(&r))?;
        let e = Arc::new(Slot::new(Entry {
            record: r.clone(),
            failures: 0,
            mark: None,
            retry_at: Instant::now(),
        }));
        map.insert(q.volume_id.clone(), e.clone());
        // A failed fsync/rename can still leave a durable reservation. Freeze
        // imports until startup reconstructs the registry from disk.
        if let Err(error) = (|| -> Result<()> {
            self.persist(&r)?;
            File::open(self.config.root.join("volumes"))?.sync_all()?;
            Ok(())
        })() {
            self.admission_failed.store(true, Ordering::SeqCst);
            return Err(error);
        }
        Ok(e)
    }
    fn reserve_residency(&self, r: &mut Record) -> Result<()> {
        if !r.evicted {
            return Ok(());
        }
        let map = self.entries.lock().map_err(|_| "registry poisoned")?;
        if self.admission_failed.load(Ordering::SeqCst) {
            return Err("reservation persistence failed; restart required".into());
        }
        let mut usage = Usage::default();
        let mut used = std::collections::BTreeSet::new();
        for id in map.keys() {
            let other: Record = read(
                &self
                    .config
                    .root
                    .join("volumes")
                    .join(id)
                    .join("record.json"),
            )?;
            if !other.reclaimed && other.id != r.id {
                usage.add_residency(other.logical_bytes, !other.evicted)?;
            }
            if (!other.deleted && !other.evicted)
                || other.worker.is_some()
                || other.client.is_some()
            {
                used.insert(other.device);
            }
        }
        self.config.limits.admit(&usage, r.logical_bytes)?;
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
        let mut next = r.clone();
        next.device = device;
        next.evicted = false;
        // A previous detached record may be resumed by sync without attach.
        next.gc_after = None;
        if let Err(error) = self.persist(&next) {
            self.admission_failed.store(true, Ordering::SeqCst);
            return Err(error);
        }
        *r = next;
        Ok(())
    }
    fn evict(&self, r: &mut Record) -> Result<()> {
        if r.evicted {
            return Ok(());
        }
        if r.deleted
            || !r.prepared
            || !r.gc_eligible
            || r.desired
            || r.worker.is_some()
            || r.client.is_some()
            || self.vm(r)?.is_some()
            || nbd_pid(&r.device)?.is_some()
        {
            return Err("volume is not eligible for local eviction".into());
        }
        unused(&r.device)?;
        let raw: Arc<dyn ObjectStore> = Arc::new(S3Store::with_timeout(
            S3Config::from_file(&self.config.credentials)?,
            Duration::from_secs(3),
        )?);
        self.evict_with_store(r, raw)
    }
    fn evict_with_store(&self, r: &mut Record, raw: Arc<dyn ObjectStore>) -> Result<()> {
        if r.deleted
            || !r.prepared
            || !r.gc_eligible
            || r.desired
            || r.worker.is_some()
            || r.client.is_some()
            || r.vm.is_some()
        {
            return Err("volume is not detached for eviction".into());
        }
        if let Some(resources) = &self.config.resources {
            resources.remove(&r.id)?;
        }
        OwnedDisk::evict_local(raw, &r.id, &self.dir(r).join("owner"))?;
        // Publish the released device/budgets only after durable local removal.
        let _map = self.entries.lock().map_err(|_| "registry poisoned")?;
        let mut next = r.clone();
        next.evicted = true;
        if let Err(error) = self.persist(&next) {
            self.admission_failed.store(true, Ordering::SeqCst);
            return Err(error);
        }
        *r = next;
        Ok(())
    }
    fn usage(&self) -> Result<Usage> {
        let map = self.entries.lock().map_err(|_| "registry poisoned")?;
        let mut usage = Usage::default();
        for id in map.keys() {
            let r: Record = read(
                &self
                    .config
                    .root
                    .join("volumes")
                    .join(id)
                    .join("record.json"),
            )?;
            if !r.reclaimed {
                usage.add_residency(r.logical_bytes, !r.evicted)?;
            }
        }
        Ok(usage)
    }
    fn store(&self) -> Result<Arc<dyn ObjectStore>> {
        Ok(Arc::new(CachedStore::new(
            Arc::new(S3Store::with_timeout(
                S3Config::from_file(&self.config.credentials)?,
                Duration::from_secs(3),
            )?),
            CACHE_BYTES as usize,
        )?))
    }
    fn prepare(&self, r: &mut Record) -> Result<()> {
        if r.prepared {
            return Ok(());
        }
        // Bound the supervisor's upload threads across simultaneous creates.
        // Other volumes' live I/O and recovery do not take this lock.
        let mut imports = self.imports.lock().map_err(|_| "import lock poisoned")?;
        use sha2::{Digest, Sha256};
        let mut file = File::open(&r.image)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        if !metadata.is_file()
            || size != r.logical_bytes
            || size == 0
            || size > 64 * 1024 * 1024 * 1024
            || !size.is_multiple_of(crate::CHUNK_BYTES as u64)
        {
            return Err("invalid raw image size/type".into());
        }
        let mut bytes = vec![0; crate::indexed::WRITE_LIMIT];
        let hash = Self::hash_image(&mut file, &mut imports, &mut bytes)?;
        if r.image_hash.is_empty() {
            r.image_hash = hash;
            self.persist(r)?;
        } else if r.image_hash != hash {
            return Err("import source changed".into());
        }
        let store = self.store()?;
        let mut disk = match store.head(&r.id)? {
            None => {
                let reference = self.import_base(&mut file, &r.image_hash, size, &mut bytes)?;
                IndexedVolume::create_from_base(store.clone(), &r.id, reference)?;
                None
            }
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
                disk.commit_import()?;
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
    fn import_base(
        &self,
        file: &mut File,
        image: &str,
        size: u64,
        bytes: &mut [u8],
    ) -> Result<crate::BaseRef> {
        use sha2::{Digest, Sha256};
        use std::io::{Seek, SeekFrom};
        let bases: Arc<dyn ObjectStore> = Arc::new(CachedStore::new(
            Arc::new(
                S3Store::with_timeout(
                    S3Config::from_file(&self.config.credentials)?,
                    Duration::from_secs(3),
                )?
                .bases()?,
            ),
            CACHE_BYTES as usize,
        )?);
        let mut disk = match IndexedVolume::open(bases.clone(), image) {
            Ok(disk) => {
                if disk.size() != size {
                    return Err("base image size mismatch".into());
                }
                return Ok(disk.export_base()?);
            }
            Err(crate::Error::NotFound) => {
                IndexedVolume::create_import(bases.clone(), image, size)?
            }
            Err(crate::Error::NotReady) => {
                IndexedVolume::resume_import(bases.clone(), image, size)?
            }
            Err(error) => return Err(error.into()),
        };
        file.seek(SeekFrom::Start(0))?;
        let mut check = Sha256::new();
        let mut at = 0;
        while at < size {
            let n = bytes.len().min((size - at) as usize);
            file.read_exact(&mut bytes[..n])?;
            check.update(&bytes[..n]);
            disk.write(at, &bytes[..n])?;
            disk.commit_import()?;
            at += n as u64;
        }
        if format!("{:x}", check.finalize()) != image {
            return Err("base image changed during import".into());
        }
        disk.finish_import()?;
        Ok(disk.export_base()?)
    }
    fn control(&self, r: &Record, op: &str) -> Result<crate::local::Status> {
        let worker = r.worker.as_ref().ok_or("worker unavailable")?;
        if let Some(resources) = &self.config.resources {
            resources.verify(&r.id, worker)?;
            if let Some(client) = &r.client {
                resources.verify(&r.id, client)?;
            }
        }
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
        if r.evicted {
            return Err("local disk evicted".into());
        }
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
        if r.evicted {
            return Ok(None);
        }
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
        if r.evicted {
            return Ok(());
        }
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
        self.persist(r)?;
        if let Some(resources) = &self.config.resources {
            resources.remove(&r.id)?;
        }
        Ok(())
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
                if let Some(resources) = &self.config.resources {
                    resources.enter(&r.id, &child)?;
                }
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
        if r.evicted {
            return Err("local resources are not reserved".into());
        }
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
                .arg(&r.image_hash)
                .arg(if self.config.local_base_reads {
                    r.image.as_os_str()
                } else {
                    std::ffi::OsStr::new("")
                })
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let p = Process::read(child.id())?.ok_or("worker exited")?;
        if let Some(resources) = &self.config.resources {
            resources.enter(&r.id, &child)?;
        }
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
        if q.operation == "resources" {
            if q.version != 1
                || !valid_id(&q.volume_id)
                || q.sandbox_dir.parent() != Some(&self.config.engine_root)
            {
                return Err("invalid resource probe".into());
            }
            if let Some(resources) = &self.config.resources {
                resources.validate(self.config.devices.len())?;
            }
            return Ok(
                serde_json::json!({"ok":true,"volume_id":q.volume_id,"resources_enforced":self.config.resources.is_some()}),
            );
        }
        let entry = self.entry(&q)?;
        let mut e = entry.foreground(!matches!(
            q.operation.as_str(),
            "usage" | "status" | "inspect"
        ))?;
        if !matches!(q.operation.as_str(), "usage" | "status" | "inspect") {
            e.mark = None;
        }
        let r = &mut e.record;
        if q.sandbox_dir != r.sandbox {
            return Err("sandbox binding mismatch".into());
        }
        if q.operation == "prepare"
            && q.logical_bytes
                .is_some_and(|expected| expected != r.logical_bytes)
        {
            return Err("admitted image sizing mismatch".into());
        }
        if q.operation == "retire" && q.logical_bytes != Some(r.logical_bytes) {
            return Err("retirement sizing mismatch".into());
        }
        if !r.deleted && q.operation != "retire" {
            self.binding(&r.id, &r.sandbox)?;
        }
        if q.operation == "usage" {
            let mut usage = accounting::volume_usage(&self.dir(r), r.logical_bytes)?;
            if r.evicted {
                usage.reservation.journal_reserved_bytes = 0;
                usage.reservation.cache_reserved_bytes = 0;
            }
            if r.reclaimed {
                usage.reservation = Usage::default();
            }
            return Ok(serde_json::json!({
                "ok": true, "volume_id": r.id,
                "usage": usage, "reclamation_complete": r.reclaimed, "local_evicted": r.evicted,
                "last_collection_unix": r.gc_last_completed,
                "limits": self.config.limits,
                "host_reservations": self.usage()?,
            }));
        }
        let mut status = None;
        if matches!(q.operation.as_str(), "delete" | "retire") {
            let first = !r.deleted;
            r.deleted = true;
            r.desired = false;
            self.persist(r)?;
            if first || r.worker.is_some() || r.client.is_some() || r.vm.is_some() {
                self.detach(r)?;
            }
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
                    self.reserve_residency(r)?;
                    r.desired = true;
                    r.gc_eligible = false;
                    r.gc_after = None;
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
                    self.reserve_residency(r)?;
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
                    r.gc_eligible = true;
                    self.persist(r)?;
                }
                _ => return Err("unknown operation".into()),
            }
        }
        e.failures = 0;
        e.retry_at = Instant::now();
        let r = &e.record;
        if q.operation == "retire" {
            return Ok(
                serde_json::json!({"ok":true,"volume_id":r.id,"reclamation_complete":r.reclaimed}),
            );
        }
        Ok(
            serde_json::json!({"ok":true,"volume_id":r.id,"device":if r.worker.is_some(){Some(&r.device)}else{None},"status":status}),
        )
    }
    fn reclaim(&self, r: &mut Record) -> Result<()> {
        let raw: Arc<dyn ObjectStore> = Arc::new(S3Store::with_timeout(
            S3Config::from_file(&self.config.credentials)?,
            Duration::from_secs(3),
        )?);
        self.reclaim_with_store(r, raw)
    }
    fn reclaim_with_store(&self, r: &mut Record, raw: Arc<dyn ObjectStore>) -> Result<()> {
        if !r.deleted || r.desired || r.worker.is_some() || r.client.is_some() || r.vm.is_some() {
            return Err("volume is not detached and deleted".into());
        }
        // Bound object-store cleanup concurrency independently of guest I/O.
        let _collector = self.reclamation.try_lock().map_err(|_| "collector busy")?;
        let owner = self.dir(r).join("owner");
        private_dir(&owner)?;
        OwnedDisk::retire(raw.clone(), &r.id, &owner)?;
        let progress = crate::reclaim::sweep(raw.as_ref(), &r.id, 16)?;
        if progress.complete {
            if let Some(resources) = &self.config.resources {
                resources.remove(&r.id)?;
            }
            fs::remove_dir_all(&owner)?;
            File::open(self.dir(r))?.sync_all()?;
            r.reclaimed = true;
            self.persist(r)?;
        }
        Ok(())
    }
    fn collect_offline(&self, e: &mut Entry, cancel: impl Fn() -> bool) -> Result<()> {
        let r = &mut e.record;
        if r.deleted
            || !r.gc_eligible
            || !r.prepared
            || r.desired
            || r.worker.is_some()
            || r.client.is_some()
            || self.vm(r)?.is_some()
            || nbd_pid(&r.device)?.is_some()
        {
            return Err("offline collection requires a stopped detached disk".into());
        }
        unused(&r.device)?;
        let _collector = self.reclamation.try_lock().map_err(|_| "collector busy")?;
        if cancel() {
            return Err("collection yielded to foreground operation".into());
        }
        let raw: Arc<dyn ObjectStore> = Arc::new(S3Store::with_timeout(
            S3Config::from_file(&self.config.credentials)?,
            Duration::from_secs(3),
        )?);
        let owner = self.dir(r).join("owner");
        private_dir(&owner)?;
        let disk = OwnedDisk::open(raw, &r.id, &owner)?;
        let progress =
            disk.collect_offline_cached(r.gc_after.as_deref(), 128, cancel, &mut e.mark)?;
        if progress.complete {
            e.mark = None;
            r.gc_last_completed = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs(),
            );
        }
        r.gc_after = progress.next_after;
        self.persist(r)
    }
    fn recover(&self, e: &mut Entry, cancel: impl Fn() -> bool) -> Result<()> {
        let r = &mut e.record;
        if r.deleted || !r.desired {
            if r.worker.is_some() || r.client.is_some() {
                self.detach(r)?;
            }
            if r.deleted {
                self.reclaim(r)?;
            } else if r.prepared && r.gc_eligible && !r.evicted {
                self.collect_offline(e, &cancel)?;
                if e.record.gc_after.is_none() && !cancel() {
                    self.evict(&mut e.record)?;
                }
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
    if args.first().is_some_and(|a| a == "warm") {
        return warm::client(&args[1..]);
    }
    if args.first().is_some_and(|a| a == "export-remote") {
        return export::client(&args[1..]);
    }
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
            // Sample before taking the operation lock, so a failed foreground
            // admission cannot be lost between job admission and cancellation.
            let ticket = entry.cancellation.load(Ordering::SeqCst);
            let Ok(mut e) = entry.try_lock() else {
                continue;
            };
            if (!e.record.deleted && e.record.evicted)
                || (!e.record.deleted
                    && !e.record.gc_eligible
                    && !e.record.desired
                    && e.record.worker.is_none()
                    && e.record.client.is_none())
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
                let _collection = if !e.record.deleted && e.record.gc_eligible && !e.record.desired
                {
                    Some(entry.collection())
                } else {
                    None
                };
                let result = if healthy {
                    Ok(())
                } else {
                    service.recover(&mut e, || {
                        entry.cancellation.load(Ordering::SeqCst) != ticket
                    })
                };
                match result {
                    Ok(()) => {
                        e.failures = 0;
                        e.retry_at = Instant::now()
                            + Duration::from_secs(if e.record.reclaimed {
                                3600
                            } else if e.record.deleted {
                                1
                            } else if !e.record.desired && e.record.prepared {
                                if e.record.gc_after.is_some() {
                                    1
                                } else {
                                    3600
                                }
                            } else {
                                2
                            });
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
                if q.operation == "warm-base" {
                    if uid != 0 || q.version != 1 {
                        return Err("root-only image preparation".into());
                    }
                    return service.warm_base(q.image.as_deref().ok_or("missing image")?);
                }
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
