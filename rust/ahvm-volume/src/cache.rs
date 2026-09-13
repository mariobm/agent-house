//! Disposable, volume-scoped FIFO cache of verified immutable objects.
use crate::{digest, Error, Head, ObjectStore, Result, CHUNK_BYTES};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
};
type Key = (String, String);
#[derive(Default)]
struct State {
    objects: HashMap<Key, Vec<u8>>,
    order: VecDeque<Key>,
    pending: HashSet<Key>,
}
pub struct CachedStore {
    inner: Arc<dyn ObjectStore>,
    capacity: usize,
    #[cfg(unix)]
    local_base: Option<(String, std::fs::File)>,
    #[cfg(unix)]
    local_failed: std::sync::atomic::AtomicBool,
    state: Arc<Mutex<State>>,
}
impl std::fmt::Debug for CachedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedStore")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}
impl CachedStore {
    pub fn new(inner: Arc<dyn ObjectStore>, bytes: usize) -> Result<Self> {
        if !(CHUNK_BYTES..=256 * 1024 * 1024).contains(&bytes) {
            return Err(Error::InvalidInput);
        }
        Ok(Self {
            inner,
            #[cfg(unix)]
            local_base: None,
            #[cfg(unix)]
            local_failed: std::sync::atomic::AtomicBool::new(false),
            capacity: bytes / CHUNK_BYTES,
            state: Arc::new(Mutex::new(State::default())),
        })
    }
    /// The image is only a cache. Every read is checked against the immutable
    /// remote map, so a stale/replaced host image cannot change guest data.
    #[cfg(unix)]
    pub fn with_local_base(mut self, image: String, file: std::fs::File) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata().map_err(|_| Error::Store)?;
        if crate::indexed::decode(&image).is_err()
            || !meta.is_file()
            || meta.uid() != 0
            || meta.mode() & 0o022 != 0
        {
            return Err(Error::InvalidInput);
        }
        self.local_base = Some((image, file));
        Ok(self)
    }
    fn prefetch_objects(&self, id: &str, hashes: &[String], base: bool) {
        // No queue and at most seven speculative requests, independent of the
        // caller. A slow adjacent object must not delay a demanded read.
        for hash in hashes {
            let key = (
                if base {
                    format!("base:{id}")
                } else {
                    id.to_owned()
                },
                hash.clone(),
            );
            {
                let mut state = self.state.lock().unwrap();
                if state.pending.len() >= 7 {
                    break;
                }
                if state.objects.contains_key(&key) || !state.pending.insert(key.clone()) {
                    continue;
                }
            }
            let inner = self.inner.clone();
            let source = id.to_owned();
            let state = self.state.clone();
            let capacity = self.capacity;
            // Guard also releases the reservation if a store panics or spawning fails.
            struct Pending(Arc<Mutex<State>>, Key);
            impl Drop for Pending {
                fn drop(&mut self) {
                    self.0.lock().unwrap().pending.remove(&self.1);
                }
            }
            let pending = Pending(state.clone(), key);
            let _ = std::thread::Builder::new()
                .name("volume-read-ahead".into())
                .spawn(move || {
                    let key = &pending.1;
                    let result = if base {
                        inner.base_chunk(&source, &key.1, None)
                    } else {
                        inner.chunk(&source, &key.1)
                    };
                    if let Ok(bytes) = result {
                        let _ = Self::insert_into(&state, capacity, &key.0, &key.1, &bytes);
                    }
                    drop(pending);
                });
        }
    }

    pub fn bytes(&self) -> usize {
        self.state.lock().unwrap().objects.len() * CHUNK_BYTES
    }
    fn insert(&self, id: &str, hash: &str, bytes: &[u8]) -> Result<()> {
        Self::insert_into(&self.state, self.capacity, id, hash, bytes)
    }
    fn insert_into(
        state: &Mutex<State>,
        capacity: usize,
        id: &str,
        hash: &str,
        bytes: &[u8],
    ) -> Result<()> {
        if bytes.len() != CHUNK_BYTES || digest(bytes) != hash {
            return Err(Error::Corrupt);
        }
        let key = (id.to_owned(), hash.to_owned());
        let mut s = state.lock().unwrap();
        if !s.objects.contains_key(&key) {
            while s.objects.len() >= capacity {
                let old = s.order.pop_front().unwrap();
                s.objects.remove(&old);
            }
            s.order.push_back(key.clone());
            s.objects.insert(key, bytes.to_vec());
        }
        Ok(())
    }
}
impl ObjectStore for CachedStore {
    fn base_chunk(&self, image: &str, hash: &str, offset: Option<u64>) -> Result<Vec<u8>> {
        let key = format!("base:{image}");
        if let Some(bytes) = self.cached_chunk(&key, hash) {
            return Ok(bytes);
        }
        #[cfg(unix)]
        if let (Some(offset), Some((local_image, file))) = (offset, &self.local_base) {
            use std::os::unix::fs::FileExt;
            if local_image == image && !self.local_failed.load(std::sync::atomic::Ordering::Relaxed)
            {
                let mut bytes = vec![0; CHUNK_BYTES];
                if file.read_exact_at(&mut bytes, offset).is_ok() && digest(&bytes) == hash {
                    // The kernel page cache already shares these bytes between VMs.
                    return Ok(bytes);
                }
                self.local_failed
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let bytes = self.inner.base_chunk(image, hash, offset)?;
        self.insert(&key, hash, &bytes)?;
        Ok(bytes)
    }
    fn prefetch(&self, id: &str, hashes: &[String]) {
        self.prefetch_objects(id, hashes, false);
    }
    fn prefetch_base(&self, image: &str, hashes: &[String]) {
        #[cfg(unix)]
        if self.local_base.as_ref().is_some_and(|(id, _)| id == image)
            && !self.local_failed.load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        self.prefetch_objects(image, hashes, true);
    }

    fn cached_chunk(&self, id: &str, hash: &str) -> Option<Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .objects
            .get(&(id.into(), hash.into()))
            .cloned()
    }
    fn head(&self, id: &str) -> Result<Option<Head>> {
        self.inner.head(id)
    } // mutable heads never cached
    fn chunk(&self, id: &str, hash: &str) -> Result<Vec<u8>> {
        if let Some(bytes) = self
            .state
            .lock()
            .unwrap()
            .objects
            .get(&(id.into(), hash.into()))
            .cloned()
        {
            return Ok(bytes);
        }
        let bytes = self.inner.chunk(id, hash)?;
        self.insert(id, hash, &bytes)?;
        Ok(bytes)
    }
    fn put_chunk(&self, id: &str, hash: &str, bytes: &[u8]) -> Result<()> {
        // Never infer remote existence from the cache after a failed/uncertain PUT.
        self.inner.put_chunk(id, hash, bytes)?;
        self.insert(id, hash, bytes)
    }
    fn publish(&self, id: &str, expected: Option<&str>, bytes: &[u8]) -> Result<String> {
        self.inner.publish(id, expected, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Condvar};
    use std::time::Duration;
    #[derive(Debug)]
    struct Blocked {
        release: Arc<(Mutex<bool>, Condvar)>,
        started: mpsc::Sender<()>,
    }
    impl ObjectStore for Blocked {
        fn base_chunk(&self, image: &str, hash: &str, _: Option<u64>) -> Result<Vec<u8>> {
            self.chunk(image, hash)
        }
        fn head(&self, _: &str) -> Result<Option<Head>> {
            unreachable!()
        }
        fn publish(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String> {
            unreachable!()
        }
        fn put_chunk(&self, _: &str, _: &str, _: &[u8]) -> Result<()> {
            unreachable!()
        }
        fn chunk(&self, _: &str, hash: &str) -> Result<Vec<u8>> {
            let wanted = vec![99; CHUNK_BYTES];
            if hash == digest(&wanted) {
                return Ok(wanted);
            }
            self.started.send(()).unwrap();
            let (lock, signal) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = signal.wait(released).unwrap();
            }
            Err(Error::Store)
        }
    }
    #[test]
    fn blocked_read_ahead_is_bounded_and_does_not_block_demanded_reads() {
        check_read_ahead(false);
    }
    #[test]
    fn blocked_base_read_ahead_is_bounded_and_does_not_block_demanded_reads() {
        check_read_ahead(true);
    }
    fn check_read_ahead(base: bool) {
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0 .0.lock().unwrap() = true;
                self.0 .1.notify_all();
            }
        }
        let release = Release(Arc::new((Mutex::new(false), Condvar::new())));
        let (tx, rx) = mpsc::channel();
        let cache = CachedStore::new(
            Arc::new(Blocked {
                release: release.0.clone(),
                started: tx,
            }),
            CHUNK_BYTES,
        )
        .unwrap();
        let hashes: Vec<_> = (0..20).map(|i| digest(&vec![i; CHUNK_BYTES])).collect();
        if base {
            cache.prefetch_base("v", &hashes);
        } else {
            cache.prefetch("v", &hashes);
        }
        for _ in 0..7 {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        if base {
            cache.prefetch_base("v", &hashes);
        } else {
            cache.prefetch("v", &hashes);
        }
        assert_eq!(cache.state.lock().unwrap().pending.len(), 7);
        assert!(rx.try_recv().is_err());
        let wanted = vec![99; CHUNK_BYTES];
        let result = if base {
            cache.base_chunk("v", &digest(&wanted), Some(0))
        } else {
            cache.chunk("v", &digest(&wanted))
        };
        assert_eq!(result.unwrap(), wanted);
        assert_eq!(cache.bytes(), CHUNK_BYTES);
        drop(release);
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !cache.state.lock().unwrap().pending.is_empty() {
            assert!(std::time::Instant::now() < end);
            std::thread::yield_now();
        }
        assert_eq!(cache.bytes(), CHUNK_BYTES); // failed speculative reads never enter cache
    }
}

#[cfg(all(test, unix))]
mod base_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn local_image_is_verified_cache_and_remote_fallback_survives_changes() {
        let remote = Arc::new(crate::indexed::tests::Store::default());
        let image = "d".repeat(64);
        let bytes = vec![9; CHUNK_BYTES];
        let hash = digest(&bytes);
        let path = std::env::temp_dir().join(format!("ahvm-base-cache-{}", std::process::id()));
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        writer.write_all(&bytes).unwrap();
        let mut cache = CachedStore::new(remote.clone(), CHUNK_BYTES).unwrap();
        // Exercise the cache path without requiring a root-owned test fixture.
        // Production admission goes through with_local_base's ownership checks.
        cache.local_base = Some((image.clone(), std::fs::File::open(&path).unwrap()));
        assert_eq!(cache.base_chunk(&image, &hash, Some(0)).unwrap(), bytes);
        assert!(cache.base_chunk(&"e".repeat(64), &hash, Some(0)).is_err());
        writer.set_len(0).unwrap();
        assert!(
            cache.base_chunk(&image, &hash, Some(0)).is_err(),
            "truncated cache is not valid data"
        );
        remote.put_chunk(&image, &hash, &bytes).unwrap();
        assert_eq!(cache.base_chunk(&image, &hash, Some(0)).unwrap(), bytes);
        std::fs::remove_file(path).unwrap();
    }
}
