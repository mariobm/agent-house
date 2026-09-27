//! Admission for expensive backend ops: a semaphore bounds simultaneous
//! boots/restores/snapshots (KVM + disk heavy). Reads (status/exec/files,
//! session drains) bypass it. No nesting: mutating routes hold at most one
//! permit, so the bound cannot deadlock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone)]
pub struct OpsLimiter {
    pub transfers: crate::bandwidth::Registry,
    sem: Arc<Semaphore>,
    streams: Arc<Mutex<StreamCounts>>,
}

impl OpsLimiter {
    pub fn new(permits: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(permits.max(1))),
            streams: Arc::default(),
            transfers: crate::bandwidth::Registry::default(),
        }
    }

    pub fn with_api_rate(mut self, rate: Option<u64>) -> Self {
        self.transfers = crate::bandwidth::Registry::new(rate);
        self
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
    inner: Arc<Mutex<HashMap<String, Arc<LifecycleEntry>>>>,
}
#[derive(Debug, Default)]
struct LifecycleEntry {
    mutex: Arc<tokio::sync::Mutex<()>>,
    epoch: std::sync::atomic::AtomicU64,
}

/// A lease exists before queuing, and is reclaimed even when a waiter cancels.
#[derive(Debug)]
struct LifecycleLease {
    registry: LifecycleLocks,
    id: String,
    entry: Arc<LifecycleEntry>,
    epoch: u64,
}
impl Drop for LifecycleLease {
    fn drop(&mut self) {
        let mut map = self
            .registry
            .inner
            .lock()
            .expect("lifecycle mutex poisoned");
        // Map + this lease are the only references. Removing under this same
        // mutex makes it impossible for a newcomer to receive a second lock.
        if Arc::strong_count(&self.entry) == 2 {
            map.remove(&self.id);
        }
    }
}
#[derive(Debug)]
pub struct LifecycleGuard {
    // Unlock before reclaiming the lease.
    _guard: tokio::sync::OwnedMutexGuard<()>,
    lease: LifecycleLease,
}
impl LifecycleGuard {
    pub fn identity_changed(&self) -> bool {
        self.lease
            .entry
            .epoch
            .load(std::sync::atomic::Ordering::Relaxed)
            != self.lease.epoch
    }
    pub fn invalidate_identity(&self) {
        self.lease
            .entry
            .epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}
impl LifecycleLocks {
    pub fn new() -> Self {
        Self::default()
    }
    #[cfg(test)]
    pub(crate) fn retained_entries(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
    fn lease(&self, id: &str) -> LifecycleLease {
        let mut map = self.inner.lock().expect("lifecycle mutex poisoned");
        let entry = map.entry(id.to_owned()).or_default().clone();
        let epoch = entry.epoch.load(std::sync::atomic::Ordering::Relaxed);
        LifecycleLease {
            registry: self.clone(),
            id: id.to_owned(),
            entry,
            epoch,
        }
    }
    pub async fn lock(&self, id: &str) -> LifecycleGuard {
        let lease = self.lease(id);
        let guard = lease.entry.mutex.clone().lock_owned().await;
        LifecycleGuard {
            _guard: guard,
            lease,
        }
    }
    pub fn try_lock(&self, id: &str) -> Option<LifecycleGuard> {
        let lease = self.lease(id);
        let guard = lease.entry.mutex.clone().try_lock_owned().ok()?;
        Some(LifecycleGuard {
            _guard: guard,
            lease,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lifecycle_reclaims_churn_and_cancelled_waiters_without_split_locks() {
        let locks = LifecycleLocks::new();
        for i in 0..10000 {
            drop(locks.lock(&format!("missing-{i}")).await);
        }
        assert!(locks.inner.lock().unwrap().is_empty());
        let held = locks.lock("same").await;
        let mut waiting = Box::pin(locks.lock("same"));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        assert!(locks.try_lock("same").is_none());
        drop(waiting);
        assert_eq!(locks.inner.lock().unwrap().len(), 1);
        let peer = locks.lock("independent").await;
        drop(peer);
        assert_eq!(locks.inner.lock().unwrap().len(), 1);
        drop(held);
        assert!(locks.inner.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn queued_identity_epoch_changes_and_map_reclaims_after_completion() {
        let locks = LifecycleLocks::new();
        let held = locks.lock("same").await;
        let mut waiting = Box::pin(locks.lock("same"));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        held.invalidate_identity();
        drop(held);
        let acquired = waiting.await;
        assert!(acquired.identity_changed());
        assert_eq!(locks.inner.lock().unwrap().len(), 1);
        drop(acquired);
        assert!(locks.inner.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reclamation_never_splits_same_key_mutex() {
        let locks = LifecycleLocks::new();
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let (locks, active) = (locks.clone(), active.clone());
            workers.push(tokio::spawn(async move {
                for _ in 0..1000 {
                    let guard = locks.lock("same").await;
                    assert_eq!(active.fetch_add(1, std::sync::atomic::Ordering::SeqCst), 0);
                    tokio::task::yield_now().await;
                    assert_eq!(active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst), 1);
                    drop(guard);
                }
            }));
        }
        for worker in workers {
            worker.await.unwrap();
        }
        assert!(locks.inner.lock().unwrap().is_empty());
    }

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
