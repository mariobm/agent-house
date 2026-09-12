//! Experimental durable-volume protocol, not connected to the daemon or VMM.
//!
//! Writes are volatile until `commit` succeeds. The store must make immutable
//! chunks durable before atomically replacing the volume head. Opening a volume
//! reads that head; data is fetched and verified on demand. No local cache is
//! needed for recovery. This bounded model is not a production block device.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;

pub mod nbd;
pub mod s3;

pub const CHUNK_BYTES: usize = 64 * 1024;
/// Deliberately small for protocol qualification; not a product disk limit.
pub const MAX_VOLUME_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 128 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid volume ID, size or byte range")]
    InvalidInput,
    #[error("volume not found")]
    NotFound,
    #[error("object missing, oversized or corrupt")]
    Corrupt,
    #[error("head changed; reopen volume before writing")]
    Conflict,
    #[error("store operation failed")]
    Store,
    #[error("publication outcome unknown; reopen volume before writing")]
    Uncertain,
    #[error("this handle requires reopening")]
    ReopenRequired,
}
pub type Result<T> = std::result::Result<T, Error>;

/// Revision is opaque: an S3 adapter uses the exact response ETag, not a SHA.
#[derive(Debug, Clone)]
pub struct Head {
    pub revision: String,
    pub manifest: Vec<u8>,
}

/// All keys are scoped to a validated volume ID. No listing is needed to open.
///
/// Implementations must bound GET responses before buffering them, verify an
/// existing immutable object has identical bytes, and provide atomic conditional
/// head replacement. Successful writes mean durable at the store, not merely
/// queued locally. `None` means create only if absent, never unconditional PUT.
/// A lost publication response must return an error, never fabricated success.
pub trait ObjectStore: std::fmt::Debug + Send + Sync {
    fn head(&self, volume: &str) -> Result<Option<Head>>;
    fn chunk(&self, volume: &str, digest: &str) -> Result<Vec<u8>>;
    fn put_chunk(&self, volume: &str, digest: &str, bytes: &[u8]) -> Result<()>;
    fn publish(&self, volume: &str, expected: Option<&str>, manifest: &[u8]) -> Result<String>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    volume: String,
    size: u64,
    generation: u64,
    // Missing entries are zero-filled. Never substitute zeros for missing objects.
    chunks: BTreeMap<u64, String>,
}

#[derive(Debug)]
pub struct Volume {
    store: Arc<dyn ObjectStore>,
    manifest: Manifest,
    revision: String,
    dirty: BTreeMap<u64, Vec<u8>>,
    reopen: bool,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn validate(m: &Manifest, id: &str) -> Result<()> {
    if m.format != 1
        || m.volume != id
        || m.size == 0
        || m.size > MAX_VOLUME_BYTES
        || !m.size.is_multiple_of(CHUNK_BYTES as u64)
        || m.chunks.iter().any(|(index, hash)| {
            *index >= m.size / CHUNK_BYTES as u64
                || hash.len() != 64
                || !hash
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        })
    {
        return Err(Error::Corrupt);
    }
    Ok(())
}

impl Volume {
    /// Logical export size; the experiment has a fixed, bounded disk size.
    pub fn size(&self) -> u64 {
        self.manifest.size
    }

