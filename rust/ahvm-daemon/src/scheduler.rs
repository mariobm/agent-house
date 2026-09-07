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
}

impl OpsLimiter {
    pub fn new(permits: usize) -> Self {
        Self {
            sem: Arc::new(Semaphore::new(permits.max(1))),
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
