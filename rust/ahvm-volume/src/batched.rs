//! One serialized writer with periodic remote commits. No unbounded work queue.
use crate::{indexed::IndexedVolume, nbd::Disk, Result};
use std::{
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};
pub struct BatchedDisk {
    volume: Arc<Mutex<IndexedVolume>>,
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}
impl std::fmt::Debug for BatchedDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BatchedDisk")
    }
}
impl BatchedDisk {
    pub fn new(volume: IndexedVolume) -> Self {
        Self::with_interval(volume, Duration::from_secs(1))
    }
    fn with_interval(volume: IndexedVolume, interval: Duration) -> Self {
        let volume = Arc::new(Mutex::new(volume));
        let (stop, rx) = mpsc::channel();
        let shared = volume.clone();
        let worker = thread::spawn(move || {
            let mut wait = interval;
            while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(wait) {
                let mut volume = shared.lock().unwrap();
                if volume.dirty_bytes() != 0 {
                    if volume.commit().is_err() {
                        wait = Duration::from_secs(5);
                    } else {
                        wait = interval;
                    }
                }
            }
        });
        Self {
            volume,
            stop,
            worker: Some(worker),
        }
    }
}
impl Disk for BatchedDisk {
    fn size(&self) -> u64 {
        self.volume.lock().unwrap().size()
    }
    fn read(&mut self, at: u64, bytes: &mut [u8]) -> Result<()> {
        self.volume.lock().unwrap().read(at, bytes)
    }
    fn write(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        self.volume.lock().unwrap().write(at, bytes)
    }
    fn flush(&mut self) -> Result<()> {
        self.volume.lock().unwrap().commit().map(|_| ())
    }
}
impl Drop for BatchedDisk {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{indexed::tests::Store, ObjectStore};
    #[test]
    fn background_commit_publishes_without_foreground_flush() {
        let store = Arc::new(Store::default());
        let volume = IndexedVolume::create(store.clone(), "background", 1024 * 1024).unwrap();
        let mut disk = BatchedDisk::with_interval(volume, Duration::from_millis(10));
        disk.write(0, b"background-data").unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let head = store.head("background").unwrap().unwrap();
            let root: serde_json::Value = serde_json::from_slice(&head.manifest).unwrap();
            if root["generation"].as_u64().unwrap() > 0 {
                break;
            }
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        disk.write(0, b"foreground-data").unwrap();
        disk.flush().unwrap();
        drop(disk);
        let reopened = IndexedVolume::open(store, "background").unwrap();
        let mut bytes = [0; 15];
        reopened.read(0, &mut bytes).unwrap();
        assert_eq!(&bytes, b"foreground-data");
    }
}
