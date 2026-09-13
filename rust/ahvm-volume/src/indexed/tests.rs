use super::*;
use crate::{cache::CachedStore, Head};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Mutex,
};
#[derive(Debug, Default)]
pub(crate) struct Store {
    heads: Mutex<BTreeMap<String, Head>>,
    objects: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    gets: AtomicUsize,
    puts: AtomicUsize,
    fail: AtomicBool,
    lost: AtomicBool,
}
impl ObjectStore for Store {
    fn base_chunk(&self, image: &str, hash: &str, _: Option<u64>) -> Result<Vec<u8>> {
        self.chunk(image, hash)
    }
    fn list_chunks(&self, id: &str, limit: usize) -> Result<Vec<String>> {
        Ok(self
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|(volume, _)| volume == id)
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
            .objects
            .lock()
            .unwrap()
            .keys()
            .filter(|(volume, h)| volume == id && after.is_none_or(|a| h.as_str() > a))
            .take(limit)
            .map(|(_, h)| h.clone())
            .collect())
    }
    fn delete_chunk(&self, id: &str, hash: &str) -> Result<()> {
        self.objects
            .lock()
            .unwrap()
            .remove(&(id.into(), hash.into()));
        Ok(())
    }
    fn head(&self, id: &str) -> Result<Option<Head>> {
        Ok(self.heads.lock().unwrap().get(id).cloned())
    }
    fn chunk(&self, id: &str, h: &str) -> Result<Vec<u8>> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.objects
            .lock()
            .unwrap()
            .get(&(id.into(), h.into()))
            .cloned()
            .ok_or(Error::Corrupt)
    }
    fn put_chunk(&self, id: &str, h: &str, b: &[u8]) -> Result<()> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(Error::Store);
        }
        self.objects
            .lock()
            .unwrap()
            .insert((id.into(), h.into()), b.into());
        Ok(())
    }
    fn publish(&self, id: &str, expected: Option<&str>, bytes: &[u8]) -> Result<String> {
        let mut heads = self.heads.lock().unwrap();
        if heads.get(id).map(|h| h.revision.as_str()) != expected {
            return Err(Error::Conflict);
        }
        let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        let manifest = if value["format"] == 4 {
            value["manifest"].clone()
        } else {
            value.clone()
        };
        if value["format"] != 5 {
            let root: Root = serde_json::from_value(manifest).unwrap();
            let objects = self.objects.lock().unwrap();
            let base = root.base.as_ref().map(|b| b.image.as_str()).unwrap_or(id);
            for h in root.pages.values() {
                let page = objects
                    .get(&(id.into(), h.clone()))
                    .or_else(|| objects.get(&(base.into(), h.clone())))
                    .expect("page missing at publish");
                for i in 0..SLOTS {
                    if let Some(hash) = slot(page, i) {
                        assert!(
                            objects.contains_key(&(id.into(), hash.clone()))
                                || objects.contains_key(&(base.into(), hash))
                        );
                    }
                }
            }
        }
        let revision = format!(
            "rev-{}",
            heads
                .get(id)
                .map_or(0, |h| h.revision[4..].parse::<u64>().unwrap() + 1)
        );
        heads.insert(
            id.into(),
            Head {
                revision: revision.clone(),
                manifest: bytes.into(),
            },
        );
        if self.lost.load(Ordering::SeqCst) {
            return Err(Error::Store);
        }
        Ok(revision)
    }
}
fn create(store: Arc<dyn ObjectStore>) -> IndexedVolume {
    IndexedVolume::create(store, "v", MAX_SIZE).unwrap()
}
#[test]
fn large_sparse_disk_opens_lazily_and_reopens_across_page_boundary() {
    let s = Arc::new(Store::default());
    let mut v = create(s.clone());
    let at = SLOTS * CHUNK_BYTES as u64 - 2;
    v.write(at, b"across-pages").unwrap();
    v.write(MAX_SIZE - 4, b"tail").unwrap();
    v.commit().unwrap();
    drop(v);
    let before = s.gets.load(Ordering::SeqCst);
    let v = IndexedVolume::open(s.clone(), "v").unwrap();
    assert_eq!(s.gets.load(Ordering::SeqCst), before);
    let mut b = [0; 12];
    v.read(at, &mut b).unwrap();
    assert_eq!(&b, b"across-pages");
    let head = s.head("v").unwrap().unwrap();
    assert!(head.manifest.len() < 1024);
    let root: Root = serde_json::from_slice(&head.manifest).unwrap();
    assert_eq!(root.pages.len(), 3);
}
#[test]
fn failed_upload_or_budget_preserves_head_and_pending_data() {
    let s = Arc::new(Store::default());
    let mut v = create(s.clone());
    v.write(0, b"old").unwrap();
    v.commit().unwrap();
    let revision = s.head("v").unwrap().unwrap().revision;
    v.write(0, b"new").unwrap();
    s.fail.store(true, Ordering::SeqCst);
    assert!(v.commit().is_err());
    assert_eq!(s.head("v").unwrap().unwrap().revision, revision);
    s.fail.store(false, Ordering::SeqCst);
    assert!(matches!(
        v.commit_until(Instant::now()),
        Err(Error::Deadline)
    ));
    assert_eq!(s.head("v").unwrap().unwrap().revision, revision);
    v.commit().unwrap();
    let mut b = [0; 3];
    IndexedVolume::open(s, "v")
        .unwrap()
        .read(0, &mut b)
        .unwrap();
    assert_eq!(&b, b"new");
}
#[test]
fn stale_and_ambiguous_writers_require_reopen() {
    let s = Arc::new(Store::default());
    let mut first = create(s.clone());
    let mut stale = IndexedVolume::open(s.clone(), "v").unwrap();
    first.write(0, b"winner").unwrap();
    first.commit().unwrap();
    stale.write(0, b"stale").unwrap();
    assert!(matches!(stale.commit(), Err(Error::Conflict)));
    assert!(matches!(stale.write(0, b"x"), Err(Error::ReopenRequired)));
    first.write(0, b"lost").unwrap();
    s.lost.store(true, Ordering::SeqCst);
    assert!(matches!(first.commit(), Err(Error::Uncertain)));
    assert!(matches!(first.commit(), Err(Error::ReopenRequired)));
    let mut b = [0; 4];
    IndexedVolume::open(s, "v")
        .unwrap()
        .read(0, &mut b)
        .unwrap();
    assert_eq!(&b, b"lost");
}
#[test]
fn incomplete_import_cannot_be_opened() {
    let s = Arc::new(Store::default());
    let mut v = IndexedVolume::create_import(s.clone(), "v", MAX_SIZE).unwrap();
    v.write(0, b"partial").unwrap();
    v.commit().unwrap();
    assert!(matches!(
        IndexedVolume::open(s.clone(), "v"),
        Err(Error::NotReady)
    ));
    v.finish_import().unwrap();
    assert!(IndexedVolume::open(s, "v").is_ok());
}
#[test]
fn page_corruption_and_missing_data_never_become_holes() {
    let s = Arc::new(Store::default());
    let mut v = create(s.clone());
    v.write(0, b"data").unwrap();
    v.commit().unwrap();
    let hash = v.root.pages[&0].clone();
    let page = s
        .objects
        .lock()
        .unwrap()
        .remove(&("v".into(), hash.clone()))
        .unwrap();
    assert!(matches!(v.read(0, &mut [0; 4]), Err(Error::Corrupt)));
    let mut bad = page.clone();
    bad[8] = 99;
    s.objects
        .lock()
        .unwrap()
        .insert(("v".into(), hash.clone()), bad);
    assert!(matches!(v.read(0, &mut [0; 4]), Err(Error::Corrupt)));
    s.objects
        .lock()
        .unwrap()
        .insert(("v".into(), hash), page.clone());
    let data = slot(&page, 0).unwrap();
    s.objects.lock().unwrap().remove(&("v".into(), data));
    assert!(matches!(v.read(0, &mut [0; 4]), Err(Error::Corrupt)));
}
#[test]
fn cache_is_bounded_verified_and_does_not_cache_heads() {
    let s = Arc::new(Store::default());
    let c = Arc::new(CachedStore::new(s.clone(), 2 * CHUNK_BYTES).unwrap());
    let mut v = create(c.clone());
    v.write(0, b"cached").unwrap();
    v.commit().unwrap();
    let before = s.gets.load(Ordering::SeqCst);
    v.read(0, &mut [0; 6]).unwrap();
    v.read(0, &mut [0; 6]).unwrap();
    assert_eq!(s.gets.load(Ordering::SeqCst), before);
    for n in 1..5 {
        v.write(n * CHUNK_BYTES as u64, &vec![n as u8; CHUNK_BYTES])
            .unwrap();
        v.commit().unwrap();
        assert!(c.bytes() <= 2 * CHUNK_BYTES);
    }
    let mut other = IndexedVolume::open(c.clone(), "v").unwrap();
    v.write(0, b"winner").unwrap();
    v.commit().unwrap();
    other.write(0, b"stale").unwrap();
    assert!(matches!(other.commit(), Err(Error::Conflict)));
}
#[test]
fn dirty_limit_applies_backpressure_and_full_overwrites_need_no_reads() {
    let s = Arc::new(Store::default());
    let mut v = create(s.clone());
    let block = vec![1; WRITE_LIMIT];
    v.write(0, &block).unwrap();
    v.write(WRITE_LIMIT as u64, &block).unwrap();
    assert_eq!(v.dirty_bytes(), DIRTY_LIMIT);
    assert_eq!(s.gets.load(Ordering::SeqCst), 0);
    s.fail.store(true, Ordering::SeqCst);
    assert!(v.write(DIRTY_LIMIT as u64, &[2; CHUNK_BYTES]).is_err());
    assert_eq!(v.dirty_bytes(), DIRTY_LIMIT);
    s.fail.store(false, Ordering::SeqCst);
    v.write(DIRTY_LIMIT as u64, &[2; CHUNK_BYTES]).unwrap();
    assert_eq!(v.dirty_bytes(), CHUNK_BYTES);
}
#[test]
fn zeroing_last_data_removes_page_and_invalid_ranges_fail() {
    let s = Arc::new(Store::default());
    let mut v = create(s);
    assert!(v.write(u64::MAX, b"bad").is_err());
    v.write(0, b"data").unwrap();
    v.commit().unwrap();
    v.write(0, &[0; CHUNK_BYTES]).unwrap();
    v.commit().unwrap();
    assert!(v.root.pages.is_empty());
}

