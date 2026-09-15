//! Disposable shared cache for immutable base metadata only. Direct-mapped slots
//! bound disk use to 16 MiB; collisions and corrupt entries simply fetch again.
use crate::{digest, CHUNK_BYTES};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
#[derive(Debug)]
pub(super) struct MetadataCache(PathBuf);
impl MetadataCache {
    pub(super) fn open(path: &Path) -> crate::Result<Self> {
        let m = fs::symlink_metadata(path).map_err(|_| crate::Error::Store)?;
        if !m.is_dir() || m.uid() != 0 || m.mode() & 0o077 != 0 {
            return Err(crate::Error::InvalidInput);
        }
        Ok(Self(path.to_owned()))
    }
    fn slot(&self, hash: &str) -> Option<PathBuf> {
        let hash = crate::indexed::decode(hash).ok()?;
        Some(self.0.join(format!("{:02x}", hash[0])))
    }
    pub(super) fn get(&self, hash: &str) -> Option<Vec<u8>> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(self.slot(hash)?)
            .ok()?;
        let mut bytes = Vec::new();
        file.take(CHUNK_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        (bytes.len() == CHUNK_BYTES && digest(&bytes) == hash).then_some(bytes)
    }
    pub(super) fn put(&self, hash: &str, bytes: &[u8]) {
        if bytes.len() != CHUNK_BYTES || digest(bytes) != hash {
            return;
        }
        let Some(slot) = self.slot(hash) else {
            return;
        };
        // Concurrent reads/writes may miss the cache, but cannot return bad
        // bytes: every hit is checked against the requested content hash.
        // Write directly so crashes cannot accumulate temporary files.
        let _ = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(slot)?;
            file.write_all(bytes)
        })();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug)]
    struct Remote(std::sync::atomic::AtomicUsize);
    impl crate::ObjectStore for Remote {
        fn base_chunk(&self, _: &str, _: &str, _: Option<u64>) -> crate::Result<Vec<u8>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(vec![23; CHUNK_BYTES])
        }
        fn head(&self, _: &str) -> crate::Result<Option<crate::Head>> {
            Ok(None)
        }
        fn chunk(&self, _: &str, _: &str) -> crate::Result<Vec<u8>> {
            Err(crate::Error::Store)
        }
        fn put_chunk(&self, _: &str, _: &str, _: &[u8]) -> crate::Result<()> {
            unreachable!()
        }
        fn publish(&self, _: &str, _: Option<&str>, _: &[u8]) -> crate::Result<String> {
            unreachable!()
        }
    }
    #[test]
    fn new_worker_reuses_metadata_but_not_mutable_or_private_objects() {
        use crate::{cache::CachedStore, ObjectStore};
        let dir = std::env::temp_dir().join(format!("shared-metadata-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let remote = std::sync::Arc::new(Remote(std::sync::atomic::AtomicUsize::new(0)));
        let hash = digest(&vec![23; CHUNK_BYTES]);
        for _ in 0..2 {
            let mut cache = CachedStore::new(remote.clone(), CHUNK_BYTES).unwrap();
            cache.metadata = Some(MetadataCache(dir.clone()));
            assert_eq!(
                cache.base_chunk("image", &hash, None).unwrap(),
                vec![23; CHUNK_BYTES]
            );
            assert!(cache.chunk("private", &hash).is_err());
            assert!(cache.head("mutable").unwrap().is_none());
        }
        assert_eq!(remote.0.load(std::sync::atomic::Ordering::Relaxed), 1);
        let cache = MetadataCache(dir.clone());
        fs::write(cache.slot(&hash).unwrap(), b"bad cache").unwrap();
        let mut worker = CachedStore::new(remote.clone(), CHUNK_BYTES).unwrap();
        worker.metadata = Some(cache);
        assert_eq!(
            worker.base_chunk("image", &hash, None).unwrap(),
            vec![23; CHUNK_BYTES]
        );
        assert_eq!(remote.0.load(std::sync::atomic::Ordering::Relaxed), 2);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn reusable_bounded_cache_rejects_corruption_and_symlinks() {
        let dir = std::env::temp_dir().join(format!("metadata-cache-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let cache = MetadataCache(dir.clone());
        let bytes = vec![17; CHUNK_BYTES];
        let hash = digest(&bytes);
        cache.put(&hash, &bytes);
        let reopened = MetadataCache(dir.clone());
        assert_eq!(reopened.get(&hash), Some(bytes.clone()));
        let slot = cache.slot(&hash).unwrap();
        fs::write(&slot, vec![18; CHUNK_BYTES]).unwrap();
        assert!(cache.get(&hash).is_none());
        fs::remove_file(&slot).unwrap();
        let other = dir.join("untouched");
        fs::write(&other, b"untouched").unwrap();
        std::os::unix::fs::symlink(&other, &slot).unwrap();
        cache.put(&hash, &bytes);
        assert!(cache.get(&hash).is_none());
        assert_eq!(fs::read(&other).unwrap(), b"untouched");
        fs::remove_file(slot).unwrap();
        fs::remove_file(other).unwrap();
        for i in 0..512_u64 {
            let mut bytes = vec![0; CHUNK_BYTES];
            bytes[..8].copy_from_slice(&i.to_le_bytes());
            cache.put(&digest(&bytes), &bytes);
        }
        assert!(fs::read_dir(&dir).unwrap().count() <= 256);
        fs::remove_dir_all(dir).unwrap();
    }
}
