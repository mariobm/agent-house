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
    reads: usize,
}
impl ObjectStore for Memory {
    fn head(&self, id: &str) -> Result<Option<Head>> {
        Ok(self.0.lock().unwrap().heads.get(id).cloned())
    }
    fn chunk(&self, id: &str, hash: &str) -> Result<Vec<u8>> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        state
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
    fn list_chunks_after(
        &self,
        id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .chunks
            .keys()
            .filter(|(v, h)| v == id && after.is_none_or(|a| h.as_str() > a))
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

fn chunks(s: &Memory, id: &str) -> usize {
    s.0.lock()
        .unwrap()
        .chunks
        .keys()
        .filter(|(v, _)| v == id)
        .count()
}
fn collect_all(disk: &OwnedDisk, limit: usize) {
    let mut after = None;
    for _ in 0..100 {
        let p = disk
            .collect_offline(after.as_deref(), limit, || false)
            .unwrap();
        if p.complete {
            return;
        }
        after = p.next_after;
    }
    panic!("collection did not finish");
}
#[test]
fn offline_collection_preserves_current_graph_and_peer() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    image(s.clone(), "peer");
    let p = dir();
    let mut disk = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    disk.write(0, &vec![8; CHUNK_BYTES]).unwrap();
    disk.flush().unwrap();
    disk.sync_remote().unwrap();
    assert_eq!(chunks(&s, "disk"), 4); // Two page/data generations.
    collect_all(&disk, 1); // Must advance past retained objects, not just page one.
    assert_eq!(chunks(&s, "disk"), 2);
    assert_eq!(chunks(&s, "peer"), 2);
    let mut out = [0];
    disk.read(0, &mut out).unwrap();
    assert_eq!(out, [8]);
    drop(disk);
    let mut reopened = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    reopened.read(0, &mut out).unwrap();
    assert_eq!(out, [8]);
    fs::remove_dir_all(p).unwrap();
}
#[test]
fn pending_journal_and_cancelled_mark_never_delete() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    let p = dir();
    let mut disk = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    disk.write(0, &[9]).unwrap();
    disk.flush().unwrap();
    assert!(matches!(
        disk.collect_offline(None, 16, || false),
        Err(Error::Backpressure)
    ));
    assert_eq!(chunks(&s, "disk"), 2);
    disk.sync_remote().unwrap();
    let count = chunks(&s, "disk");
    let calls = AtomicU64::new(0);
    assert!(matches!(
        disk.collect_offline(None, 16, || calls.fetch_add(1, Ordering::Relaxed) >= 2),
        Err(Error::Deadline)
    ));
    assert_eq!(chunks(&s, "disk"), count);
    fs::remove_dir_all(p).unwrap();
}
#[test]
fn missing_page_or_unknown_checkpoint_metadata_blocks_collection() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    let p = dir();
    let disk = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    let h = s.head("disk").unwrap().unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&h.manifest).unwrap();
    let hash = value["manifest"]["pages"]["0"]
        .as_str()
        .unwrap()
        .to_string();
    s.0.lock().unwrap().chunks.remove(&("disk".into(), hash));
    assert!(disk.collect_offline(None, 16, || false).is_err());
    assert_eq!(chunks(&s, "disk"), 1);
    value["checkpoints"] = serde_json::json!([{"expires_at":9999999999u64}]);
    s.publish(
        "disk",
        Some(&h.revision),
        &serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    assert!(disk.collect_offline(None, 16, || false).is_err());
    assert_eq!(chunks(&s, "disk"), 1);
    fs::remove_dir_all(p).unwrap();
}
#[test]
fn failed_delete_and_restart_rebuild_references_before_continuing() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    let p = dir();
    let mut disk = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    disk.write(0, &vec![8; CHUNK_BYTES]).unwrap();
    disk.sync_remote().unwrap();
    s.0.lock().unwrap().fail_delete = true;
    assert!(disk.collect_offline(None, 128, || false).is_err());
    let mut out = [0];
    disk.read(0, &mut out).unwrap();
    assert_eq!(out, [8]);
    // A write between passes may reference an old digest again. Re-mark, don't
    // reuse the previous garbage set. Missing chunks are uploaded again.
    disk.write(0, &vec![7; CHUNK_BYTES]).unwrap();
    disk.sync_remote().unwrap();
    drop(disk);
    let mut disk = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    collect_all(&disk, 1);
    disk.read(0, &mut out).unwrap();
    assert_eq!(out, [7]);
    assert_eq!(chunks(&s, "disk"), 2);
    fs::remove_dir_all(p).unwrap();
}

#[test]
fn unchanged_head_reuses_mark_but_new_publication_invalidates_it() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    let p = dir();
    let mut disk = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    let mut mark = None;
    disk.collect_offline_cached(None, 1, || false, &mut mark)
        .unwrap();
    let reads = s.0.lock().unwrap().reads;
    disk.collect_offline_cached(None, 1, || false, &mut mark)
        .unwrap();
    assert_eq!(s.0.lock().unwrap().reads, reads);
    disk.write(0, &vec![8; CHUNK_BYTES]).unwrap();
    disk.sync_remote().unwrap();
    let reads = s.0.lock().unwrap().reads;
    disk.collect_offline_cached(None, 128, || false, &mut mark)
        .unwrap();
    assert!(s.0.lock().unwrap().reads > reads);
    let mut b = [0];
    disk.read(0, &mut b).unwrap();
    assert_eq!(b, [8]);
    fs::remove_dir_all(p).unwrap();
}

#[test]
fn fully_zeroed_disk_reclaims_all_chunks_but_keeps_readable_head() {
    let s = Arc::new(Memory::default());
    image(s.clone(), "disk");
    let p = dir();
    let mut disk = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    disk.write(0, &vec![0; CHUNK_BYTES]).unwrap();
    disk.sync_remote().unwrap();
    collect_all(&disk, 1);
    assert_eq!(chunks(&s, "disk"), 0);
    drop(disk);
    let mut disk = OwnedDisk::open(s.clone(), "disk", &p).unwrap();
    let mut b = [1];
    disk.read(0, &mut b).unwrap();
    assert_eq!(b, [0]);
    fs::remove_dir_all(p).unwrap();
}