#[test]
fn read_ahead_is_bounded_and_speculative_corruption_does_not_hide_requested_data() {
    let s = Arc::new(Store::default());
    let mut v = create(s.clone());
    for index in 0..10u64 {
        v.write(
            index * CHUNK_BYTES as u64,
            &vec![index as u8 + 1; CHUNK_BYTES],
        )
        .unwrap();
    }
    v.commit().unwrap();
    // Delete an adjacent object: reading the requested intact chunk must succeed,
    // while explicitly requesting the missing chunk must still fail closed.
    let missing = digest(&vec![2; CHUNK_BYTES]);
    s.objects.lock().unwrap().remove(&("v".into(), missing));
    let cached = Arc::new(CachedStore::new(s.clone(), 16 * CHUNK_BYTES).unwrap());
    let v = IndexedVolume::open(cached.clone(), "v").unwrap();
    let before = s.gets.load(Ordering::SeqCst);
    let mut byte = [0];
    v.read(0, &mut byte).unwrap();
    assert_eq!(byte, [1]);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while s.gets.load(Ordering::SeqCst) - before < 9 {
        assert!(std::time::Instant::now() < until);
        std::thread::yield_now();
    }
    assert_eq!(s.gets.load(Ordering::SeqCst) - before, 9); // one page + eight data requests
    let before = s.gets.load(Ordering::SeqCst);
    v.read(0, &mut byte).unwrap();
    assert_eq!(s.gets.load(Ordering::SeqCst), before); // cache hit performs no read-ahead
    assert!(v.read(CHUNK_BYTES as u64, &mut byte).is_err());
    assert!(cached.bytes() <= 16 * CHUNK_BYTES);
}

