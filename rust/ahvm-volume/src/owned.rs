//! Explicit, exclusive writer ownership in the same CAS object as the disk map.
//! No expiry or forced takeover: an unreachable owner blocks acquisition.
//! Use OwnedDisk for ALL guest I/O. Stop the old VM before releasing ownership.
use crate::{
    indexed::IndexedVolume,
    local::{LocalDisk, Status},
    nbd::Disk,
    Error, Head, ObjectStore, Result, MAX_MANIFEST_BYTES,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{mpsc, Arc, Mutex, RwLock},
    thread,
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: u32,
    volume: String,
    epoch: u64,
    owner: Option<String>,
    manifest: serde_json::Value,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    volume: String,
    token: String,
    epoch: u64,
}
fn hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn err<T>(r: std::io::Result<T>) -> Result<T> {
    r.map_err(|_| Error::Store)
}
fn encode(e: &Envelope) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(e).map_err(|_| Error::Corrupt)?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(Error::Corrupt);
    }
    Ok(bytes)
}
fn decode(store: Arc<dyn ObjectStore>, id: &str, h: &Head) -> Result<Envelope> {
    if h.manifest.len() > MAX_MANIFEST_BYTES || h.revision.is_empty() {
        return Err(Error::Corrupt);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&h.manifest).map_err(|_| Error::Corrupt)?;
    let e = if value["format"] == 4 {
        let e: Envelope = serde_json::from_value(value).map_err(|_| Error::Corrupt)?;
        if e.volume != id || e.epoch == 0 || e.owner.as_ref().is_some_and(|s| !hash(s)) {
            return Err(Error::Corrupt);
        }
        e
    } else {
        Envelope {
            format: 4,
            volume: id.into(),
            epoch: 0,
            owner: None,
            manifest: value,
        }
    };
    // Reuse the indexed format validator; reject incomplete/corrupt images.
    IndexedVolume::from_head(
        store,
        id,
        Head {
            revision: h.revision.clone(),
            manifest: serde_json::to_vec(&e.manifest).map_err(|_| Error::Corrupt)?,
        },
    )?;
    Ok(e)
}
struct Store {
    raw: Arc<dyn ObjectStore>,
    id: String,
    identity: Identity,
    active: RwLock<bool>,
    _lock: File,
}
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnedStore")
    }
}
impl Store {
    fn active(&self) -> Result<std::sync::RwLockReadGuard<'_, bool>> {
        let guard = self.active.read().map_err(|_| Error::ReopenRequired)?;
        if !*guard {
            return Err(Error::ReopenRequired);
        }
        Ok(guard)
    }
    fn matches(&self, e: &Envelope) -> bool {
        e.epoch == self.identity.epoch && e.owner.as_deref() == Some(&self.identity.token)
    }
    fn open(raw: Arc<dyn ObjectStore>, id: &str, dir: &Path) -> Result<Arc<Self>> {
        if !crate::valid_id(id) {
            return Err(Error::InvalidInput);
        }
        let meta = err(fs::symlink_metadata(dir))?;
        if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidInput);
        }
        let lock_path = dir.join("owner.lock");
        for path in [&lock_path, &dir.join("owner.json")] {
            match fs::symlink_metadata(path) {
                Ok(m) if m.is_file() && m.permissions().mode() & 0o077 == 0 => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(Error::InvalidInput),
            }
        }
        let lock = err(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path))?;
        lock.try_lock().map_err(|_| Error::Conflict)?;
        let head = raw.head(id)?.ok_or(Error::NotFound)?;
        let mut envelope = decode(raw.clone(), id, &head)?;
        if envelope.epoch == 0 {
            return Err(Error::NotReady);
        }
        let identity_path = dir.join("owner.json");
        let identity: Identity = if identity_path.exists() {
            let mut bytes = Vec::new();
            err(err(File::open(&identity_path))?
                .take(4097)
                .read_to_end(&mut bytes))?;
            if bytes.len() > 4096 {
                return Err(Error::Corrupt);
            }
            let identity: Identity = serde_json::from_slice(&bytes).map_err(|_| Error::Corrupt)?;
            if identity.volume != id || !hash(&identity.token) || identity.epoch == 0 {
                return Err(Error::Corrupt);
            }
            identity
        } else {
            if envelope.owner.is_some() {
                return Err(Error::Conflict);
            }
            let mut bytes = [0; 32];
            err(File::open("/dev/urandom"))?
                .read_exact(&mut bytes)
                .map_err(|_| Error::Store)?;
            let identity = Identity {
                volume: id.into(),
                token: bytes.iter().map(|b| format!("{b:02x}")).collect(),
                epoch: envelope.epoch.checked_add(1).ok_or(Error::Corrupt)?,
            };
            let mut f = err(OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&identity_path))?;
            err(f.write_all(&serde_json::to_vec(&identity).map_err(|_| Error::Corrupt)?))?;
            err(f.sync_all())?;
            err(File::open(dir))?.sync_all().map_err(|_| Error::Store)?;
            identity
        };
        // A previous attempt may have left complete bytes but failed their fsync.
        // Persist the identity again before retrying a conditional remote claim.
        err(err(File::open(&identity_path))?.sync_all())?;
        err(err(File::open(dir))?.sync_all())?;
        if let Some(parent) = dir.parent() {
            err(err(File::open(parent))?.sync_all())?;
        }
        if envelope.owner.as_deref() != Some(&identity.token) || envelope.epoch != identity.epoch {
            // Only an unowned predecessor can be claimed. A stale identity can
            // never regain ownership after release or a later acquisition.
            if envelope.owner.is_some() || envelope.epoch.checked_add(1) != Some(identity.epoch) {
                return Err(Error::Conflict);
            }
            envelope.epoch = identity.epoch;
            envelope.owner = Some(identity.token.clone());
            let rev = raw
                .publish(id, Some(&head.revision), &encode(&envelope)?)
                .map_err(crate::publication_error)?;
            if rev.is_empty() {
                return Err(Error::Uncertain);
            }
        }
        Ok(Arc::new(Self {
            raw,
            id: id.into(),
            identity,
            active: RwLock::new(true),
            _lock: lock,
        }))
    }
    fn release(&self) -> Result<()> {
        let mut active = self.active.write().map_err(|_| Error::ReopenRequired)?;
        // Closing is irreversible even on a lost response. No guest I/O or
        // background commit may use this owner again; release itself is retryable.
        *active = false;
        let h = self.raw.head(&self.id)?.ok_or(Error::NotFound)?;
        let mut e = decode(self.raw.clone(), &self.id, &h)?;
        if e.epoch > self.identity.epoch || (e.epoch == self.identity.epoch && e.owner.is_none()) {
            return Ok(());
        }
        if !self.matches(&e) {
            return Err(Error::Conflict);
        }
        e.owner = None;
        let rev = self
            .raw
            .publish(&self.id, Some(&h.revision), &encode(&e)?)
            .map_err(crate::publication_error)?;
        if rev.is_empty() {
            return Err(Error::Uncertain);
        }
        Ok(())
    }
}
impl ObjectStore for Store {
    fn prefetch(&self, id: &str, digests: &[String]) {
        if let Ok(_active) = self.active() {
            if id == self.id {
                self.raw.prefetch(id, digests);
            }
        }
    }
    fn cached_chunk(&self, id: &str, digest: &str) -> Option<Vec<u8>> {
        let _active = self.active().ok()?;
        if id != self.id {
            return None;
        }
        self.raw.cached_chunk(id, digest)
    }
    fn head(&self, id: &str) -> Result<Option<Head>> {
        let _active = self.active()?;
        if id != self.id {
            return Err(Error::InvalidInput);
        }
        let h = self.raw.head(id)?.ok_or(Error::NotFound)?;
        let e = decode(self.raw.clone(), id, &h)?;
        if !self.matches(&e) {
            return Err(Error::Conflict);
        }
        Ok(Some(Head {
            revision: h.revision,
            manifest: serde_json::to_vec(&e.manifest).map_err(|_| Error::Corrupt)?,
        }))
    }
    fn chunk(&self, id: &str, digest: &str) -> Result<Vec<u8>> {
        let _active = self.active()?;
        if id != self.id {
            return Err(Error::InvalidInput);
        }
        self.raw.chunk(id, digest)
    }
    fn put_chunk(&self, id: &str, digest: &str, bytes: &[u8]) -> Result<()> {
        let _active = self.active()?;
        if id != self.id {
            return Err(Error::InvalidInput);
        }
        self.raw.put_chunk(id, digest, bytes)
    }
    fn publish(&self, id: &str, expected: Option<&str>, manifest: &[u8]) -> Result<String> {
        let _active = self.active()?;
        if id != self.id || expected.is_none() {
            return Err(Error::InvalidInput);
        }
        let h = Head {
            revision: expected.unwrap().into(),
            manifest: manifest.into(),
        };
        let mut e = decode(self.raw.clone(), id, &h)?;
        // The inner indexed layer publishes formats 2/3 only. The opaque outer
        // revision makes data and ownership replacement ONE conditional write.
        if e.epoch != 0 {
            return Err(Error::InvalidInput);
        }
        e.epoch = self.identity.epoch;
        e.owner = Some(self.identity.token.clone());
        self.raw.publish(id, expected, &encode(&e)?)
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Active,
    Closing,
    Released,
}
/// Disposable marking cache, valid only for this exact immutable head revision.
#[derive(Debug)]
pub(crate) struct OfflineMark {
    id: String,
    revision: String,
    live: Vec<[u8; 32]>,
}
/// Private owner directory must be retained with the journal for service restart.
/// Another host uses its own fresh directory only AFTER explicit release.
#[derive(Clone)]
pub struct OwnedDisk {
    disk: LocalDisk,
    store: Arc<Store>,
    phase: Arc<RwLock<Phase>>,
}
impl std::fmt::Debug for OwnedDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OwnedDisk")
    }
}
impl OwnedDisk {
    /// Offline-only enrollment. The caller must stop all legacy writers first.
    /// Never use enrollment as a takeover operation on a live unowned disk.
    pub fn enroll(raw: Arc<dyn ObjectStore>, id: &str) -> Result<()> {
        if !crate::valid_id(id) {
            return Err(Error::InvalidInput);
        }
        let h = raw.head(id)?.ok_or(Error::NotFound)?;
        let mut e = decode(raw.clone(), id, &h)?;
        if e.epoch != 0 {
            return Ok(());
        }
        e.epoch = 1;
        let revision = raw
            .publish(id, Some(&h.revision), &encode(&e)?)
            .map_err(crate::publication_error)?;
        if revision.is_empty() {
            return Err(Error::Uncertain);
        }
        Ok(())
    }
    /// Permanently retire a deleted disk. Caller must stop the VM, detach NBD,
    /// and serialize against import. Does not flush data the user chose to delete.
    /// The local owner lock and remote CAS prevent retiring another live owner.
    pub fn retire(raw: Arc<dyn ObjectStore>, id: &str, dir: &Path) -> Result<()> {
        if !crate::valid_id(id) || !dir.is_absolute() {
            return Err(Error::InvalidInput);
        }
        let head = raw.head(id)?;
        if let Some(h) = &head {
            if crate::reclaim::retired(id, h)? {
                return Ok(());
            }
            let v: serde_json::Value =
                serde_json::from_slice(&h.manifest).map_err(|_| Error::Corrupt)?;
            if v["format"] != 2 {
                let envelope = decode(raw.clone(), id, h)?;
                if envelope.epoch != 0 && envelope.owner.is_none() {
                    // Explicit release already fenced the old owner. Compete
                    // with any new acquisition through the same conditional head.
                    raw.publish(id, Some(&h.revision), &crate::reclaim::tombstone(id)?)
                        .map_err(crate::publication_error)?;
                    return Ok(());
                }
                let store = Store::open(raw.clone(), id, dir)?;
                let mut active = store.active.write().map_err(|_| Error::ReopenRequired)?;
                *active = false;
                let h = raw.head(id)?.ok_or(Error::NotFound)?;
                let e = decode(raw.clone(), id, &h)?;
                if !store.matches(&e) {
                    return Err(Error::Conflict);
                }
                raw.publish(id, Some(&h.revision), &crate::reclaim::tombstone(id)?)
                    .map_err(crate::publication_error)?;
                return Ok(());
            }
            // Incomplete imports have no owned worker. Validate their identity
            // and format before retiring under the supervisor operation lock.
            if v["ready"] == true {
                IndexedVolume::from_head(raw.clone(), id, h.clone())?;
            } else {
                let size = v["size"].as_u64().ok_or(Error::Corrupt)?;
                IndexedVolume::resume_import(raw.clone(), id, size)?;
            }
        }
        raw.publish(
            id,
            head.as_ref().map(|h| h.revision.as_str()),
            &crate::reclaim::tombstone(id)?,
        )
        .map_err(crate::publication_error)?;
        Ok(())
    }
    pub fn open(raw: Arc<dyn ObjectStore>, id: &str, dir: &Path) -> Result<Self> {
        if !dir.is_absolute() {
            return Err(Error::InvalidInput);
        }
        let store = Store::open(raw, id, dir)?;
        let journal = dir.join("journal");
        if !journal.exists() {
            err(fs::create_dir(&journal))?;
            err(fs::set_permissions(
                &journal,
                fs::Permissions::from_mode(0o700),
            ))?;
        }
        // The journal's own fsync cannot persist its entry in the owner directory.
        err(err(File::open(dir))?.sync_all())?;
        if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
            err(err(File::open(parent))?.sync_all())?;
        }
        let disk = LocalDisk::open(store.clone(), id, &journal)?;
        Ok(Self {
            disk,
            store,
            phase: Arc::new(RwLock::new(Phase::Active)),
        })
    }
    fn active(&self) -> Result<std::sync::RwLockReadGuard<'_, Phase>> {
        let guard = self.phase.read().map_err(|_| Error::ReopenRequired)?;
        if *guard != Phase::Active {
            return Err(Error::ReopenRequired);
        }
        Ok(guard)
    }
    pub fn status(&self) -> Result<Status> {
        let _active = self.active()?;
        Ok(self.disk.status())
    }
    pub fn sync_remote(&self) -> Result<Status> {
        let _active = self.active()?;
        self.disk.sync_remote()
    }
    /// Caller must first stop its VM. Blocks new I/O, drains the journal, then
    /// releases the remote owner atomically. Does not expire or force takeover.
    pub fn release(&self) -> Result<()> {
        let mut phase = self.phase.write().map_err(|_| Error::ReopenRequired)?;
        if *phase == Phase::Released {
            return Ok(());
        }
        if *phase == Phase::Active {
            self.disk.sync_remote()?;
            *phase = Phase::Closing;
        }
        self.store.release()?;
        *phase = Phase::Released;
        Ok(())
    }
    /// Caller must hold the sandbox operation lock and prove its VM/NBD are
    /// detached. This handle holds the exclusive owner lock throughout marking
    /// and deletion; no background replication task may be started for it.
    /// Pending local writes are a refusal, never disposable cache.
    pub fn collect_offline(
        &self,
        after: Option<&str>,
        limit: usize,
        cancel: impl Fn() -> bool,
    ) -> Result<crate::reclaim::Collection> {
        self.collect_offline_cached(after, limit, cancel, &mut None)
    }
    pub(crate) fn collect_offline_cached(
        &self,
        after: Option<&str>,
        limit: usize,
        cancel: impl Fn() -> bool,
        mark: &mut Option<OfflineMark>,
    ) -> Result<crate::reclaim::Collection> {
        if !(1..=128).contains(&limit) || after.is_some_and(|s| !hash(s)) {
            return Err(Error::InvalidInput);
        }
        let phase = self.phase.write().map_err(|_| Error::ReopenRequired)?;
        if *phase != Phase::Active {
            return Err(Error::ReopenRequired);
        }
        if cancel() {
            return Err(Error::Deadline);
        }
        let status = self.disk.status();
        if status.local_failed {
            return Err(Error::ReopenRequired);
        }
        if status.pending_bytes != 0 {
            return Err(Error::Backpressure);
        }
        let head = self.store.head(&self.store.id)?.ok_or(Error::NotFound)?;
        if mark
            .as_ref()
            .is_none_or(|m| m.id != self.store.id || m.revision != head.revision)
        {
            // Do not retain a stale large graph while allocating its replacement.
            *mark = None;
            let disk = IndexedVolume::from_head(self.store.clone(), &self.store.id, head.clone())?;
            *mark = Some(OfflineMark {
                id: self.store.id.clone(),
                revision: head.revision.clone(),
                live: disk.references(&cancel)?,
            });
        }
        let live = &mark.as_ref().unwrap().live;
        let hashes = self
            .store
            .raw
            .list_chunks_after(&self.store.id, after, limit)?;
        if hashes.len() > limit
            || hashes.windows(2).any(|w| w[0] >= w[1])
            || hashes
                .iter()
                .any(|h| !hash(h) || after.is_some_and(|a| h.as_str() <= a))
        {
            return Err(Error::Corrupt);
        }
        if cancel() {
            return Err(Error::Deadline);
        }
        // Defense in depth: this pass must still own exactly the marked head.
        if self
            .store
            .head(&self.store.id)?
            .is_none_or(|h| h.revision != head.revision)
        {
            return Err(Error::Conflict);
        }
        let mut deleted = 0;
        for hash in &hashes {
            if cancel() {
                return Err(Error::Deadline);
            }
            if live.binary_search(&crate::indexed::decode(hash)?).is_err() {
                self.store.raw.delete_chunk(&self.store.id, hash)?;
                deleted += 1;
            }
        }
        Ok(crate::reclaim::Collection {
            scanned_objects: hashes.len(),
            deleted_objects: deleted,
            complete: hashes.is_empty(),
            next_after: hashes.last().cloned(),
        })
    }
    pub fn background(&self) -> Background {
        let (tx, rx) = mpsc::channel();
        let disk = self.clone();
        let worker = thread::spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(Duration::from_secs(1))
            {
                let _ = disk.sync_remote();
            }
        });
        Background {
            stop: tx,
            worker: Mutex::new(Some(worker)),
        }
    }
}
impl Disk for OwnedDisk {
    fn size(&self) -> u64 {
        self.disk.size()
    }
    fn read(&mut self, at: u64, out: &mut [u8]) -> Result<()> {
        let phase = self.phase.clone();
        let guard = phase.read().map_err(|_| Error::ReopenRequired)?;
        if *guard != Phase::Active {
            return Err(Error::ReopenRequired);
        }
        self.disk.read(at, out)
    }
    fn write(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        let phase = self.phase.clone();
        let guard = phase.read().map_err(|_| Error::ReopenRequired)?;
        if *guard != Phase::Active {
            return Err(Error::ReopenRequired);
        }
        self.disk.write(at, bytes)
    }
    fn flush(&mut self) -> Result<()> {
        let phase = self.phase.clone();
        let guard = phase.read().map_err(|_| Error::ReopenRequired)?;
        if *guard != Phase::Active {
            return Err(Error::ReopenRequired);
        }
        self.disk.flush()
    }
}
#[derive(Debug)]
pub struct Background {
    stop: mpsc::Sender<()>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Drop for Background {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(w) = self.worker.get_mut().unwrap().take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, Default)]
    struct Memory {
        heads: Mutex<std::collections::HashMap<String, Head>>,
        chunks: Mutex<std::collections::HashMap<(String, String), Vec<u8>>>,
    }
    impl ObjectStore for Memory {
        fn head(&self, id: &str) -> Result<Option<Head>> {
            Ok(self.heads.lock().unwrap().get(id).cloned())
        }
        fn chunk(&self, id: &str, h: &str) -> Result<Vec<u8>> {
            self.chunks
                .lock()
                .unwrap()
                .get(&(id.into(), h.into()))
                .cloned()
                .ok_or(Error::Corrupt)
        }
        fn put_chunk(&self, id: &str, h: &str, b: &[u8]) -> Result<()> {
            self.chunks
                .lock()
                .unwrap()
                .insert((id.into(), h.into()), b.into());
            Ok(())
        }
        fn publish(&self, id: &str, expected: Option<&str>, bytes: &[u8]) -> Result<String> {
            let mut heads = self.heads.lock().unwrap();
            let old = heads.get(id);
            if old.map(|h| h.revision.as_str()) != expected {
                return Err(Error::Conflict);
            }
            let n = old.map(|h| h.revision.parse::<u64>().unwrap()).unwrap_or(0) + 1;
            let revision = n.to_string();
            heads.insert(
                id.into(),
                Head {
                    revision: revision.clone(),
                    manifest: bytes.into(),
                },
            );
            Ok(revision)
        }
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "ahvm-owned-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&p).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
            Self(p)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[derive(Debug, Default)]
    struct Faults {
        inner: Memory,
        lost: AtomicBool,
        offline: AtomicBool,
    }
    impl ObjectStore for Faults {
        fn head(&self, id: &str) -> Result<Option<Head>> {
            if self.offline.load(Ordering::SeqCst) {
                return Err(Error::Store);
            }
            self.inner.head(id)
        }
        fn chunk(&self, id: &str, h: &str) -> Result<Vec<u8>> {
            self.inner.chunk(id, h)
        }
        fn put_chunk(&self, id: &str, h: &str, b: &[u8]) -> Result<()> {
            self.inner.put_chunk(id, h, b)
        }
        fn publish(&self, id: &str, r: Option<&str>, b: &[u8]) -> Result<String> {
            let revision = self.inner.publish(id, r, b)?;
            if self.lost.swap(false, Ordering::SeqCst) {
                Err(Error::Uncertain)
            } else {
                Ok(revision)
            }
        }
    }
    fn fixture() -> Arc<Faults> {
        let s = Arc::new(Faults::default());
        IndexedVolume::create(s.clone(), "owned", 4 * crate::CHUNK_BYTES as u64).unwrap();
        OwnedDisk::enroll(s.clone(), "owned").unwrap();
        s
    }
    #[test]
    fn handoff_drains_data_and_revokes_all_old_handles() {
        let s = fixture();
        let a = Directory::new();
        let b = Directory::new();
        let mut old = OwnedDisk::open(s.clone(), "owned", &a.0).unwrap();
        let mut clone = old.clone();
        assert!(matches!(
            OwnedDisk::open(s.clone(), "owned", &b.0),
            Err(Error::Conflict)
        ));
        old.write(0, b"persisted").unwrap();
        old.flush().unwrap();
        old.release().unwrap();
        assert!(old.read(0, &mut [0; 9]).is_err());
        assert!(clone.write(0, b"stale").is_err());
        assert!(clone.flush().is_err());
        let mut next = OwnedDisk::open(s.clone(), "owned", &b.0).unwrap();
        let mut bytes = [0; 9];
        next.read(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"persisted");
        old.release().unwrap(); // cannot clear the next owner's claim
        next.write(0, b"new").unwrap();
        next.release().unwrap();
        assert!(IndexedVolume::open(s, "owned").is_err()); // legacy readers reject envelope
    }
    #[test]
    fn restart_reuses_private_identity_and_pending_journal_not_another_owner() {
        let s = fixture();
        let a = Directory::new();
        let b = Directory::new();
        let mut disk = OwnedDisk::open(s.clone(), "owned", &a.0).unwrap();
        assert!(OwnedDisk::open(s.clone(), "owned", &a.0).is_err());
        disk.write(0, b"local").unwrap();
        disk.flush().unwrap();
        drop(disk);
        assert!(OwnedDisk::open(s.clone(), "owned", &b.0).is_err());
        let mut resumed = OwnedDisk::open(s.clone(), "owned", &a.0).unwrap();
        let mut bytes = [0; 5];
        resumed.read(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"local");
        resumed.release().unwrap();
        drop(resumed);
        assert!(OwnedDisk::open(s, "owned", &a.0).is_err()); // released epochs never revive
    }
    #[test]
    fn lost_claim_response_is_reconciled_by_the_same_identity() {
        let s = fixture();
        let a = Directory::new();
        let b = Directory::new();
        s.lost.store(true, Ordering::SeqCst);
        assert!(matches!(
            OwnedDisk::open(s.clone(), "owned", &a.0),
            Err(Error::Uncertain)
        ));
        assert!(OwnedDisk::open(s.clone(), "owned", &b.0).is_err());
        let disk = OwnedDisk::open(s, "owned", &a.0).unwrap();
        disk.release().unwrap();
    }
    #[test]
    fn lost_release_reply_cannot_reactivate_or_release_a_new_owner() {
        let s = fixture();
        let a = Directory::new();
        let b = Directory::new();
        let mut old = OwnedDisk::open(s.clone(), "owned", &a.0).unwrap();
        s.lost.store(true, Ordering::SeqCst);
        assert!(old.release().is_err());
        assert!(old.flush().is_err());
        let mut next = OwnedDisk::open(s.clone(), "owned", &b.0).unwrap();
        old.release().unwrap();
        next.write(0, b"new").unwrap();
        next.release().unwrap();
    }
    #[test]
    fn disconnected_owner_blocks_takeover_and_local_fsync_still_works() {
        let s = fixture();
        let a = Directory::new();
        let b = Directory::new();
        let mut old = OwnedDisk::open(s.clone(), "owned", &a.0).unwrap();
        s.offline.store(true, Ordering::SeqCst);
        old.write(0, b"offline").unwrap();
        old.flush().unwrap();
        assert!(old.release().is_err());
        s.offline.store(false, Ordering::SeqCst);
        assert!(OwnedDisk::open(s.clone(), "owned", &b.0).is_err());
        old.release().unwrap();
    }
    #[test]
    fn competing_acquisitions_have_exactly_one_winner() {
        let s = fixture();
        let a = Directory::new();
        let b = Directory::new();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let run = |dir: Directory| {
            let s = s.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                (OwnedDisk::open(s, "owned", &dir.0), dir)
            })
        };
        let left = run(a);
        let right = run(b);
        let (l, _a) = left.join().unwrap();
        let (r, _b) = right.join().unwrap();
        assert_ne!(l.is_ok(), r.is_ok());
    }
    #[test]
    fn legacy_volume_needs_offline_enrollment_and_identity_cannot_cross_volumes() {
        let s = Arc::new(Memory::default());
        IndexedVolume::create(s.clone(), "owned", crate::CHUNK_BYTES as u64).unwrap();
        let a = Directory::new();
        assert!(matches!(
            OwnedDisk::open(s.clone(), "owned", &a.0),
            Err(Error::NotReady)
        ));
        OwnedDisk::enroll(s.clone(), "owned").unwrap();
        let disk = OwnedDisk::open(s.clone(), "owned", &a.0).unwrap();
        drop(disk);
        IndexedVolume::create(s.clone(), "different", crate::CHUNK_BYTES as u64).unwrap();
        OwnedDisk::enroll(s.clone(), "different").unwrap();
        assert!(OwnedDisk::open(s, "different", &a.0).is_err());
    }
    #[test]
    fn uploads_do_not_block_local_flush_and_release_drains_newer_writes() {
        #[derive(Debug)]
        struct Park {
            inner: Memory,
            entered: mpsc::Sender<()>,
            ready: Arc<(Mutex<bool>, std::sync::Condvar)>,
        }
        impl ObjectStore for Park {
            fn head(&self, id: &str) -> Result<Option<Head>> {
                self.inner.head(id)
            }
            fn chunk(&self, id: &str, h: &str) -> Result<Vec<u8>> {
                self.inner.chunk(id, h)
            }
            fn publish(&self, id: &str, r: Option<&str>, b: &[u8]) -> Result<String> {
                self.inner.publish(id, r, b)
            }
            fn put_chunk(&self, id: &str, h: &str, b: &[u8]) -> Result<()> {
                let _ = self.entered.send(());
                let mut ready = self.ready.0.lock().unwrap();
                while !*ready {
                    ready = self.ready.1.wait(ready).unwrap();
                }
                self.inner.put_chunk(id, h, b)
            }
        }
        let (tx, rx) = mpsc::channel();
        let ready = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let raw = Arc::new(Park {
            inner: Memory::default(),
            entered: tx,
            ready: ready.clone(),
        });
        IndexedVolume::create(raw.clone(), "owned", crate::CHUNK_BYTES as u64).unwrap();
        OwnedDisk::enroll(raw.clone(), "owned").unwrap();
        let a = Directory::new();
        let b = Directory::new();
        let mut disk = OwnedDisk::open(raw.clone(), "owned", &a.0).unwrap();
        disk.write(0, b"old").unwrap();
        disk.flush().unwrap();
        let bg = disk.clone();
        let upload = thread::spawn(move || bg.sync_remote());
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let mut foreground = disk.clone();
        let (done, receive) = mpsc::channel();
        let writer = thread::spawn(move || {
            foreground.write(0, b"new").unwrap();
            foreground.flush().unwrap();
            done.send(()).unwrap();
        });
        let progressed = receive.recv_timeout(Duration::from_secs(3));
        assert!(OwnedDisk::open(raw.clone(), "owned", &b.0).is_err());
        *ready.0.lock().unwrap() = true;
        ready.1.notify_all();
        writer.join().unwrap();
        upload.join().unwrap().unwrap();
        progressed.unwrap();
        disk.release().unwrap();
        let mut next = OwnedDisk::open(raw, "owned", &b.0).unwrap();
        let mut bytes = [0; 3];
        next.read(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"new");
        next.release().unwrap();
    }
}
