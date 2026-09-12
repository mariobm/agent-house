//! Admission for expensive backend ops: a semaphore bounds simultaneous
//! boots/restores/snapshots (KVM + disk heavy). Reads (status/exec/files,
//! session drains) bypass it. No nesting: mutating routes hold at most one
//! permit, so the bound cannot deadlock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct OpsLimiter {
    sem: Arc<Semaphore>,
    streams: Arc<Mutex<StreamCounts>>,
}

impl OpsLimiter {
    pub fn new(permits: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(permits.max(1))),
            streams: Arc::default(),
        }
    }

    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        self.sem
            .clone()
            .acquire_owned()
            .await
            .expect("ops semaphore closed")
    }

    /// Non-blocking take for the thermal sweep: a busy scheduler defers
    /// the row to the next sweep instead of stalling the whole pass.
    pub fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.sem.clone().try_acquire_owned().ok()
    }

    #[cfg(test)]
    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }
}

/// Per-sandbox lifecycle serialization: routes and the thermal sweep take
/// the same id's lock around their read-modify-write sequences (backend op
/// plus store mirror), so a sampled state can never overwrite a newer
/// commit. One global order prevents deadlocks: lifecycle, then permit,
/// then backend. Locks are never nested across ids (fork will order
/// multiple ids lexicographically when it needs two).
#[derive(Debug, Clone, Default)]
pub struct LifecycleLocks {
    inner: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl LifecycleLocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn lock(&self, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let entry = {
            let mut inner = self.inner.lock().expect("lifecycle mutex poisoned");
            inner
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        entry.lock_owned().await
    }

    /// Non-blocking take for the sweep: one long lifecycle transition
    /// must not stall reconciliation of later rows (mirrors the permit
    /// handling).
    pub fn try_lock(&self, id: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        let entry = {
            let mut inner = self.inner.lock().expect("lifecycle mutex poisoned");
            inner
                .entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        entry.try_lock_owned().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permits_bound_and_release() {
        let lim = OpsLimiter::new(2);
        assert_eq!(lim.available(), 2);
        let _a = lim.acquire().await;
        let _b = lim.acquire().await;
        assert_eq!(lim.available(), 0);
        assert!(lim.sem.try_acquire().is_err());
        drop(_a);
        assert_eq!(lim.available(), 1);
    }
}

/// Shared admission for long-lived guest transports, separate from lifecycle
/// permits so open terminals cannot prevent a VM from being stopped.
#[derive(Debug, Default)]
struct StreamCounts {
    total: usize,
    by_sandbox: HashMap<String, usize>,
}

#[derive(Debug)]
pub struct StreamGuard {
    counts: Arc<Mutex<StreamCounts>>,
    id: String,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().expect("stream counts poisoned");
        counts.total -= 1;
        let n = counts
            .by_sandbox
            .get_mut(&self.id)
            .expect("stream count missing");
        *n -= 1;
        if *n == 0 {
            counts.by_sandbox.remove(&self.id);
        }
    }
}

impl OpsLimiter {
    pub fn try_stream(&self, id: &str) -> Option<Arc<StreamGuard>> {
        let mut counts = self.streams.lock().expect("stream counts poisoned");
        if counts.total >= 32 || counts.by_sandbox.get(id).copied().unwrap_or(0) >= 4 {
            return None;
        }
        counts.total += 1;
        *counts.by_sandbox.entry(id.to_owned()).or_default() += 1;
        Some(Arc::new(StreamGuard {
            counts: self.streams.clone(),
            id: id.to_owned(),
        }))
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    #[test]
    fn caps_are_shared_and_retained_by_detached_backend_work() {
        let limiter = OpsLimiter::new(2);
        let mut guards: Vec<_> = (0..4).map(|_| limiter.try_stream("a").unwrap()).collect();
        assert!(limiter.clone().try_stream("a").is_none());
        let peer = limiter.try_stream("b").unwrap();
        let backend = guards.pop().unwrap();
        let detached = backend.clone();
        drop(backend);
        assert!(limiter.try_stream("a").is_none());
        drop(detached);
        assert!(limiter.try_stream("a").is_some());
        drop(guards);
        drop(peer);
        assert!(limiter.streams.lock().unwrap().by_sandbox.is_empty());
    }
    #[test]
    fn global_cap_does_not_queue_connections() {
        let limiter = OpsLimiter::new(2);
        let guards: Vec<_> = (0..32)
            .map(|i| limiter.try_stream(&i.to_string()).unwrap())
            .collect();
        assert!(limiter.try_stream("new").is_none());
        drop(guards);
        assert!(limiter.try_stream("new").is_some());
    }
}
