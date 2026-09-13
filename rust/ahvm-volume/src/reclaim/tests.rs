use super::*;
use crate::{digest, indexed::IndexedVolume, nbd::Disk, owned::OwnedDisk, CHUNK_BYTES};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

#[derive(Debug, Default)]
pub(crate) struct Memory(Mutex<State>);
#[derive(Debug, Default)]
struct State {
    heads: BTreeMap<String, Head>,
    chunks: BTreeMap<(String, String), Vec<u8>>,
    revision: u64,
    lose_publish: bool,
    fail_delete: bool,
}
impl ObjectStore for Memory {
    fn head(&self, id: &str) -> Result<Option<Head>> {
        Ok(self.0.lock().unwrap().heads.get(id).cloned())
    }
    fn chunk(&self, id: &str, hash: &str) -> Result<Vec<u8>> {
        self.0
            .lock()
            .unwrap()
            .chunks
            .get(&(id.into(), hash.into()))
            .cloned()
            .ok_or(Error::Corrupt)
    }
    fn put_chunk(&self, id: &str, hash: &str, b: &[u8]) -> Result<()> {
        assert_eq!(hash, digest(b));
        self.0
            .lock()
            .unwrap()
            .chunks
            .insert((id.into(), hash.into()), b.into());
        Ok(())
    }
    fn publish(&self, id: &str, expected: Option<&str>, bytes: &[u8]) -> Result<String> {
        let mut s = self.0.lock().unwrap();
        if s.heads.get(id).map(|h| h.revision.as_str()) != expected {
            return Err(Error::Conflict);
        }
        s.revision += 1;
        let revision = s.revision.to_string();
        s.heads.insert(
            id.into(),
            Head {
                revision: revision.clone(),
                manifest: bytes.into(),
            },
        );
        if s.lose_publish {
            s.lose_publish = false;
            return Err(Error::Uncertain);
        }
        Ok(revision)
    }
    fn list_chunks(&self, id: &str, limit: usize) -> Result<Vec<String>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .chunks
            .keys()
            .filter(|(v, _)| v == id)
            .take(limit)
            .map(|(_, h)| h.clone())
            .collect())
    }
    fn delete_chunk(&self, id: &str, hash: &str) -> Result<()> {
        let mut s = self.0.lock().unwrap();
        s.chunks.remove(&(id.into(), hash.into()));
        if s.fail_delete {
            s.fail_delete = false;
            return Err(Error::Store);
        }
        Ok(())
    }
}
static NEXT: AtomicU64 = AtomicU64::new(0);
fn dir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "ahvm-reclaim-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&p).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
    p
}
fn image(s: Arc<Memory>, id: &str) {
    let mut v = IndexedVolume::create(s.clone(), id, CHUNK_BYTES as u64).unwrap();
    v.write(0, &vec![7; CHUNK_BYTES]).unwrap();
    v.commit().unwrap();
    OwnedDisk::enroll(s, id).unwrap();
}
#[test]
fn retirement_refuses_live_worker_and_other_owner() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    let p = dir();
    let other = dir();
    let worker = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    assert!(OwnedDisk::retire(s.clone(), "disk", &p).is_err());
    assert!(OwnedDisk::retire(s.clone(), "disk", &other).is_err());
    assert!(sweep(s.as_ref(), "disk", 16).is_err());
    drop(worker);
    OwnedDisk::retire(s.clone(), "disk", &p).unwrap();
    assert!(OwnedDisk::open(s.clone(), "disk", &p).is_err());
    assert!(OwnedDisk::enroll(s.clone(), "disk").is_err());
    fs::remove_dir_all(p).unwrap();
    fs::remove_dir_all(other).unwrap();
}
#[test]
fn interrupted_deletes_resume_without_touching_peer_or_reusing_identity() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    image(s.clone(), "peer");
    let p = dir();
    let stale = s.head("disk").unwrap().unwrap();
    OwnedDisk::retire(s.clone(), "disk", &p).unwrap();
    s.0.lock().unwrap().fail_delete = true;
    assert!(sweep(s.as_ref(), "disk", 1).is_err());
    while !sweep(s.as_ref(), "disk", 1).unwrap().complete {}
    let peer = IndexedVolume::from_head(s.clone(), "peer", {
        let h = s.head("peer").unwrap().unwrap();
        let e: serde_json::Value = serde_json::from_slice(&h.manifest).unwrap();
        Head {
            revision: h.revision,
            manifest: serde_json::to_vec(&e["manifest"]).unwrap(),
        }
    })
    .unwrap();
    let mut b = [0];
    peer.read(0, &mut b).unwrap();
    assert_eq!(b, [7]);
    assert!(s
        .publish("disk", Some(&stale.revision), &stale.manifest)
        .is_err());
    assert!(retired("disk", &s.head("disk").unwrap().unwrap()).unwrap());
    fs::remove_dir_all(p).unwrap();
}
#[test]
fn lost_retirement_reply_and_late_orphan_upload_are_retryable() {
    let s = Arc::new(Memory::default());
    let p = dir();
    s.0.lock().unwrap().lose_publish = true;
    assert!(OwnedDisk::retire(s.clone(), "disk", &p).is_err());
    OwnedDisk::retire(s.clone(), "disk", &p).unwrap();
    assert!(sweep(s.as_ref(), "disk", 1).unwrap().complete);
    let bytes = vec![3; CHUNK_BYTES];
    s.put_chunk("disk", &digest(&bytes), &bytes).unwrap();
    assert!(!sweep(s.as_ref(), "disk", 1).unwrap().complete);
    assert!(sweep(s.as_ref(), "disk", 1).unwrap().complete);
    fs::remove_dir_all(p).unwrap();
}
#[test]
fn incomplete_import_and_dirty_deleted_disk_can_be_retired() {
    let s = Arc::new(Memory::default());
    let p = dir();
    let mut import = IndexedVolume::create_import(s.clone(), "import", CHUNK_BYTES as u64).unwrap();
    import.write(0, &[1]).unwrap();
    import.commit().unwrap();
    OwnedDisk::retire(s.clone(), "import", &p).unwrap();
    assert!(import.finish_import().is_err());
    image(s.clone(), "disk");
    let mut worker = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    worker.write(0, &[9]).unwrap();
    worker.flush().unwrap();
    drop(worker);
    // Delete does not upload dirty data the caller explicitly discarded.
    OwnedDisk::retire(s.clone(), "disk", &p).unwrap();
    while !sweep(s.as_ref(), "disk", 16).unwrap().complete {}
    fs::remove_dir_all(p).unwrap();
}
#[test]
fn malformed_tombstone_or_live_head_never_authorizes_deletion() {
    let s = Memory::default();
    s.publish(
        "disk",
        None,
        br#"{"format":5,"volume":"other","deleted":true}"#,
    )
    .unwrap();
    assert!(sweep(&s, "disk", 16).is_err());
    assert!(sweep(&s, "disk", 0).is_err());
}

#[test]
fn ready_import_before_enrollment_can_be_retired() {
    let s = Arc::new(Memory::default());
    let p = dir();
    IndexedVolume::create(s.clone(), "disk", CHUNK_BYTES as u64).unwrap();
    OwnedDisk::retire(s.clone(), "disk", &p).unwrap();
    assert!(sweep(s.as_ref(), "disk", 16).unwrap().complete);
    fs::remove_dir_all(p).unwrap();
}

#[test]
fn explicitly_released_owner_cannot_block_retirement_or_resume_io() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    let p = dir();
    let mut old = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    old.release().unwrap();
    OwnedDisk::retire(s.clone(), "disk", &p).unwrap();
    assert!(old.write(0, &[1]).is_err());
    while !sweep(s.as_ref(), "disk", 16).unwrap().complete {}
    drop(old);
    fs::remove_dir_all(p).unwrap();
}
