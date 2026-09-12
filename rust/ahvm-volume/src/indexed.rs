//! Format 2: bounded root -> immutable pages -> immutable 64-KiB data chunks.
//! Format 1 remains separate; there is no implicit format/mode migration.
use crate::{
    digest, publication_error, valid_id, Error, ObjectStore, Result, CHUNK_BYTES,
    MAX_MANIFEST_BYTES,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};
pub const MAX_SIZE: u64 = 64 * 1024 * 1024 * 1024;
pub const DIRTY_LIMIT: usize = 64 * 1024 * 1024;
pub const WRITE_LIMIT: usize = 32 * 1024 * 1024;
const SLOTS: u64 = 1024;
const MAGIC: &[u8; 8] = b"AHVMPG02";
const BUDGET: Duration = Duration::from_secs(30);
const WORKERS: usize = 8;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    format: u32,
    volume: String,
    size: u64,
    generation: u64,
    ready: bool,
    pages: BTreeMap<u64, String>,
}
pub struct IndexedVolume {
    store: Arc<dyn ObjectStore>,
    root: Root,
    revision: String,
    dirty: BTreeMap<u64, Arc<Vec<u8>>>,
    poisoned: bool,
}
impl std::fmt::Debug for IndexedVolume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexedVolume")
            .field("size", &self.root.size)
            .field("generation", &self.root.generation)
            .field("dirty_bytes", &self.dirty_bytes())
            .field("poisoned", &self.poisoned)
            .finish()
    }
}
fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn deadline(end: Instant) -> Result<()> {
    if Instant::now() >= end {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
fn decode(s: &str) -> Result<[u8; 32]> {
    if !valid_hash(s) {
        return Err(Error::Corrupt);
    }
    let mut hash = [0; 32];
    for (i, v) in hash.iter_mut().enumerate() {
        *v = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| Error::Corrupt)?;
    }
    Ok(hash)
}
fn slot(page: &[u8], index: u64) -> Option<String> {
    let at = 16 + (index % SLOTS) as usize * 32;
    let hash = &page[at..at + 32];
    if hash.iter().all(|b| *b == 0) {
        None
    } else {
        Some(hash.iter().map(|b| format!("{b:02x}")).collect())
    }
}
impl IndexedVolume {
    pub fn size(&self) -> u64 {
        self.root.size
    }
    pub fn dirty_bytes(&self) -> usize {
        self.dirty.len() * CHUNK_BYTES
    }
    pub fn create(store: Arc<dyn ObjectStore>, id: &str, size: u64) -> Result<Self> {
        Self::create_inner(store, id, size, true)
    }
    pub fn create_import(store: Arc<dyn ObjectStore>, id: &str, size: u64) -> Result<Self> {
        Self::create_inner(store, id, size, false)
    }
    fn create_inner(store: Arc<dyn ObjectStore>, id: &str, size: u64, ready: bool) -> Result<Self> {
        if !valid_id(id) || size == 0 || size > MAX_SIZE || !size.is_multiple_of(CHUNK_BYTES as u64)
        {
            return Err(Error::InvalidInput);
        }
        let root = Root {
            format: 2,
            volume: id.into(),
            size,
            generation: 0,
            ready,
            pages: BTreeMap::new(),
        };
        let bytes = serde_json::to_vec(&root).map_err(|_| Error::Corrupt)?;
        let revision = store.publish(id, None, &bytes).map_err(publication_error)?;
        if revision.is_empty() {
            return Err(Error::Uncertain);
        }
        Ok(Self {
            store,
            root,
            revision,
            dirty: BTreeMap::new(),
            poisoned: false,
        })
    }
    pub fn open(store: Arc<dyn ObjectStore>, id: &str) -> Result<Self> {
        if !valid_id(id) {
            return Err(Error::InvalidInput);
        }
        let head = store.head(id)?.ok_or(Error::NotFound)?;
        if head.manifest.len() > MAX_MANIFEST_BYTES || head.revision.is_empty() {
            return Err(Error::Corrupt);
        }
        let root: Root = serde_json::from_slice(&head.manifest).map_err(|_| Error::Corrupt)?;
        if root.format != 2
            || root.volume != id
            || root.size == 0
            || root.size > MAX_SIZE
            || !root.size.is_multiple_of(CHUNK_BYTES as u64)
            || root.pages.iter().any(|(p, h)| {
                *p >= (root.size / CHUNK_BYTES as u64).div_ceil(SLOTS) || !valid_hash(h)
            })
        {
            return Err(Error::Corrupt);
        }
        if !root.ready {
            return Err(Error::NotReady);
        }
        Ok(Self {
            store,
            root,
            revision: head.revision,
            dirty: BTreeMap::new(),
            poisoned: false,
        })
    }
    fn check(&self, at: u64, len: usize) -> Result<()> {
        if self.poisoned {
            return Err(Error::ReopenRequired);
        }
        if at
            .checked_add(len as u64)
            .is_none_or(|end| end > self.root.size)
        {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }
    fn page(&self, index: u64) -> Result<Vec<u8>> {
        let mut bytes = vec![0; CHUNK_BYTES];
        if let Some(hash) = self.root.pages.get(&index) {
            bytes = self.store.chunk(&self.root.volume, hash)?;
            if bytes.len() != CHUNK_BYTES
                || digest(&bytes) != *hash
                || &bytes[..8] != MAGIC
                || u64::from_be_bytes(bytes[8..16].try_into().unwrap()) != index
                || bytes[16 + SLOTS as usize * 32..].iter().any(|b| *b != 0)
            {
                return Err(Error::Corrupt);
            }
            // A last partial page must not claim data beyond the logical disk.
            let valid = (self.root.size / CHUNK_BYTES as u64 - index * SLOTS).min(SLOTS) as usize;
            if bytes[16 + valid * 32..16 + SLOTS as usize * 32]
                .iter()
                .any(|b| *b != 0)
            {
                return Err(Error::Corrupt);
            }
        } else {
            bytes[..8].copy_from_slice(MAGIC);
            bytes[8..16].copy_from_slice(&index.to_be_bytes());
        }
        Ok(bytes)
    }
    fn load(&self, index: u64, prefetch: bool) -> Result<Vec<u8>> {
        if let Some(b) = self.dirty.get(&index) {
            return Ok(b.as_ref().clone());
        }
        let page = self.page(index / SLOTS)?;
        let Some(hash) = slot(&page, index) else {
            return Ok(vec![0; CHUNK_BYTES]);
        };
        let bytes = if let Some(bytes) = self.store.cached_chunk(&self.root.volume, &hash) {
            bytes
        } else if prefetch {
            // Up to eight adjacent physical chunks, in parallel, only on a cache
            // miss. Speculative failures never replace the requested data/error.
            let end = (index + 8)
                .min((index / SLOTS + 1) * SLOTS)
                .min(self.size() / CHUNK_BYTES as u64);
            let hashes: std::collections::BTreeSet<_> =
                (index..end).filter_map(|i| slot(&page, i)).collect();
            std::thread::scope(|scope| -> Result<Vec<u8>> {
                let jobs: Vec<_> = hashes
                    .iter()
                    .map(|h| {
                        let requested = h == &hash;
                        scope.spawn(move || (requested, self.store.chunk(&self.root.volume, h)))
                    })
                    .collect();
                let mut result = Err(Error::Corrupt);
                for job in jobs {
                    let (requested, bytes) = job.join().map_err(|_| Error::Store)?;
                    if requested {
                        result = bytes;
                    }
                }
                result
            })?
        } else {
            self.store.chunk(&self.root.volume, &hash)?
        };
        if bytes.len() != CHUNK_BYTES || digest(&bytes) != hash {
            return Err(Error::Corrupt);
        }
        Ok(bytes)
    }
    pub fn read(&self, offset: u64, out: &mut [u8]) -> Result<()> {
        self.check(offset, out.len())?;
        if out.len() > WRITE_LIMIT {
            return Err(Error::InvalidInput);
        }
        let mut done = 0;
        while done < out.len() {
            let pos = offset + done as u64;
            let within = pos as usize % CHUNK_BYTES;
            let n = (CHUNK_BYTES - within).min(out.len() - done);
            let bytes = self.load(pos / CHUNK_BYTES as u64, true)?;
            out[done..done + n].copy_from_slice(&bytes[within..within + n]);
            done += n;
        }
        Ok(())
    }
    pub fn write(&mut self, offset: u64, input: &[u8]) -> Result<()> {
        self.check(offset, input.len())?;
        if input.len() > WRITE_LIMIT {
            return Err(Error::InvalidInput);
        }
        if input.is_empty() {
            return Ok(());
        }
        let first = offset / CHUNK_BYTES as u64;
        let last = (offset + input.len() as u64 - 1) / CHUNK_BYTES as u64;
        let extra = (first..=last)
            .filter(|i| !self.dirty.contains_key(i))
            .count();
        if self.dirty_bytes() + extra * CHUNK_BYTES > DIRTY_LIMIT {
            self.commit()?;
        }
        let mut pending = BTreeMap::new();
        let mut done = 0;
        while done < input.len() {
            let pos = offset + done as u64;
            let index = pos / CHUNK_BYTES as u64;
            let within = pos as usize % CHUNK_BYTES;
            let n = (CHUNK_BYTES - within).min(input.len() - done);
            let mut bytes = if within == 0 && n == CHUNK_BYTES {
                vec![0; CHUNK_BYTES]
            } else {
                self.load(index, false)?
            };
            bytes[within..within + n].copy_from_slice(&input[done..done + n]);
            pending.insert(index, Arc::new(bytes));
            done += n;
        }
        self.dirty.extend(pending);
        Ok(())
    }
    fn publish(&mut self, next: Root, end: Instant) -> Result<u64> {
        let bytes = serde_json::to_vec(&next).map_err(|_| Error::Corrupt)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Corrupt);
        }
        deadline(end)?;
        let revision = match self
            .store
            .publish(&next.volume, Some(&self.revision), &bytes)
        {
            Ok(v) if !v.is_empty() => v,
            result => {
                self.poisoned = true;
                return Err(publication_error(result.err().unwrap_or(Error::Uncertain)));
            }
        };
        self.root = next;
        self.revision = revision;
        self.dirty.clear();
        Ok(self.root.generation)
    }
    pub fn commit(&mut self) -> Result<u64> {
        self.commit_until(Instant::now() + BUDGET)
    }
    fn commit_until(&mut self, end: Instant) -> Result<u64> {
        self.check(0, 0)?;
        if self.dirty.is_empty() {
            return Ok(self.root.generation);
        }
        deadline(end)?;
        let mut next = self.root.clone();
        next.generation = next.generation.checked_add(1).ok_or(Error::Corrupt)?;
        let mut pages = BTreeMap::new();
        let mut uploads: BTreeMap<String, Arc<Vec<u8>>> = BTreeMap::new();
        for (&index, bytes) in &self.dirty {
            deadline(end)?;
            let page_index = index / SLOTS;
            if let std::collections::btree_map::Entry::Vacant(entry) = pages.entry(page_index) {
                entry.insert(self.page(page_index)?);
            }
            let page = pages.get_mut(&page_index).unwrap();
            let at = 16 + (index % SLOTS) as usize * 32;
            if bytes.iter().all(|b| *b == 0) {
                page[at..at + 32].fill(0);
            } else {
                let hash = digest(bytes);
                page[at..at + 32].copy_from_slice(&decode(&hash)?);
                uploads.entry(hash).or_insert_with(|| bytes.clone());
            }
        }
        // Upload data and page objects before the only visibility point, the root CAS.
        for (index, page) in pages {
            if page[16..].iter().all(|b| *b == 0) {
                next.pages.remove(&index);
            } else {
                let hash = digest(&page);
                next.pages.insert(index, hash.clone());
                uploads.insert(hash, Arc::new(page));
            }
        }
        let objects: Vec<_> = uploads.into_iter().collect();
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..WORKERS.min(objects.len()))
                .map(|worker| {
                    let objects = &objects;
                    let store = &self.store;
                    let id = &self.root.volume;
                    scope.spawn(move || -> Result<()> {
                        for i in (worker..objects.len()).step_by(WORKERS) {
                            deadline(end)?;
                            store.put_chunk(id, &objects[i].0, &objects[i].1)?;
                        }
                        Ok(())
                    })
                })
                .collect();
            let mut result = Ok(());
            for worker in workers {
                if let Err(e) = worker.join().unwrap_or(Err(Error::Store)) {
                    result = Err(e);
                }
            }
            result
        })?;
        self.publish(next, end)
    }
    pub fn finish_import(&mut self) -> Result<u64> {
        self.commit()?;
        self.check(0, 0)?;
        let mut next = self.root.clone();
        next.ready = true;
        next.generation = next.generation.checked_add(1).ok_or(Error::Corrupt)?;
        self.publish(next, Instant::now() + BUDGET)
    }
}

#[cfg(test)]
pub(crate) mod tests;
