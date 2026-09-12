use super::*;
use crate::{indexed::tests::Store, Head};
use std::io::SeekFrom;
use std::sync::atomic::{AtomicBool, Ordering};
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "ahvm-local-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        Self(p)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[derive(Debug, Default)]
struct Faults {
    inner: Store,
    offline: AtomicBool,
    lost: AtomicBool,
}
impl Faults {
    fn check(&self) -> Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            Err(Error::Store)
        } else {
            Ok(())
        }
    }
}
impl ObjectStore for Faults {
    fn head(&self, id: &str) -> Result<Option<Head>> {
        self.check()?;
        self.inner.head(id)
    }
    fn chunk(&self, id: &str, h: &str) -> Result<Vec<u8>> {
        self.check()?;
        self.inner.chunk(id, h)
    }
    fn put_chunk(&self, id: &str, h: &str, b: &[u8]) -> Result<()> {
        self.check()?;
        self.inner.put_chunk(id, h, b)
    }
    fn publish(&self, id: &str, r: Option<&str>, b: &[u8]) -> Result<String> {
        self.check()?;
        let out = self.inner.publish(id, r, b)?;
        if self.lost.load(Ordering::SeqCst) {
            Err(Error::Uncertain)
        } else {
            Ok(out)
        }
    }
}
fn fixture() -> Arc<Faults> {
    let s = Arc::new(Faults::default());
    IndexedVolume::create(s.clone(), "local", 128 * 1024 * 1024).unwrap();
    s
}
#[test]
fn local_flush_survives_process_reopen_but_host_loss_can_lose_pending_data() {
    let s = fixture();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    s.offline.store(true, Ordering::SeqCst);
    disk.write(0, b"pending").unwrap();
    disk.flush().unwrap();
    assert!(disk.sync_remote().is_err());
    assert_eq!(disk.status().remote_sequence, 0);
    drop(disk);
    s.offline.store(false, Ordering::SeqCst);
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    let mut b = [0; 7];
    disk.read(0, &mut b).unwrap();
    assert_eq!(&b, b"pending");
    let fresh = Directory::new();
    let mut host_loss = LocalDisk::open(s.clone(), "local", &fresh.0).unwrap();
    host_loss.read(0, &mut b).unwrap();
    assert_eq!(b, [0; 7]);
    drop(host_loss);
    let status = disk.sync_remote().unwrap();
    assert_eq!(status.pending_bytes, 0);
    assert_eq!(status.remote_sequence, status.local_sequence);
    drop(disk);
    let fresh = Directory::new();
    let mut restored = LocalDisk::open(s, "local", &fresh.0).unwrap();
    restored.read(0, &mut b).unwrap();
    assert_eq!(&b, b"pending");
}
#[test]
fn lost_publication_response_reconciles_and_preserves_newer_local_write() {
    let s = fixture();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    disk.write(0, b"old").unwrap();
    disk.flush().unwrap();
    s.lost.store(true, Ordering::SeqCst);
    assert!(disk.sync_remote().is_err());
    disk.write(0, b"new").unwrap();
    disk.flush().unwrap();
    drop(disk);
    s.lost.store(false, Ordering::SeqCst);
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    assert_eq!(disk.status().remote_sequence, 1);
    let mut b = [0; 3];
    disk.read(0, &mut b).unwrap();
    assert_eq!(&b, b"new");
    disk.sync_remote().unwrap();
    drop(disk);
    let remote = IndexedVolume::open(s, "local").unwrap();
    remote.read(0, &mut b).unwrap();
    assert_eq!(&b, b"new");
}
#[test]
fn lock_torn_tail_and_corruption_are_handled() {
    let s = fixture();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    assert!(matches!(
        LocalDisk::open(s.clone(), "local", &d.0),
        Err(Error::Conflict)
    ));
    disk.write(0, b"ok").unwrap();
    disk.flush().unwrap();
    drop(disk);
    let path = d.0.join("journal");
    let old = fs::metadata(&path).unwrap().len();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"torn")
        .unwrap();
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    let mut b = [0; 2];
    disk.read(0, &mut b).unwrap();
    assert_eq!(&b, b"ok");
    drop(disk);
    assert_eq!(fs::metadata(&path).unwrap().len(), old);
    let mut file = OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::End(-1)).unwrap();
    file.write_all(b"!").unwrap();
    drop(file);
    assert!(matches!(
        LocalDisk::open(s, "local", &d.0),
        Err(Error::Corrupt)
    ));
}
#[test]
fn another_journal_owner_cannot_silently_overwrite_remote_state() {
    let s = fixture();
    let a = Directory::new();
    let b = Directory::new();
    let mut first = LocalDisk::open(s.clone(), "local", &a.0).unwrap();
    let mut second = LocalDisk::open(s.clone(), "local", &b.0).unwrap();
    first.write(0, b"first").unwrap();
    first.flush().unwrap();
    second.write(0, b"second").unwrap();
    second.sync_remote().unwrap();
    assert!(matches!(first.sync_remote(), Err(Error::Conflict)));
    assert!(first.flush().is_err());
    drop(first);
    assert!(matches!(
        LocalDisk::open(s, "local", &a.0),
        Err(Error::Conflict)
    ));
}
#[test]
fn zero_write_still_records_remote_journal_watermark() {
    let s = fixture();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    disk.write(0, &[0; 512]).unwrap();
    disk.flush().unwrap();
    assert_eq!(disk.sync_remote().unwrap().remote_sequence, 1);
    assert_eq!(
        IndexedVolume::open(s, "local")
            .unwrap()
            .replication()
            .unwrap()
            .sequence,
        1
    );
}
#[test]
fn dirty_backlog_is_bounded_and_compaction_replays_latest_data() {
    let s = fixture();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    let bytes = vec![7; WRITE_LIMIT];
    disk.write(0, &bytes).unwrap();
    disk.write(WRITE_LIMIT as u64, &bytes).unwrap();
    assert!(matches!(
        disk.write(DIRTY_LIMIT as u64, b"full"),
        Err(Error::Backpressure)
    ));
    disk.flush().unwrap();
    disk.write(0, b"last").unwrap();
    disk.state.lock().unwrap().compact().unwrap();
    drop(disk);
    let mut disk = LocalDisk::open(s, "local", &d.0).unwrap();
    let mut b = [0; 4];
    disk.read(0, &mut b).unwrap();
    assert_eq!(&b, b"last");
    assert_eq!(disk.status().pending_bytes, DIRTY_LIMIT);
}
#[test]
fn foreground_flush_does_not_wait_for_remote_upload() {
    #[derive(Debug)]
    struct Park {
        inner: Store,
        entered: mpsc::Sender<()>,
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }
    impl ObjectStore for Park {
        fn head(&self, id: &str) -> Result<Option<Head>> {
            self.inner.head(id)
        }
        fn chunk(&self, id: &str, h: &str) -> Result<Vec<u8>> {
            self.inner.chunk(id, h)
        }
        fn publish(&self, id: &str, r: Option<&str>, b: &[u8]) -> Result<String> {
            self.inner.publish(id, r, b)
        }
        fn put_chunk(&self, id: &str, h: &str, b: &[u8]) -> Result<()> {
            let _ = self.entered.send(());
            let (mut ready, cv) = (self.release.0.lock().unwrap(), &self.release.1);
            while !*ready {
                ready = cv.wait(ready).unwrap();
            }
            self.inner.put_chunk(id, h, b)
        }
    }
    struct Release(Arc<(Mutex<bool>, std::sync::Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }
    let release = Release(Arc::new((Mutex::new(false), std::sync::Condvar::new())));
    let (tx, rx) = mpsc::channel();
    let s = Arc::new(Park {
        inner: Store::default(),
        entered: tx,
        release: release.0.clone(),
    });
    IndexedVolume::create(s.clone(), "local", 1024 * 1024).unwrap();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s, "local", &d.0).unwrap();
    disk.write(0, b"old").unwrap();
    disk.flush().unwrap();
    let background = disk.clone();
    let worker = thread::spawn(move || background.sync_remote());
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (done, completed) = mpsc::channel();
    let mut foreground = disk.clone();
    let foreground = thread::spawn(move || {
        foreground.write(0, b"new").unwrap();
        foreground.flush().unwrap();
        done.send(foreground.status().local_sequence).unwrap();
    });
    assert_eq!(completed.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
    drop(release);
    foreground.join().unwrap();
    assert_eq!(worker.join().unwrap().unwrap().remote_sequence, 1);
    assert!(disk.status().pending_bytes > 0);
    disk.sync_remote().unwrap();
    assert_eq!(disk.status().remote_sequence, 2);
}

#[test]
fn successful_retry_returns_current_replication_status() {
    let s = fixture();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    disk.write(0, b"retry").unwrap();
    disk.flush().unwrap();
    s.offline.store(true, Ordering::SeqCst);
    assert!(disk.sync_remote().is_err());
    assert!(disk.status().replication_failed);
    s.offline.store(false, Ordering::SeqCst);
    assert!(!disk.sync_remote().unwrap().replication_failed);
}
#[test]
fn journal_write_error_cannot_be_followed_by_a_successful_flush() {
    let s = fixture();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s, "local", &d.0).unwrap();
    disk.state.lock().unwrap().file = File::open(d.0.join("journal")).unwrap(); // read-only descriptor
    assert!(disk.write(0, b"cannot-log").is_err());
    assert!(disk.flush().is_err());
}
#[test]
fn corrupted_header_is_rejected_even_when_json_still_parses() {
    let s = fixture();
    let d = Directory::new();
    let disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    drop(disk);
    let path = d.0.join("journal");
    let mut bytes = fs::read(&path).unwrap();
    let at = bytes.windows(5).position(|b| b == b"local").unwrap();
    bytes[at] = b'L';
    fs::write(path, bytes).unwrap();
    assert!(matches!(
        LocalDisk::open(s, "local", &d.0),
        Err(Error::Corrupt)
    ));
}

#[test]
fn compaction_failure_after_reconciling_a_lost_response_poisons_local_io() {
    let s = fixture();
    let d = Directory::new();
    let mut disk = LocalDisk::open(s.clone(), "local", &d.0).unwrap();
    disk.write(0, b"remote-accepted").unwrap();
    disk.flush().unwrap();
    s.lost.store(true, Ordering::SeqCst);
    assert!(disk.sync_remote().is_err());
    s.lost.store(false, Ordering::SeqCst);
    fs::create_dir(d.0.join("journal.new")).unwrap();
    assert!(disk.sync_remote().is_err());
    assert!(disk.flush().is_err());
    assert!(disk.write(0, b"unsafe").is_err());
}