#[test]
fn identical_rewrites_and_zero_holes_do_not_republish() {
    let s = Arc::new(Store::default());
    let mut v = create(s.clone());
    v.write(0, b"same").unwrap();
    v.commit().unwrap();
    let puts = s.puts.load(Ordering::SeqCst);
    let before = s.head("v").unwrap().unwrap().revision;
    v.write(0, b"same").unwrap();
    v.write(CHUNK_BYTES as u64, &[0; 512]).unwrap();
    v.commit().unwrap();
    assert_eq!(s.puts.load(Ordering::SeqCst), puts);
    assert_eq!(s.head("v").unwrap().unwrap().revision, before);
    assert_eq!(v.dirty_bytes(), 0);
    // Mixed commits still publish changed chunks, including clearing old data.
    v.write(0, b"same").unwrap();
    v.write(CHUNK_BYTES as u64, b"new").unwrap();
    v.commit().unwrap();
    assert_eq!(s.puts.load(Ordering::SeqCst), puts + 2); // changed data + page
    v.write(0, &[0; 4]).unwrap();
    v.commit().unwrap();
    let reopened = IndexedVolume::open(s, "v").unwrap();
    let mut b = [0; 4];
    reopened.read(0, &mut b).unwrap();
    assert_eq!(b, [0; 4]);
    reopened.read(CHUNK_BYTES as u64, &mut b[..3]).unwrap();
    assert_eq!(&b[..3], b"new");
}

