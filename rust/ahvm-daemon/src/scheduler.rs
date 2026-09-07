//! Admission for expensive backend ops: a semaphore bounds simultaneous
//! boots/restores/snapshots (KVM + disk heavy). Reads (status/exec/files,
//! session drains) bypass it. No nesting: mutating routes hold at most one
//! permit, so the bound cannot deadlock.

use std::sync::Arc;
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

    #[cfg(test)]
    pub fn available(&self) -> usize {
        self.sem.available_permits()
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