    /// Creates an empty, zero-filled disk. Existing heads are never overwritten.
    pub fn create(store: Arc<dyn ObjectStore>, id: &str, size: u64) -> Result<Self> {
        if !valid_id(id)
            || size == 0
            || size > MAX_VOLUME_BYTES
            || !size.is_multiple_of(CHUNK_BYTES as u64)
        {
            return Err(Error::InvalidInput);
        }
        let manifest = Manifest {
            format: 1,
            volume: id.into(),
            size,
            generation: 0,
            chunks: BTreeMap::new(),
        };
        let bytes = serde_json::to_vec(&manifest).map_err(|_| Error::Corrupt)?;
        let revision = store.publish(id, None, &bytes).map_err(publication_error)?;
        if revision.is_empty() {
            return Err(Error::Uncertain);
        }
        Ok(Self {
            store,
            manifest,
            revision,
            dirty: BTreeMap::new(),
            reopen: false,
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
        let manifest: Manifest =
            serde_json::from_slice(&head.manifest).map_err(|_| Error::Corrupt)?;
        validate(&manifest, id)?;
        Ok(Self {
            store,
            manifest,
            revision: head.revision,
            dirty: BTreeMap::new(),
            reopen: false,
        })
    }

    fn check_range(&self, offset: u64, len: usize) -> Result<()> {
        if self.reopen {
            return Err(Error::ReopenRequired);
        }
        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > self.manifest.size)
        {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }

    fn load(&self, index: u64) -> Result<Vec<u8>> {
        if let Some(bytes) = self.dirty.get(&index) {
            return Ok(bytes.clone());
        }
        let Some(hash) = self.manifest.chunks.get(&index) else {
            return Ok(vec![0; CHUNK_BYTES]);
        };
        let bytes = self.store.chunk(&self.manifest.volume, hash)?;
        if bytes.len() != CHUNK_BYTES || digest(&bytes) != *hash {
            return Err(Error::Corrupt);
        }
        Ok(bytes)
    }

    pub fn read(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        self.check_range(offset, output.len())?;
        let mut copied = 0;
        while copied < output.len() {
            let pos = offset + copied as u64;
            let within = pos as usize % CHUNK_BYTES;
            let count = (CHUNK_BYTES - within).min(output.len() - copied);
            let bytes = self.load(pos / CHUNK_BYTES as u64)?;
            output[copied..copied + count].copy_from_slice(&bytes[within..within + count]);
            copied += count;
        }
        Ok(())
    }

    /// Buffers a bounded write. This method does not promise remote durability.
    /// A failed multi-chunk read leaves all pre-existing pending writes unchanged.
    pub fn write(&mut self, offset: u64, input: &[u8]) -> Result<()> {
        self.check_range(offset, input.len())?;
        let mut pending = BTreeMap::new();
        let mut copied = 0;
        while copied < input.len() {
            let pos = offset + copied as u64;
            let index = pos / CHUNK_BYTES as u64;
            let within = pos as usize % CHUNK_BYTES;
            let count = (CHUNK_BYTES - within).min(input.len() - copied);
            let mut bytes = self.load(index)?;
            bytes[within..within + count].copy_from_slice(&input[copied..copied + count]);
            pending.insert(index, bytes);
            copied += count;
        }
        self.dirty.extend(pending);
        Ok(())
    }

    /// Publish data first, then the complete disk map with a conditional write.
    /// Errors during publication poison the handle: never rebase stale writes or
    /// retry an ambiguous publication with an unconditional overwrite.
    pub fn commit(&mut self) -> Result<u64> {
        if self.reopen {
            return Err(Error::ReopenRequired);
        }
        if self.dirty.is_empty() {
            return Ok(self.manifest.generation);
        }
        let mut next = self.manifest.clone();
        next.generation = next.generation.checked_add(1).ok_or(Error::Corrupt)?;
        for (&index, bytes) in &self.dirty {
            if bytes.iter().all(|b| *b == 0) {
                next.chunks.remove(&index);
            } else {
                let hash = digest(bytes);
                self.store.put_chunk(&next.volume, &hash, bytes)?;
                next.chunks.insert(index, hash);
            }
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| Error::Corrupt)?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Corrupt);
        }
        let revision = match self
            .store
            .publish(&next.volume, Some(&self.revision), &bytes)
        {
            Ok(revision) if !revision.is_empty() => revision,
            result => {
                self.reopen = true;
                return Err(publication_error(result.err().unwrap_or(Error::Uncertain)));
            }
        };
        self.manifest = next;
        self.revision = revision;
        self.dirty.clear();
        Ok(self.manifest.generation)
    }
}

fn publication_error(error: Error) -> Error {
    match error {
        Error::Conflict => Error::Conflict,
        _ => Error::Uncertain,
    }
}

#[cfg(test)]
mod tests;