#[test]
fn unfinished_import_can_resume_only_before_ready() {
    let store = Arc::new(Store::default());
    let id = "resume";
    let size = 2 * crate::CHUNK_BYTES as u64;
    let mut d = IndexedVolume::create_import(store.clone(), id, size).unwrap();
    d.write(0, &vec![7; crate::CHUNK_BYTES]).unwrap();
    d.commit().unwrap();
    drop(d);
    assert!(IndexedVolume::open(store.clone(), id).is_err());
    let mut d = IndexedVolume::resume_import(store.clone(), id, size).unwrap();
    d.write(0, &vec![0; crate::CHUNK_BYTES]).unwrap();
    d.write(crate::CHUNK_BYTES as u64, &vec![9; crate::CHUNK_BYTES])
        .unwrap();
    d.finish_import().unwrap();
    assert!(IndexedVolume::resume_import(store.clone(), id, size).is_err());
    let d = IndexedVolume::open(store.clone(), id).unwrap();
    let mut bytes = vec![1; crate::CHUNK_BYTES];
    d.read(0, &mut bytes).unwrap();
    assert!(bytes.iter().all(|b| *b == 0));
    d.read(crate::CHUNK_BYTES as u64, &mut bytes).unwrap();
    assert!(bytes.iter().all(|b| *b == 9));
}

#[test]
fn parallel_import_preserves_visibility_and_refuses_live_disks() {
    let store = Arc::new(Store::default());
    let id = "a".repeat(64);
    let mut disk =
        IndexedVolume::create_import(store.clone(), &id, 64 * CHUNK_BYTES as u64).unwrap();
    let mut bytes = vec![0; 64 * CHUNK_BYTES];
    for (i, chunk) in bytes.chunks_mut(CHUNK_BYTES).enumerate() {
        chunk.fill((i + 1) as u8);
    }
    disk.write(0, &bytes).unwrap();
    disk.commit_import().unwrap();
    assert!(IndexedVolume::open(store.clone(), &id).is_err());
    disk.finish_import().unwrap();
    assert!(disk.commit_import().is_err());
    let reopened = IndexedVolume::open(store, &id).unwrap();
    let mut out = vec![0; bytes.len()];
    reopened.read(0, &mut out).unwrap();
    assert_eq!(out, bytes);
}

