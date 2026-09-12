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
            capacity: bytes / CHUNK_BYTES,
            state: Arc::new(Mutex::new(State::default())),
        })
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
    fn prefetch(&self, id: &str, hashes: &[String]) {
        // No queue and at most seven speculative requests, independent of the
        // caller. A slow adjacent object must not delay a demanded read.
        for hash in hashes {
            let key = (id.to_owned(), hash.clone());
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
                    if let Ok(bytes) = inner.chunk(&key.0, &key.1) {
                        let _ = Self::insert_into(&state, capacity, &key.0, &key.1, &bytes);
                    }
                    drop(pending);
                });
        }
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
        cache.prefetch("v", &hashes);
        for _ in 0..7 {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        cache.prefetch("v", &hashes);
        assert_eq!(cache.state.lock().unwrap().pending.len(), 7);
        assert!(rx.try_recv().is_err());
        let wanted = vec![99; CHUNK_BYTES];
        assert_eq!(cache.chunk("v", &digest(&wanted)).unwrap(), wanted);
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
