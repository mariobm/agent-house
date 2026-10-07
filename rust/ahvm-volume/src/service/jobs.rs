//! Admit work before creating threads, including when the OS refuses a spawn.
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
};

pub(super) struct Jobs {
    active: Arc<AtomicUsize>,
    limit: usize,
}

impl Jobs {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            active: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }

    pub(super) fn take(&self) -> Option<Permit> {
        let mut current = self.active.load(Ordering::Acquire);
        while current < self.limit {
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Permit(self.active.clone())),
                Err(observed) => current = observed,
            }
        }
        None
    }
}

pub(super) struct Permit(Arc<AtomicUsize>);

impl Permit {
    pub(super) fn spawn(
        self,
        name: &str,
        job: impl FnOnce() + Send + 'static,
    ) -> io::Result<JoinHandle<()>> {
        self.spawn_with(
            |job| thread::Builder::new().name(name.into()).spawn(job),
            job,
        )
    }

    fn spawn_with(
        self,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<JoinHandle<()>>,
        job: impl FnOnce() + Send + 'static,
    ) -> io::Result<JoinHandle<()>> {
        spawn(Box::new(move || {
            let _permit = self;
            job();
        }))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Barrier, Condvar, Mutex};

    #[test]
    fn concurrent_admission_never_exceeds_limit_and_releases_all_permits() {
        let jobs = Arc::new(Jobs::new(4));
        let gate = Arc::new(Barrier::new(33));
        let (tx, rx) = mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..32 {
            let (jobs, gate, tx) = (jobs.clone(), gate.clone(), tx.clone());
            workers.push(thread::spawn(move || {
                gate.wait();
                let permit = jobs.take();
                tx.send(permit.is_some()).unwrap();
                gate.wait();
                drop(permit);
            }));
        }
        gate.wait();
        assert_eq!((0..32).filter(|_| rx.recv().unwrap()).count(), 4);
        assert!(jobs.take().is_none());
        gate.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(jobs.active.load(Ordering::Acquire), 0);
        assert!(jobs.take().is_some());
    }

    #[test]
    fn large_startup_burst_keeps_cleanup_separate_from_live_recovery() {
        let cleanup = Jobs::new(1);
        let live = Jobs::new(8);
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..1024 {
            if let Some(permit) = cleanup.take() {
                let gate = gate.clone();
                workers.push(
                    permit
                        .spawn("test-cleanup", move || {
                            let (lock, wake) = &*gate;
                            let _guard = wake
                                .wait_while(lock.lock().unwrap(), |done| !*done)
                                .unwrap();
                        })
                        .unwrap(),
                );
            }
        }
        assert_eq!(workers.len(), 1);
        live.take()
            .unwrap()
            .spawn("test-live", move || tx.send(()).unwrap())
            .unwrap()
            .join()
            .unwrap();
        rx.recv().unwrap();
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(cleanup.take().is_some());
    }

    #[test]
    fn failed_spawn_releases_admission_and_allows_retry() {
        let jobs = Jobs::new(1);
        let error = jobs
            .take()
            .unwrap()
            .spawn_with(
                |_| Err(io::Error::from(io::ErrorKind::WouldBlock)),
                || panic!("failed spawn must not run the job"),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        jobs.take()
            .unwrap()
            .spawn("test-retry", || {})
            .unwrap()
            .join()
            .unwrap();
        assert!(jobs.take().is_some());
    }
}
