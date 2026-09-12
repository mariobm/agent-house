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
        let root: Root = serde_json::from_slice(bytes).unwrap();
        let objects = self.objects.lock().unwrap();
        for h in root.pages.values() {
            let page = objects
                .get(&(id.into(), h.clone()))
                .expect("page missing at publish");
            for i in 0..SLOTS {
                if let Some(hash) = slot(page, i) {
                    assert!(objects.contains_key(&(id.into(), hash)));
                }
            }
        }
        let revision = format!("rev-{}", root.generation);
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
    assert_eq!(s.gets.load(Ordering::SeqCst) - before, 9); // one page + eight data requests
    let before = s.gets.load(Ordering::SeqCst);
    v.read(0, &mut byte).unwrap();
    assert_eq!(s.gets.load(Ordering::SeqCst), before); // cache hit performs no read-ahead
    assert!(v.read(CHUNK_BYTES as u64, &mut byte).is_err());
    assert!(cached.bytes() <= 16 * CHUNK_BYTES);
}