#[test]
fn shared_base_isolated_writes_zeros_recovery_and_collection() {
    let store = Arc::new(Store::default());
    let image = "b".repeat(64);
    let mut original =
        IndexedVolume::create_import(store.clone(), &image, 3 * CHUNK_BYTES as u64).unwrap();
    original.write(0, &vec![4; 3 * CHUNK_BYTES]).unwrap();
    original.finish_import().unwrap();
    let reference = original.export_base().unwrap();
    let before = store.puts.load(Ordering::SeqCst);
    let mut first =
        IndexedVolume::create_from_base(store.clone(), "first", reference.clone()).unwrap();
    let second =
        IndexedVolume::create_from_base(store.clone(), "second", reference.clone()).unwrap();
    assert_eq!(
        store.puts.load(Ordering::SeqCst),
        before,
        "clone uploads no image data"
    );
    assert!(
        first.references(&|| false).unwrap().is_empty(),
        "base is outside VM GC"
    );
    first.write(0, b"private").unwrap();
    first
        .write(CHUNK_BYTES as u64, &vec![0; CHUNK_BYTES])
        .unwrap();
    first.commit().unwrap();
    let reopened = IndexedVolume::open(store.clone(), "first").unwrap();
    let mut data = vec![0; 3 * CHUNK_BYTES];
    reopened.read(0, &mut data).unwrap();
    assert_eq!(&data[..7], b"private");
    assert!(data[CHUNK_BYTES..2 * CHUNK_BYTES].iter().all(|b| *b == 0));
    assert!(data[2 * CHUNK_BYTES..].iter().all(|b| *b == 4));
    second.read(0, &mut data).unwrap();
    assert!(data.iter().all(|b| *b == 4));
    assert_eq!(
        reopened.references(&|| false).unwrap().len(),
        2,
        "only overlay page and changed block"
    );
    let puts = store.puts.load(Ordering::SeqCst);
    first.write(0, &vec![4; 3 * CHUNK_BYTES]).unwrap();
    first.commit().unwrap();
    assert_eq!(
        store.puts.load(Ordering::SeqCst),
        puts,
        "reverting to base uploads no duplicate bytes"
    );
    assert!(first.references(&|| false).unwrap().is_empty());
    first.write(0, &vec![0; 3 * CHUNK_BYTES]).unwrap();
    first.commit().unwrap();
    IndexedVolume::open(store.clone(), "first")
        .unwrap()
        .read(0, &mut data)
        .unwrap();
    assert!(
        data.iter().all(|b| *b == 0),
        "missing overlay page means zero, never base fallback"
    );
    // Immutable catalog is pinned; changing the mutable image import head cannot
    // redirect existing VMs to another image generation.
    store.heads.lock().unwrap().remove(&image);
    IndexedVolume::open(store.clone(), "second")
        .unwrap()
        .read(0, &mut data)
        .unwrap();
    assert!(data.iter().all(|b| *b == 4));
    store
        .objects
        .lock()
        .unwrap()
        .remove(&(image, reference.catalog));
    assert!(matches!(
        IndexedVolume::open(store, "second"),
        Err(Error::Corrupt)
    ));
}

