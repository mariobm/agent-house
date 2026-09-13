//! Durable reservations, not remote-object usage or an RSS/filesystem quota.
use super::Result;
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

pub(super) const CACHE_BYTES: u64 = 64 * 1024 * 1024;
// Compaction can hold both the original log and its replacement simultaneously.
pub(super) const JOURNAL_BYTES: u64 = 2 * crate::local::LOG_LIMIT;
const MAX_DISK: u64 = 64 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Limits {
    pub max_volume_bytes: u64,
    pub max_logical_bytes: u64,
    pub max_journal_bytes: u64,
    pub max_cache_bytes: u64,
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        valid_size(self.max_volume_bytes)?;
        if self.max_logical_bytes < self.max_volume_bytes
            || self.max_journal_bytes < JOURNAL_BYTES
            || self.max_cache_bytes < CACHE_BYTES
        {
            return Err("storage budgets must accommodate at least one volume".into());
        }
        Ok(())
    }
    pub fn admit(&self, used: &Usage, size: u64) -> Result<()> {
        if size > self.max_volume_bytes {
            return Err("per-volume logical capacity limit".into());
        }
        let mut next = used.clone();
        next.add(size)?;
        if next.logical_bytes > self.max_logical_bytes {
            return Err("host logical capacity reservation exhausted".into());
        }
        if next.journal_reserved_bytes > self.max_journal_bytes {
            return Err("host journal reservation exhausted".into());
        }
        if next.cache_reserved_bytes > self.max_cache_bytes {
            return Err("host cache reservation exhausted".into());
        }
        Ok(())
    }
}
fn valid_size(size: u64) -> Result<()> {
    if size == 0 || size > MAX_DISK || !size.is_multiple_of(crate::CHUNK_BYTES as u64) {
        return Err("invalid reserved disk size".into());
    }
    Ok(())
}
#[derive(Debug, Default, Clone, Serialize)]
pub(super) struct Usage {
    pub retained_volumes: u64,
    pub logical_bytes: u64,
    pub journal_reserved_bytes: u64,
    pub cache_reserved_bytes: u64,
}
impl Usage {
    pub fn add(&mut self, size: u64) -> Result<()> {
        self.add_residency(size, true)
    }
    pub fn add_residency(&mut self, size: u64, resident: bool) -> Result<()> {
        valid_size(size)?;
        fn add(a: u64, b: u64) -> Result<u64> {
            a.checked_add(b)
                .ok_or_else(|| "storage accounting overflow".into())
        }
        let next = Self {
            retained_volumes: add(self.retained_volumes, 1)?,
            logical_bytes: add(self.logical_bytes, size)?,
            journal_reserved_bytes: add(
                self.journal_reserved_bytes,
                if resident { JOURNAL_BYTES } else { 0 },
            )?,
            cache_reserved_bytes: add(
                self.cache_reserved_bytes,
                if resident { CACHE_BYTES } else { 0 },
            )?,
        };
        *self = next;
        Ok(())
    }
}
#[derive(Debug, Serialize)]
pub(super) struct VolumeUsage {
    pub reservation: Usage,
    /// Includes record/owner metadata and both journal files during compaction.
    pub local_file_bytes: u64,
    pub local_allocated_bytes: u64,
}
pub(super) fn volume_usage(dir: &Path, size: u64) -> Result<VolumeUsage> {
    let mut out = VolumeUsage {
        reservation: Usage::default(),
        local_file_bytes: 0,
        local_allocated_bytes: 0,
    };
    out.reservation.add(size)?;
    let mut stack = vec![dir.to_path_buf()];
    let mut visited = 0;
    while let Some(path) = stack.pop() {
        visited += 1;
        if visited > 1024 {
            return Err("unexpected volume directory size".into());
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // Atomic journal/record replacement may remove a sampled path.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && path != dir => continue,
            Err(e) => return Err(e.into()),
        };
        if meta.is_dir() {
            for item in fs::read_dir(path)? {
                if stack.len() + visited >= 1024 {
                    return Err("unexpected volume directory size".into());
                }
                stack.push(item?.path());
            }
        } else if meta.is_file() {
            out.local_file_bytes = out
                .local_file_bytes
                .checked_add(meta.len())
                .ok_or("usage overflow")?;
            out.local_allocated_bytes = out
                .local_allocated_bytes
                .checked_add(meta.blocks().checked_mul(512).ok_or("usage overflow")?)
                .ok_or("usage overflow")?;
        } else {
            return Err("unexpected volume file type".into());
        }
    }
    Ok(out)
}
