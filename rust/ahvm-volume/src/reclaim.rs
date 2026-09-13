//! Reclamation of retired disks and exclusive offline collection for retained disks.
//! Keep the small tombstone forever: removing it would permit identity reuse.
use crate::{Error, Head, ObjectStore, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tombstone {
    format: u32,
    volume: String,
    deleted: bool,
}
pub(crate) fn tombstone(id: &str) -> Result<Vec<u8>> {
    serde_json::to_vec(&Tombstone {
        format: 5,
        volume: id.into(),
        deleted: true,
    })
    .map_err(|_| Error::Corrupt)
}
pub(crate) fn retired(id: &str, h: &Head) -> Result<bool> {
    if h.manifest.len() > crate::MAX_MANIFEST_BYTES || h.revision.is_empty() {
        return Err(Error::Corrupt);
    }
    let v: serde_json::Value = serde_json::from_slice(&h.manifest).map_err(|_| Error::Corrupt)?;
    if v["format"] != 5 {
        return Ok(false);
    }
    let t: Tombstone = serde_json::from_value(v).map_err(|_| Error::Corrupt)?;
    if t.volume != id || !t.deleted {
        return Err(Error::Corrupt);
    }
    Ok(true)
}
#[derive(Debug, Serialize)]
pub struct Progress {
    pub deleted_objects: usize,
    pub complete: bool,
}
/// A bounded, retryable pass. Always relist the first page after deletion; never
/// persist an offset into a shrinking listing. A lost delete reply is retryable.
/// Subsequent passes also sweep late orphan uploads from terminated writers.
pub fn sweep(store: &dyn ObjectStore, id: &str, limit: usize) -> Result<Progress> {
    if !crate::valid_id(id) || !(1..=128).contains(&limit) {
        return Err(Error::InvalidInput);
    }
    if !retired(id, &store.head(id)?.ok_or(Error::NotFound)?)? {
        return Err(Error::Conflict);
    }
    let hashes = store.list_chunks(id, limit)?;
    if hashes.len() > limit
        || hashes.iter().any(|h| {
            h.len() != 64
                || !h
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(Error::Corrupt);
    }
    for hash in &hashes {
        store.delete_chunk(id, hash)?;
    }
    // Completion requires an observed empty listing, not merely a short page.
    Ok(Progress {
        deleted_objects: hashes.len(),
        complete: hashes.is_empty(),
    })
}

/// An empty page completes a cycle. Resume by last key, never by list offset.
#[derive(Debug, Serialize)]
pub struct Collection {
    pub scanned_objects: usize,
    pub deleted_objects: usize,
    pub complete: bool,
    pub next_after: Option<String>,
}

#[cfg(test)]
pub(crate) mod tests;