#[test]
fn corrupt_overlay_never_falls_back_to_base() {
    let store = Arc::new(Store::default());
    let image = "c".repeat(64);
    let mut base = IndexedVolume::create_import(store.clone(), &image, CHUNK_BYTES as u64).unwrap();
    base.write(0, &vec![9; CHUNK_BYTES]).unwrap();
    base.finish_import().unwrap();
    let mut disk =
        IndexedVolume::create_from_base(store.clone(), "vm", base.export_base().unwrap()).unwrap();
    disk.write(0, &vec![8; CHUNK_BYTES]).unwrap();
    disk.commit().unwrap();
    store
        .objects
        .lock()
        .unwrap()
        .remove(&("vm".into(), digest(&vec![8; CHUNK_BYTES])));
    assert!(matches!(disk.read(0, &mut [0; 1]), Err(Error::Corrupt)));
}

#[cfg(unix)]
#[test]
fn base_backed_owned_disk_survives_eviction_and_retirement_is_scoped() {
    use crate::{nbd::Disk, owned::OwnedDisk};
    use std::os::unix::fs::PermissionsExt;
    let store = Arc::new(Store::default());
    let image = "f".repeat(64);
    let mut base = IndexedVolume::create_import(store.clone(), &image, CHUNK_BYTES as u64).unwrap();
    base.write(0, &vec![6; CHUNK_BYTES]).unwrap();
    base.finish_import().unwrap();
    let reference = base.export_base().unwrap();
    IndexedVolume::create_from_base(store.clone(), "owned", reference.clone()).unwrap();
    IndexedVolume::create_from_base(store.clone(), "peer", reference.clone()).unwrap();
    let path = std::env::temp_dir().join(format!("ahvm-base-owned-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Interrupted enrollment can retire a ready format-6 head without an owner.
    IndexedVolume::create_from_base(store.clone(), "unenrolled", reference).unwrap();
    OwnedDisk::retire(store.clone(), "unenrolled", &path).unwrap();
    OwnedDisk::enroll(store.clone(), "owned").unwrap();
    let mut disk = OwnedDisk::open(store.clone(), "owned", &path).unwrap();
    disk.write(0, b"changed").unwrap();
    disk.flush().unwrap();
    disk.sync_remote().unwrap();
    let collection = disk.collect_offline(None, 128, || false).unwrap();
    assert_eq!(collection.deleted_objects, 0);
    assert_eq!(collection.scanned_objects, 2);
    drop(disk);
    OwnedDisk::evict_local(store.clone(), "owned", &path).unwrap();
    let mut recovered = OwnedDisk::open(store.clone(), "owned", &path).unwrap();
    let mut bytes = [0; 7];
    recovered.read(0, &mut bytes).unwrap();
    assert_eq!(&bytes, b"changed");
    recovered.release().unwrap();
    drop(recovered);
    OwnedDisk::retire(store.clone(), "owned", &path).unwrap();
    while !crate::reclaim::sweep(store.as_ref(), "owned", 128)
        .unwrap()
        .complete
    {}
    let peer = IndexedVolume::open(store.clone(), "peer").unwrap();
    peer.read(0, &mut bytes).unwrap();
    assert_eq!(bytes, [6; 7]);
    assert!(!store.list_chunks(&image, 128).unwrap().is_empty());
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn pinned_catalog_rejects_validly_hashed_malformed_metadata() {
    let store = Arc::new(Store::default());
    let image = "1".repeat(64);
    let base = IndexedVolume::create(store.clone(), &image, CHUNK_BYTES as u64).unwrap();
    let reference = base.export_base().unwrap();
    let original = store.base_chunk(&image, &reference.catalog, None).unwrap();
    for mutation in 0..3 {
        let mut bytes = original.clone();
        match mutation {
            0 => bytes[8..16].fill(0),       // zero logical size
            1 => bytes[48 + 32] = 1,         // page beyond the one-block disk
            _ => bytes[CHUNK_BYTES - 1] = 1, // nonzero reserved padding
        }
        let hash = digest(&bytes);
        store.put_chunk(&image, &hash, &bytes).unwrap();
        assert!(matches!(
            IndexedVolume::create_from_base(
                store.clone(),
                "bad",
                BaseRef {
                    image: image.clone(),
                    catalog: hash
                }
            ),
            Err(Error::Corrupt)
        ));
    }
}
