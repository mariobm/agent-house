//! Experimental local-durable journal with asynchronous remote replication.
//! A guest flush syncs the journal, NOT R2. Host-disk loss can lose pending writes.
use crate::{
    digest,
    indexed::{IndexedVolume, Replication, DIRTY_LIMIT, WRITE_LIMIT},
    nbd::Disk,
    Error, ObjectStore, Result, CHUNK_BYTES,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};
const MAGIC: &[u8; 8] = b"AHVMWL01";
const RECORD: usize = 16 + CHUNK_BYTES + 64;
const LOG_LIMIT: u64 = 256 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    volume: String,
    id: String,
    base: String,
    acknowledged: u64,
    sequence: u64,
}
#[derive(Clone)]
struct Pending {
    sequence: u64,
    bytes: Arc<Vec<u8>>,
}
struct State {
    header: Header,
    file: File,
    _lock: File,
    directory: PathBuf,
    reader: IndexedVolume,
    pending: BTreeMap<u64, Pending>,
    sequence: u64,
    local_sequence: u64,
    failed: bool,
    replication_failed: bool,
}
#[derive(Debug, Serialize)]
pub struct Status {
    pub local_failed: bool,
    pub local_sequence: u64,
    pub remote_sequence: u64,
    pub pending_bytes: usize,
    pub replication_failed: bool,
}
#[derive(Clone)]
pub struct LocalDisk {
    state: Arc<Mutex<State>>,
    writer: Arc<Mutex<()>>,
    store: Arc<dyn ObjectStore>,
    id: String,
}
impl std::fmt::Debug for LocalDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalDurableDisk")
    }
}
fn io<T>(r: std::io::Result<T>) -> Result<T> {
    r.map_err(|_| Error::Store)
}
fn secure(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o077 == 0 => Ok(()),
        Ok(_) => Err(Error::InvalidInput),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Error::Store),
    }
}
fn header(file: &mut File, h: &Header) -> Result<()> {
    let b = serde_json::to_vec(h).map_err(|_| Error::Corrupt)?;
    if b.len() > 4096 {
        return Err(Error::Corrupt);
    }
    io(file.write_all(MAGIC))?;
    io(file.write_all(&(b.len() as u32).to_be_bytes()))?;
    io(file.write_all(&b))?;
    io(file.write_all(digest(&b).as_bytes()))
}
fn record(file: &mut File, index: u64, p: &Pending) -> Result<()> {
    let mut b = Vec::with_capacity(RECORD);
    b.extend_from_slice(&p.sequence.to_be_bytes());
    b.extend_from_slice(&index.to_be_bytes());
    b.extend_from_slice(&p.bytes);
    let hash = digest(&b);
    b.extend_from_slice(hash.as_bytes());
    io(file.write_all(&b))
}
impl State {
    fn status(&self) -> Status {
        Status {
            local_failed: self.failed,
            local_sequence: self.local_sequence,
            remote_sequence: self.header.acknowledged,
            pending_bytes: self.pending.len() * CHUNK_BYTES,
            replication_failed: self.replication_failed,
        }
    }
    fn check(&self, at: u64, len: usize) -> Result<()> {
        if self.failed {
            return Err(Error::ReopenRequired);
        }
        if len > WRITE_LIMIT
            || at
                .checked_add(len as u64)
                .is_none_or(|end| end > self.reader.size())
        {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }
    fn compact(&mut self) -> Result<()> {
        let result = self.compact_inner();
        // A failure after rename may leave the old descriptor unlinked. Never
        // acknowledge subsequent writes through it; reopening selects the path.
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn compact_inner(&mut self) -> Result<()> {
        let temp = self.directory.join("journal.new");
        secure(&temp)?;
        if temp.exists() {
            io(fs::remove_file(&temp))?;
        }
        let mut out = io(OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp))?;
        self.header.sequence = self.sequence;
        header(&mut out, &self.header)?;
        let mut entries: Vec<_> = self.pending.iter().collect();
        entries.sort_by_key(|(index, p)| (p.sequence, **index));
        for (index, p) in entries {
            record(&mut out, *index, p)?;
        }
        io(out.sync_all())?;
        io(fs::rename(&temp, self.directory.join("journal")))?;
        io(io(File::open(&self.directory))?.sync_all())?;
        self.file = out;
        self.local_sequence = self.sequence;
        Ok(())
    }
    fn accept(&mut self, remote: IndexedVolume) -> Result<()> {
        let ack = if remote.revision() == self.header.base {
            self.header.acknowledged
        } else {
            match remote.replication() {
                Some(r)
                    if r.id == self.header.id
                        && r.sequence >= self.header.acknowledged
                        && r.sequence <= self.sequence =>
                {
                    r.sequence
                }
                _ => return Err(Error::Conflict),
            }
        };
        self.header.base = remote.revision().into();
        self.header.acknowledged = ack;
        self.pending.retain(|_, p| p.sequence > ack);
        self.reader = remote;
        Ok(())
    }
}
impl LocalDisk {
    pub fn open(store: Arc<dyn ObjectStore>, id: &str, directory: &Path) -> Result<Self> {
        let m = io(fs::symlink_metadata(directory))?;
        if !m.is_dir() || m.permissions().mode() & 0o077 != 0 {
            return Err(Error::InvalidInput);
        }
        let lock_path = directory.join("lock");
        secure(&lock_path)?;
        let lock = io(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path))?;
        lock.try_lock().map_err(|_| Error::Conflict)?;
        let remote = IndexedVolume::open(store.clone(), id)?;
        let path = directory.join("journal");
        secure(&path)?;
        let exists = path.exists();
        let mut file = io(OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path))?;
        if io(file.metadata())?.len() > LOG_LIMIT {
            return Err(Error::Corrupt);
        }
        let mut pending = BTreeMap::new();
        let h;
        let mut sequence;
        if exists {
            let mut start = [0; 12];
            io(file.read_exact(&mut start))?;
            let len = u32::from_be_bytes(start[8..].try_into().unwrap()) as usize;
            if &start[..8] != MAGIC || len > 4096 {
                return Err(Error::Corrupt);
            }
            let mut bytes = vec![0; len];
            io(file.read_exact(&mut bytes))?;
            let mut checksum = [0; 64];
            io(file.read_exact(&mut checksum))?;
            if digest(&bytes).as_bytes() != checksum {
                return Err(Error::Corrupt);
            }
            h = serde_json::from_slice::<Header>(&bytes).map_err(|_| Error::Corrupt)?;
            if h.volume != id
                || h.id.len() != 64
                || !h.id.bytes().all(|c| c.is_ascii_hexdigit())
                || h.acknowledged > h.sequence
            {
                return Err(Error::Corrupt);
            }
            let replay_ack = remote
                .replication()
                .filter(|r| r.id == h.id)
                .map_or(h.acknowledged, |r| r.sequence.max(h.acknowledged));
            sequence = h.sequence;
            let mut last = h.acknowledged;
            loop {
                let at = io(file.stream_position())?;
                let left = io(file.metadata())?.len() - at;
                if left == 0 {
                    break;
                }
                if left < RECORD as u64 {
                    io(file.set_len(at))?;
                    break;
                } // unacknowledged torn tail
                let mut b = vec![0; RECORD];
                io(file.read_exact(&mut b))?;
                if digest(&b[..RECORD - 64]).as_bytes() != &b[RECORD - 64..] {
                    return Err(Error::Corrupt);
                }
                let seq = u64::from_be_bytes(b[..8].try_into().unwrap());
                let index = u64::from_be_bytes(b[8..16].try_into().unwrap());
                if seq == 0 || seq < last || index >= remote.size() / CHUNK_BYTES as u64 {
                    return Err(Error::Corrupt);
                }
                sequence = sequence.max(seq);
                last = seq;
                if seq > replay_ack {
                    pending.insert(
                        index,
                        Pending {
                            sequence: seq,
                            bytes: Arc::new(b[16..16 + CHUNK_BYTES].to_vec()),
                        },
                    );
                }
                if pending.len() * CHUNK_BYTES > DIRTY_LIMIT {
                    return Err(Error::Corrupt);
                }
            }
        } else {
            let mut random = [0; 32];
            io(File::open("/dev/urandom"))?
                .read_exact(&mut random)
                .map_err(|_| Error::Store)?;
            h = Header {
                volume: id.into(),
                id: digest(&random),
                base: remote.revision().into(),
                acknowledged: 0,
                sequence: 0,
            };
            sequence = 0;
            header(&mut file, &h)?;
            io(file.sync_all())?;
            io(io(File::open(directory))?.sync_all())?;
        }
        let mut s = State {
            header: h,
            file,
            _lock: lock,
            directory: directory.into(),
            reader: remote.clean_copy()?,
            pending,
            sequence,
            local_sequence: sequence,
            failed: false,
            replication_failed: false,
        };
        s.accept(remote)?;
        s.compact()?;
        Ok(Self {
            state: Arc::new(Mutex::new(s)),
            writer: Arc::new(Mutex::new(())),
            store,
            id: id.into(),
        })
    }
    pub fn status(&self) -> Status {
        self.state.lock().unwrap().status()
    }
    /// Barrier for all writes admitted before this call's snapshot. Guest flush
    /// is deliberately separate and does not call this method.
    pub fn sync_remote(&self) -> Result<Status> {
        let _writer = self.writer.lock().unwrap();
        let result = self.replicate();
        let mut state = self.state.lock().unwrap();
        state.replication_failed = result.is_err();
        result.map(|_| state.status())
    }
    fn replicate(&self) -> Result<Status> {
        {
            let s = self.state.lock().unwrap();
            s.check(0, 0)?;
            if s.pending.is_empty() {
                return Ok(s.status());
            }
        }
        // Reconcile a successful CAS whose response or local compaction was lost.
        let mut remote = IndexedVolume::open(self.store.clone(), &self.id)?;
        let (entries, mark) = {
            let mut s = self.state.lock().unwrap();
            if let Err(e) = s.accept(remote.clean_copy()?) {
                s.failed = true;
                return Err(e);
            }
            if let Err(e) = io(s.file.sync_all()) {
                s.failed = true;
                return Err(e);
            }
            s.local_sequence = s.sequence;
            if s.pending.is_empty() {
                s.compact()?;
                return Ok(s.status());
            }
            (
                s.pending.clone(),
                Replication {
                    id: s.header.id.clone(),
                    sequence: s.sequence,
                },
            )
        };
        for (index, p) in entries {
            remote.write(index * CHUNK_BYTES as u64, &p.bytes)?;
        }
        remote.commit_replication(mark)?;
        let mut s = self.state.lock().unwrap();
        s.accept(remote)?;
        if let Err(e) = s.compact() {
            s.failed = true;
            return Err(e);
        }
        Ok(s.status())
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
            worker: Some(worker),
        }
    }
}
impl Disk for LocalDisk {
    fn size(&self) -> u64 {
        self.state.lock().unwrap().reader.size()
    }
    fn read(&mut self, at: u64, out: &mut [u8]) -> Result<()> {
        let s = self.state.lock().unwrap();
        s.check(at, out.len())?;
        let mut done = 0;
        while done < out.len() {
            let pos = at + done as u64;
            let within = pos as usize % CHUNK_BYTES;
            let n = (CHUNK_BYTES - within).min(out.len() - done);
            if let Some(p) = s.pending.get(&(pos / CHUNK_BYTES as u64)) {
                out[done..done + n].copy_from_slice(&p.bytes[within..within + n]);
            } else {
                s.reader.read(pos, &mut out[done..done + n])?;
            }
            done += n;
        }
        Ok(())
    }
    fn write(&mut self, at: u64, input: &[u8]) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        s.check(at, input.len())?;
        if input.is_empty() {
            return Ok(());
        }
        let first = at / CHUNK_BYTES as u64;
        let last = (at + input.len() as u64 - 1) / CHUNK_BYTES as u64;
        let extra = (first..=last)
            .filter(|i| !s.pending.contains_key(i))
            .count();
        if (s.pending.len() + extra) * CHUNK_BYTES > DIRTY_LIMIT {
            return Err(Error::Backpressure);
        }
        let sequence = s.sequence.checked_add(1).ok_or(Error::Corrupt)?;
        let mut staged = BTreeMap::new();
        let mut done = 0;
        while done < input.len() {
            let pos = at + done as u64;
            let index = pos / CHUNK_BYTES as u64;
            let within = pos as usize % CHUNK_BYTES;
            let n = (CHUNK_BYTES - within).min(input.len() - done);
            let mut bytes = vec![0; CHUNK_BYTES];
            if within != 0 || n != CHUNK_BYTES {
                if let Some(p) = s.pending.get(&index) {
                    bytes.copy_from_slice(&p.bytes);
                } else {
                    s.reader.read(index * CHUNK_BYTES as u64, &mut bytes)?;
                }
            }
            bytes[within..within + n].copy_from_slice(&input[done..done + n]);
            staged.insert(
                index,
                Pending {
                    sequence,
                    bytes: Arc::new(bytes),
                },
            );
            done += n;
        }
        let result = (|| {
            if io(s.file.metadata())?.len() + staged.len() as u64 * RECORD as u64 > LOG_LIMIT {
                s.compact()?;
            }
            for (index, p) in &staged {
                record(&mut s.file, *index, p)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            s.failed = true;
            return Err(e);
        }
        s.sequence = sequence;
        s.pending.extend(staged);
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        s.check(0, 0)?;
        if let Err(e) = io(s.file.sync_all()) {
            s.failed = true;
            return Err(e);
        }
        s.local_sequence = s.sequence;
        Ok(())
    }
}
pub struct Background {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}
impl std::fmt::Debug for Background {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplicationWorker")
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod tests;
