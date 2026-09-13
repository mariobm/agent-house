//! Immutable shared image catalog. VM roots pin its content hash, never a mutable
//! image head. Base objects are outside per-VM garbage collection.
use crate::{
    digest,
    indexed::{decode, MAX_SIZE},
    Error, ObjectStore, Result, CHUNK_BYTES,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
const MAGIC: &[u8; 8] = b"AHVMBS01";
const HEADER: usize = 48;
const PAGES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseRef {
    pub image: String,
    pub catalog: String,
}
#[derive(Debug, Clone)]
pub(crate) struct Catalog {
    pub size: u64,
    pub pages: BTreeMap<u64, String>,
}
impl Catalog {
    pub fn encode(&self, image: &str) -> Result<Vec<u8>> {
        let mut bytes = vec![0; CHUNK_BYTES];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..16].copy_from_slice(&self.size.to_be_bytes());
        bytes[16..48].copy_from_slice(&decode(image)?);
        for (&page, hash) in &self.pages {
            if page >= PAGES as u64 {
                return Err(Error::InvalidInput);
            }
            let at = HEADER + page as usize * 32;
            bytes[at..at + 32].copy_from_slice(&decode(hash)?);
        }
        Ok(bytes)
    }
    pub fn load(store: &dyn ObjectStore, base: &BaseRef) -> Result<Self> {
        decode(&base.image)?;
        decode(&base.catalog)?;
        let bytes = store.base_chunk(&base.image, &base.catalog, None)?;
        if bytes.len() != CHUNK_BYTES
            || digest(&bytes) != base.catalog
            || &bytes[..8] != MAGIC
            || bytes[16..48] != decode(&base.image)?
            || bytes[HEADER + PAGES * 32..].iter().any(|b| *b != 0)
        {
            return Err(Error::Corrupt);
        }
        let size = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        if size == 0 || size > MAX_SIZE || !size.is_multiple_of(CHUNK_BYTES as u64) {
            return Err(Error::Corrupt);
        }
        let count = (size / CHUNK_BYTES as u64).div_ceil(1024);
        let mut pages = BTreeMap::new();
        for i in 0..PAGES {
            let hash = &bytes[HEADER + i * 32..HEADER + (i + 1) * 32];
            if hash.iter().any(|b| *b != 0) {
                if i as u64 >= count {
                    return Err(Error::Corrupt);
                }
                pages.insert(i as u64, hash.iter().map(|b| format!("{b:02x}")).collect());
            }
        }
        Ok(Self { size, pages })
    }
}
