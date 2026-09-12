//! Disposable, volume-scoped FIFO cache of verified immutable objects.
use crate::{digest, Error, Head, ObjectStore, Result, CHUNK_BYTES};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
type Key = (String, String);
#[derive(Default)]
struct State {
    objects: HashMap<Key, Vec<u8>>,
    order: VecDeque<Key>,
}
pub struct CachedStore {
    inner: Arc<dyn ObjectStore>,
    capacity: usize,
    state: Mutex<State>,
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
            state: Mutex::new(State::default()),
        })
    }
    pub fn bytes(&self) -> usize {
        self.state.lock().unwrap().objects.len() * CHUNK_BYTES
    }
    fn insert(&self, id: &str, hash: &str, bytes: &[u8]) -> Result<()> {
        if bytes.len() != CHUNK_BYTES || digest(bytes) != hash {
            return Err(Error::Corrupt);
        }
        let key = (id.to_owned(), hash.to_owned());
        let mut s = self.state.lock().unwrap();
        if !s.objects.contains_key(&key) {
            while s.objects.len() >= self.capacity {
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
