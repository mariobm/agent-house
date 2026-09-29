//! Bounded roots, immutable index pages, and immutable data objects.
//! Formats 8–11 pack private 64-KiB blocks into objects of at most 1 MiB.
//! Format 1 remains separate; there is no implicit format/mode migration.
use crate::{
    base::{BaseRef, Catalog},
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
const PACK_MAGIC: &[u8; 8] = b"AHVMPG08";
const PACK_PAGE_BYTES: usize = 128 * 1024;
const PACK_SLOT_BYTES: usize = 72;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replication: Option<Replication>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base: Option<BaseRef>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Replication {
    pub id: String,
    pub sequence: u64,
}
pub struct IndexedVolume {
    store: Arc<dyn ObjectStore>,
    root: Root,
    base: Option<Catalog>,
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
pub(crate) fn decode(s: &str) -> Result<[u8; 32]> {
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
    let at = slot_at(page, index);
    let hash = &page[at..at + 32];
    if hash.iter().all(|b| *b == 0) {
        None
    } else {
        Some(hash.iter().map(|b| format!("{b:02x}")).collect())
    }
}
fn stride(page: &[u8]) -> usize {
    if &page[..8] == PACK_MAGIC {
        PACK_SLOT_BYTES
    } else {
        32
    }
}
fn slot_at(page: &[u8], index: u64) -> usize {
    16 + (index % SLOTS) as usize * stride(page)
}
fn object_slot(page: &[u8], index: u64) -> Option<(String, usize)> {
    let chunk = slot(page, index)?;
    let at = slot_at(page, index);
    if stride(page) == PACK_SLOT_BYTES && page[at + 32..at + 64].iter().any(|b| *b != 0) {
        let pack = page[at + 32..at + 64]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let offset = u32::from_be_bytes(page[at + 64..at + 68].try_into().unwrap()) as usize;
        Some((pack, offset))
    } else {
        Some((chunk, 0))
    }
}
fn packed_page(page: &[u8]) -> Vec<u8> {
    if stride(page) == PACK_SLOT_BYTES {
        return page.to_vec();
    }
    let mut out = vec![0; PACK_PAGE_BYTES];
    out[..8].copy_from_slice(PACK_MAGIC);
    out[8..16].copy_from_slice(&page[8..16]);
    for index in 0..SLOTS {
        let old = slot_at(page, index);
        let new = slot_at(&out, index);
        out[new..new + 32].copy_from_slice(&page[old..old + 32]);
    }
    out
}
impl IndexedVolume {
    /// Pin a verified completed image catalog. The supplied store must address
    /// the base namespace; this never publishes a mutable reference into a VM.
    pub fn export_base(&self) -> Result<BaseRef> {
        if !self.root.ready
            || (self.packed() && !self.root.pages.is_empty())
            || self.root.base.is_some()
            || self.root.replication.is_some()
            || !self.dirty.is_empty()
            || self.poisoned
        {
            return Err(Error::InvalidInput);
        }
        decode(&self.root.volume)?;
        let catalog = Catalog {
            size: self.root.size,
            pages: self.root.pages.clone(),
        };
        let bytes = catalog.encode(&self.root.volume)?;
        let hash = digest(&bytes);
        self.store.put_chunk(&self.root.volume, &hash, &bytes)?;
        Ok(BaseRef {
            image: self.root.volume.clone(),
            catalog: hash,
        })
    }
    /// New VM contains a private map and writes; the immutable image is shared.
    pub fn create_from_base(
        store: Arc<dyn ObjectStore>,
        id: &str,
        reference: BaseRef,
    ) -> Result<Self> {
        if !valid_id(id) {
            return Err(Error::InvalidInput);
        }
        let base = Catalog::load(store.as_ref(), &reference)?;
        let root = Root {
            format: 10,
            volume: id.into(),
            size: base.size,
            generation: 0,
            ready: true,
            pages: base.pages.clone(),
            replication: None,
            base: Some(reference),
        };
        let bytes = serde_json::to_vec(&root).map_err(|_| Error::Corrupt)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Corrupt);
        }
        let revision = store.publish(id, None, &bytes).map_err(publication_error)?;
        if revision.is_empty() {
            return Err(Error::Uncertain);
        }
        Ok(Self {
            store,
            root,
            base: Some(base),
            revision,
            dirty: BTreeMap::new(),
            poisoned: false,
        })
    }
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
            format: if ready { 8 } else { 2 },
            volume: id.into(),
            size,
            generation: 0,
            ready,
            pages: BTreeMap::new(),
            replication: None,
            base: None,
        };
        let bytes = serde_json::to_vec(&root).map_err(|_| Error::Corrupt)?;
        let revision = store.publish(id, None, &bytes).map_err(publication_error)?;
        if revision.is_empty() {
            return Err(Error::Uncertain);
        }
        Ok(Self {
            store,
            root,
            base: None,
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
        Self::from_head(store, id, head)
    }
    pub(crate) fn from_head(
        store: Arc<dyn ObjectStore>,
        id: &str,
        head: crate::Head,
    ) -> Result<Self> {
        Self::decode_head(store, id, head, true)
    }
    fn decode_head(
        store: Arc<dyn ObjectStore>,
        id: &str,
        head: crate::Head,
        ready: bool,
    ) -> Result<Self> {
        if head.manifest.len() > MAX_MANIFEST_BYTES || head.revision.is_empty() {
            return Err(Error::Corrupt);
        }
        let root: Root = serde_json::from_slice(&head.manifest).map_err(|_| Error::Corrupt)?;
        if !matches!(root.format, 2 | 3 | 6 | 7 | 8 | 9 | 10 | 11)
            || matches!(root.format, 3 | 7 | 9 | 11) != root.replication.is_some()
            || matches!(root.format, 6 | 7 | 10 | 11) != root.base.is_some()
            || root
                .replication
                .as_ref()
                .is_some_and(|r| !valid_hash(&r.id) || r.sequence == 0)
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
        if ready && !root.ready {
            return Err(Error::NotReady);
        }
        let base = root
            .base
            .as_ref()
            .map(|b| Catalog::load(store.as_ref(), b))
            .transpose()?;
        if base.as_ref().is_some_and(|b| b.size != root.size) {
            return Err(Error::Corrupt);
        }
        Ok(Self {
            store,
            root,
            base,
            revision: head.revision,
            dirty: BTreeMap::new(),
            poisoned: false,
        })
    }
    /// Resume ONLY an unfinished, unowned import. Caller must bind and verify
    /// the source image and rewrite every byte before finish_import.
    pub fn resume_import(store: Arc<dyn ObjectStore>, id: &str, size: u64) -> Result<Self> {
        if !valid_id(id) {
            return Err(Error::InvalidInput);
        }
        let head = store.head(id)?.ok_or(Error::NotFound)?;
        let disk = Self::decode_head(store, id, head, false)?;
        if disk.root.ready || disk.root.format != 2 || disk.size() != size {
            return Err(Error::Conflict);
        }
        Ok(disk)
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
    fn packed(&self) -> bool {
        matches!(self.root.format, 8..=11)
    }
    fn page_from(&self, index: u64, hash: Option<&String>, base: bool) -> Result<Vec<u8>> {
        let packed = self.packed() && !base;
        let page_bytes = if packed { PACK_PAGE_BYTES } else { CHUNK_BYTES };
        let width = if packed { PACK_SLOT_BYTES } else { 32 };
        let magic = if packed { PACK_MAGIC } else { MAGIC };
        let mut bytes = vec![0; page_bytes];
        if let Some(hash) = hash {
            bytes = if base {
                self.store.base_chunk(
                    &self.root.base.as_ref().ok_or(Error::Corrupt)?.image,
                    hash,
                    None,
                )?
            } else {
                self.store.chunk(&self.root.volume, hash)?
            };
            if bytes.len() != page_bytes
                || digest(&bytes) != *hash
                || &bytes[..8] != magic
                || u64::from_be_bytes(bytes[8..16].try_into().unwrap()) != index
                || bytes[16 + SLOTS as usize * width..].iter().any(|b| *b != 0)
            {
                return Err(Error::Corrupt);
            }
            let valid = (self.root.size / CHUNK_BYTES as u64 - index * SLOTS).min(SLOTS) as usize;
            if bytes[16 + valid * width..16 + SLOTS as usize * width]
                .iter()
                .any(|b| *b != 0)
            {
                return Err(Error::Corrupt);
            }
            if packed {
                let mut locations = BTreeMap::new();
                for i in 0..SLOTS {
                    let at = slot_at(&bytes, i);
                    let empty = bytes[at..at + 32].iter().all(|b| *b == 0);
                    let direct = bytes[at + 32..at + 64].iter().all(|b| *b == 0);
                    let offset =
                        u32::from_be_bytes(bytes[at + 64..at + 68].try_into().unwrap()) as usize;
                    if bytes[at + 68..at + 72].iter().any(|b| *b != 0)
                        || (empty && bytes[at + 32..at + 68].iter().any(|b| *b != 0))
                        || (direct && offset != 0)
                        || (!direct
                            && (!offset.is_multiple_of(CHUNK_BYTES)
                                || offset >= crate::MAX_OBJECT_BYTES))
                    {
                        return Err(Error::Corrupt);
                    }
                    if !empty && !direct {
                        let key = (bytes[at + 32..at + 64].to_vec(), offset);
                        if let Some(previous) = locations.insert(key, bytes[at..at + 32].to_vec()) {
                            if previous != bytes[at..at + 32] {
                                return Err(Error::Corrupt);
                            }
                        }
                    }
                }
            }
        } else {
            bytes[..8].copy_from_slice(magic);
            bytes[8..16].copy_from_slice(&index.to_be_bytes());
        }
        if self.packed() && base {
            bytes = packed_page(&bytes);
        }
        Ok(bytes)
    }
    fn base_page(&self, index: u64) -> Result<Vec<u8>> {
        self.page_from(
            index,
            self.base.as_ref().and_then(|b| b.pages.get(&index)),
            true,
        )
    }
    fn page(&self, index: u64) -> Result<Vec<u8>> {
        let hash = self.root.pages.get(&index);
        let inherited = hash.is_some()
            && self
                .base
                .as_ref()
                .is_some_and(|b| b.pages.get(&index) == hash);
        self.page_from(index, hash, inherited)
    }
    /// Full validated reference graph for offline collection. Includes metadata
    /// pages as well as data hashes; never fetches all data blocks. A missing or
    /// corrupt page aborts marking before any deletion can begin.
    pub(crate) fn references(&self, cancel: &impl Fn() -> bool) -> Result<Vec<[u8; 32]>> {
        let mut live = Vec::with_capacity(self.root.pages.len() * (SLOTS as usize + 1));
        for (index, hash) in &self.root.pages {
            if cancel() {
                return Err(Error::Deadline);
            }
            // Immutable base pages/data are retained separately, never swept
            // under the VM namespace. No full-image mark scan on every stop.
            if self
                .base
                .as_ref()
                .is_some_and(|b| b.pages.get(index) == Some(hash))
            {
                continue;
            }
            let page = self.page(*index)?;
            let base_page = self.base_page(*index)?;
            live.push(decode(hash)?);
            for index in 0..SLOTS {
                if let Some((object, _)) = object_slot(&page, index) {
                    let inherited = slot(&page, index) == slot(&base_page, index)
                        && object_slot(&page, index) == object_slot(&base_page, index);
                    if !inherited {
                        live.push(decode(&object)?);
                    }
                }
            }
        }
        if cancel() {
            return Err(Error::Deadline);
        }
        live.sort_unstable();
        live.dedup();
        Ok(live)
    }
    fn load(&self, index: u64, prefetch: bool) -> Result<Vec<u8>> {
        if let Some(b) = self.dirty.get(&index) {
            return Ok(b.as_ref().clone());
        }
        let page = self.page(index / SLOTS)?;
        let Some(hash) = slot(&page, index) else {
            return Ok(vec![0; CHUNK_BYTES]);
        };
        let base_page = self
            .base
            .as_ref()
            .map(|_| self.base_page(index / SLOTS))
            .transpose()?;
        let inherited = base_page.as_ref().is_some_and(|p| {
            slot(p, index).as_ref() == Some(&hash)
                && object_slot(p, index) == object_slot(&page, index)
        });
        let (object, object_offset) = object_slot(&page, index).ok_or(Error::Corrupt)?;
        let cached = if inherited {
            None
        } else {
            self.store.cached_chunk(&self.root.volume, &object)
        };
        if prefetch && cached.is_none() {
            let end = (index + 8)
                .min((index / SLOTS + 1) * SLOTS)
                .min(self.size() / CHUNK_BYTES as u64);
            let mut private = Vec::new();
            let mut shared = Vec::new();
            for i in index + 1..end {
                if let Some(next) = slot(&page, i).filter(|h| h != &hash) {
                    if base_page
                        .as_ref()
                        .is_some_and(|p| slot(p, i).as_ref() == Some(&next))
                    {
                        shared.push(next);
                    } else {
                        if let Some((object, _)) = object_slot(&page, i) {
                            if object != object_slot(&page, index).ok_or(Error::Corrupt)?.0
                                && !private.contains(&object)
                            {
                                private.push(object);
                            }
                        }
                    }
                }
            }
            // One shared seven-request admission budget in the cache, including
            // mixed pages with both base blocks and private filesystem metadata.
            self.store.prefetch(&self.root.volume, &private);
            if let Some(base) = &self.root.base {
                self.store.prefetch_base(&base.image, &shared);
            }
        }
        let bytes = if inherited {
            self.store.base_chunk(
                &self.root.base.as_ref().unwrap().image,
                &hash,
                Some(index * CHUNK_BYTES as u64),
            )?
        } else if let Some(bytes) = cached {
            bytes
        } else {
            self.store.chunk(&self.root.volume, &object)?
        };
        let bytes = if !inherited && object != hash {
            if bytes.len() > crate::MAX_OBJECT_BYTES
                || !bytes.len().is_multiple_of(CHUNK_BYTES)
                || digest(&bytes) != object
                || object_offset
                    .checked_add(CHUNK_BYTES)
                    .is_none_or(|end| end > bytes.len())
            {
                return Err(Error::Corrupt);
            }
            bytes[object_offset..object_offset + CHUNK_BYTES].to_vec()
        } else {
            bytes
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
    pub(crate) fn clean_copy(&self) -> Result<Self> {
        self.check(0, 0)?;
        if !self.dirty.is_empty() {
            return Err(Error::InvalidInput);
        }
        Ok(Self {
            store: self.store.clone(),
            root: self.root.clone(),
            base: self.base.clone(),
            revision: self.revision.clone(),
            dirty: BTreeMap::new(),
            poisoned: false,
        })
    }
    pub(crate) fn revision(&self) -> &str {
        &self.revision
    }
    pub(crate) fn replication(&self) -> Option<&Replication> {
        self.root.replication.as_ref()
    }
    pub(crate) fn commit_replication(&mut self, mark: Replication) -> Result<u64> {
        if !valid_hash(&mark.id) || mark.sequence == 0 {
            return Err(Error::InvalidInput);
        }
        // Remote replication is off the guest's fsync path. Allow a full bounded
        // backlog more time than the strict synchronous experiment.
        self.commit_inner(
            Instant::now() + Duration::from_secs(120),
            Some(mark),
            WORKERS,
        )
    }
    pub fn commit(&mut self) -> Result<u64> {
        self.commit_until(Instant::now() + BUDGET)
    }
    fn commit_until(&mut self, end: Instant) -> Result<u64> {
        if self.root.replication.is_some() {
            return Err(Error::InvalidInput);
        }
        self.commit_inner(end, None, WORKERS)
    }
    /// Initial image import has no guest latency to protect. Use bounded extra
    /// parallelism for its many immutable objects; ordinary replication stays at 8.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn commit_import(&mut self) -> Result<u64> {
        if self.root.ready || self.root.replication.is_some() {
            return Err(Error::InvalidInput);
        }
        self.commit_inner(Instant::now() + Duration::from_secs(120), None, 32)
    }
    fn commit_inner(
        &mut self,
        end: Instant,
        mark: Option<Replication>,
        concurrency: usize,
    ) -> Result<u64> {
        self.check(0, 0)?;
        if self.dirty.is_empty() && mark.is_none() {
            return Ok(self.root.generation);
        }
        deadline(end)?;
        let mut next = self.root.clone();
        if let Some(mark) = mark {
            next.format = if self.packed() {
                if next.base.is_some() {
                    11
                } else {
                    9
                }
            } else if next.base.is_some() {
                7
            } else {
                3
            };
            next.replication = Some(mark);
        }
        next.generation = next.generation.checked_add(1).ok_or(Error::Corrupt)?;
        if self.packed() {
            return self.commit_packed(next, end, concurrency, false);
        }
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
                let raw = decode(&hash)?;
                // The committed page already references this immutable content.
                // Rewriting identical bytes needs neither another PUT nor a GET
                // to resolve a duplicate-object conditional failure.
                if page[at..at + 32] != raw {
                    page[at..at + 32].copy_from_slice(&raw);
                    let inherited = self.base.is_some()
                        && slot(&self.base_page(page_index)?, index).as_ref() == Some(&hash);
                    if !inherited {
                        uploads.entry(hash).or_insert_with(|| bytes.clone());
                    }
                }
            }
        }
        // Upload data and page objects before the only visibility point, the root CAS.
        for (index, page) in pages {
            if page[16..].iter().all(|b| *b == 0) {
                next.pages.remove(&index);
            } else {
                let hash = digest(&page);
                if self.root.pages.get(&index) != Some(&hash) {
                    next.pages.insert(index, hash.clone());
                    if !self
                        .base
                        .as_ref()
                        .is_some_and(|b| b.pages.get(&index) == Some(&hash))
                    {
                        uploads.insert(hash, Arc::new(page));
                    }
                }
            }
        }
        if next.pages == self.root.pages && next.replication == self.root.replication {
            // Same semantics as a clean flush: no new remote state to publish.
            self.dirty.clear();
            return Ok(self.root.generation);
        }
        let objects: Vec<_> = uploads.into_iter().collect();
        std::thread::scope(|scope| -> Result<()> {
            let workers: Vec<_> = (0..concurrency.min(objects.len()))
                .map(|worker| {
                    let objects = &objects;
                    let store = &self.store;
                    let id = &self.root.volume;
                    scope.spawn(move || -> Result<()> {
                        for i in (worker..objects.len()).step_by(concurrency) {
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
    fn upload_objects(
        &self,
        objects: &[(String, Arc<Vec<u8>>)],
        end: Instant,
        concurrency: usize,
    ) -> Result<()> {
        std::thread::scope(|scope| {
            let workers: Vec<_> = (0..concurrency.min(objects.len()))
                .map(|worker| {
                    scope.spawn(move || {
                        for i in (worker..objects.len()).step_by(concurrency) {
                            deadline(end)?;
                            self.store.put_chunk(
                                &self.root.volume,
                                &objects[i].0,
                                &objects[i].1,
                            )?;
                        }
                        Ok(())
                    })
                })
                .collect();
            let mut result = Ok(());
            for worker in workers {
                if let Err(error) = worker.join().unwrap_or(Err(Error::Store)) {
                    result = Err(error);
                }
            }
            result
        })
    }
    fn commit_packed(
        &mut self,
        mut next: Root,
        end: Instant,
        concurrency: usize,
        force: bool,
    ) -> Result<u64> {
        let mut pages = BTreeMap::new();
        let mut chunks: BTreeMap<String, Arc<Vec<u8>>> = BTreeMap::new();
        let mut changed = Vec::new();
        let mut base_pages = BTreeMap::new();
        for (&index, bytes) in &self.dirty {
            deadline(end)?;
            let page_index = index / SLOTS;
            if let std::collections::btree_map::Entry::Vacant(entry) = pages.entry(page_index) {
                entry.insert(self.page(page_index)?);
                base_pages.insert(page_index, self.base_page(page_index)?);
            }
            let page = pages.get_mut(&page_index).unwrap();
            let at = slot_at(page, index);
            if bytes.iter().all(|b| *b == 0) {
                page[at..at + PACK_SLOT_BYTES].fill(0);
                continue;
            }
            let hash = digest(bytes);
            if !force && slot(page, index).as_ref() == Some(&hash) {
                continue;
            }
            page[at..at + PACK_SLOT_BYTES].fill(0);
            page[at..at + 32].copy_from_slice(&decode(&hash)?);
            if slot(&base_pages[&page_index], index).as_ref() != Some(&hash) {
                chunks.entry(hash.clone()).or_insert_with(|| bytes.clone());
                changed.push((index, hash));
            }
        }
        let mut uploads = Vec::new();
        let mut locations = BTreeMap::new();
        let unique: Vec<_> = chunks.into_iter().collect();
        for group in unique.chunks(crate::MAX_OBJECT_BYTES / CHUNK_BYTES) {
            deadline(end)?;
            let mut bytes = Vec::with_capacity(group.len() * CHUNK_BYTES);
            for (_, chunk) in group {
                bytes.extend_from_slice(chunk);
            }
            let hash = digest(&bytes);
            for (offset, (chunk, _)) in group.iter().enumerate() {
                locations.insert(chunk.clone(), (hash.clone(), offset * CHUNK_BYTES));
            }
            uploads.push((hash, Arc::new(bytes)));
        }
        for (index, hash) in changed {
            let page = pages.get_mut(&(index / SLOTS)).unwrap();
            let at = slot_at(page, index);
            let (pack, offset) = &locations[&hash];
            page[at + 32..at + 64].copy_from_slice(&decode(pack)?);
            page[at + 64..at + 68].copy_from_slice(&(*offset as u32).to_be_bytes());
        }
        for (index, page) in pages {
            if page[16..].iter().all(|b| *b == 0) {
                next.pages.remove(&index);
            } else if self.base.is_some() && page == base_pages[&index] {
                next.pages
                    .insert(index, self.base.as_ref().unwrap().pages[&index].clone());
            } else {
                let hash = digest(&page);
                if self.root.pages.get(&index) != Some(&hash) {
                    next.pages.insert(index, hash.clone());
                    uploads.push((hash, Arc::new(page)));
                }
            }
        }
        if next.pages == self.root.pages && next.replication == self.root.replication {
            self.dirty.clear();
            return Ok(self.root.generation);
        }
        self.upload_objects(&uploads, end, concurrency)?;
        self.publish(next, end)
    }
    /// Repack one index page under exclusive offline ownership. The budget caps
    /// source pack bytes plus rewritten logical bytes (metadata is one 128-KiB
    /// page). Packs referenced by other pages remain reachable until those pages
    /// are compacted too. No history/checkpoint formats are accepted by decoding.
    pub(crate) fn compact_packs(
        &mut self,
        after: Option<crate::reclaim::CompactionCursor>,
        budget: usize,
        cancel: &impl Fn() -> bool,
    ) -> Result<crate::reclaim::Compaction> {
        self.check(0, 0)?;
        if after.as_ref().is_some_and(|cursor| {
            cursor.page >= (self.size() / CHUNK_BYTES as u64).div_ceil(SLOTS)
                || cursor
                    .after_pack
                    .as_deref()
                    .is_some_and(|hash| !valid_hash(hash))
        }) {
            return Err(Error::InvalidInput);
        }
        if !self.dirty.is_empty()
            || !(2 * crate::MAX_OBJECT_BYTES..=8 * crate::MAX_OBJECT_BYTES).contains(&budget)
        {
            return Err(Error::InvalidInput);
        }
        if !self.packed() {
            return Ok(crate::reclaim::Compaction {
                rewritten_blocks: 0,
                next_page: None,
            });
        }
        let Some((&page_index, _)) = self
            .root
            .pages
            .iter()
            .find(|(index, _)| after.as_ref().is_none_or(|cursor| **index >= cursor.page))
        else {
            return Ok(crate::reclaim::Compaction {
                rewritten_blocks: 0,
                next_page: None,
            });
        };
        if cancel() {
            return Err(Error::Deadline);
        }
        let page = self.page(page_index)?;
        let mut groups: BTreeMap<String, Vec<(u64, usize, String)>> = BTreeMap::new();
        for within in 0..SLOTS {
            let index = page_index * SLOTS + within;
            if let Some((pack, offset)) = object_slot(&page, index) {
                let chunk = slot(&page, index).ok_or(Error::Corrupt)?;
                if pack != chunk {
                    groups.entry(pack).or_default().push((index, offset, chunk));
                }
            }
        }
        let mut consumed = 0;
        let resume = after
            .as_ref()
            .filter(|cursor| cursor.page == page_index)
            .and_then(|cursor| cursor.after_pack.as_deref());
        let mut completed_pack = resume.map(str::to_owned);
        let mut next_cursor = None;
        for (pack, entries) in groups {
            if resume.is_some_and(|hash| pack.as_str() <= hash) {
                continue;
            }

            if cancel() {
                return Err(Error::Deadline);
            }
            // Reserve worst-case input before reading; never exceed the bound.
            if consumed + crate::MAX_OBJECT_BYTES + CHUNK_BYTES > budget {
                next_cursor = Some(crate::reclaim::CompactionCursor {
                    page: page_index,
                    after_pack: completed_pack.clone(),
                });
                break;
            }
            let bytes = self.store.chunk(&self.root.volume, &pack)?;
            if bytes.len() > crate::MAX_OBJECT_BYTES
                || !bytes.len().is_multiple_of(CHUNK_BYTES)
                || digest(&bytes) != pack
            {
                return Err(Error::Corrupt);
            }
            consumed += bytes.len();
            if bytes.len() < CHUNK_BYTES {
                return Err(Error::Corrupt);
            }
            let mut verified = std::collections::BTreeSet::new();
            for (_, offset, hash) in &entries {
                if cancel() {
                    return Err(Error::Deadline);
                }
                let chunk = bytes
                    .get(*offset..offset.checked_add(CHUNK_BYTES).ok_or(Error::Corrupt)?)
                    .ok_or(Error::Corrupt)?;
                if verified.insert((*offset, hash.clone())) && digest(chunk) != *hash {
                    return Err(Error::Corrupt);
                }
            }
            let offsets: std::collections::BTreeSet<_> =
                entries.iter().map(|(_, offset, _)| *offset).collect();
            if offsets.len() * CHUNK_BYTES >= bytes.len() {
                completed_pack = Some(pack);
                continue;
            }
            let available = (budget - consumed) / CHUNK_BYTES;
            let partial = entries.len() > available;
            for (index, offset, hash) in entries.into_iter().take(available) {
                let chunk = bytes
                    .get(offset..offset + CHUNK_BYTES)
                    .ok_or(Error::Corrupt)?;
                if digest(chunk) != hash {
                    return Err(Error::Corrupt);
                }
                self.dirty.insert(index, Arc::new(chunk.to_vec()));
                consumed += CHUNK_BYTES;
            }
            if partial {
                // Resume before this same pack after the CAS; the rewritten
                // aliases are no longer in its group, so each pass progresses.
                next_cursor = Some(crate::reclaim::CompactionCursor {
                    page: page_index,
                    after_pack: completed_pack.clone(),
                });
                break;
            }
            completed_pack = Some(pack);
        }
        let count = self.dirty.len();
        if count != 0 {
            if cancel() {
                self.dirty.clear();
                return Err(Error::Deadline);
            }
            let mut next = self.root.clone();
            next.generation = next.generation.checked_add(1).ok_or(Error::Corrupt)?;
            self.commit_packed(next, Instant::now() + BUDGET, WORKERS, true)?;
        }
        Ok(crate::reclaim::Compaction {
            rewritten_blocks: count,
            next_page: next_cursor.or_else(|| {
                self.root
                    .pages
                    .keys()
                    .find(|index| **index > page_index)
                    .map(|page| crate::reclaim::CompactionCursor {
                        page: *page,
                        after_pack: None,
                    })
            }),
        })
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
