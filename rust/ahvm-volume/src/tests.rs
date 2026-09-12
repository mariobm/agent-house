use super::*;
use std::sync::Mutex;

#[derive(Debug, Default, Clone, Copy)]
enum Failure {
    #[default]
    None,
    Chunk(usize),
    BeforeHead,
    LostHeadReply,
}

/// A deterministic object-service model. Survives Volume handle loss, but is
/// intentionally not advertised as a disk, network or S3 durability test.
#[derive(Debug, Default)]
struct MemoryStore(Mutex<State>);
#[derive(Debug, Default)]
struct State {
    heads: BTreeMap<String, Head>,
    objects: BTreeMap<(String, String), Vec<u8>>,
    revision: u64,
    failure: Failure,
    puts: usize,
    gets: usize,
}
impl ObjectStore for MemoryStore {
    fn head(&self, volume: &str) -> Result<Option<Head>> {
        Ok(self.0.lock().unwrap().heads.get(volume).cloned())
    }
    fn chunk(&self, volume: &str, hash: &str) -> Result<Vec<u8>> {
        let mut state = self.0.lock().unwrap();
        state.gets += 1;
        state
            .objects
            .get(&(volume.into(), hash.into()))
            .cloned()
            .ok_or(Error::Corrupt)
    }
    fn put_chunk(&self, volume: &str, hash: &str, bytes: &[u8]) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        state.puts += 1;
        if matches!(state.failure, Failure::Chunk(n) if n == state.puts) {
            return Err(Error::Store);
        }
        assert_eq!(hash, digest(bytes));
        let object = state
            .objects
            .entry((volume.into(), hash.into()))
            .or_insert_with(|| bytes.to_vec());
        assert_eq!(object, bytes);
        Ok(())
    }
    fn publish(&self, volume: &str, expected: Option<&str>, bytes: &[u8]) -> Result<String> {
        let mut state = self.0.lock().unwrap();
        if state.heads.get(volume).map(|h| h.revision.as_str()) != expected {
            return Err(Error::Conflict);
        }
        if matches!(state.failure, Failure::BeforeHead) {
            return Err(Error::Store);
        }
        let manifest: Manifest = serde_json::from_slice(bytes).unwrap();
        // Publication must never make a reference to data that hasn't arrived.
        for hash in manifest.chunks.values() {
            assert!(state.objects.contains_key(&(volume.into(), hash.clone())));
        }
        state.revision += 1;
        let revision = format!("opaque-etag-{}", state.revision);
        state.heads.insert(
            volume.into(),
            Head {
                revision: revision.clone(),
                manifest: bytes.to_vec(),
            },
        );
        if matches!(state.failure, Failure::LostHeadReply) {
            return Err(Error::Store);
        }
        Ok(revision)
    }
}
fn store() -> Arc<MemoryStore> {
    Arc::new(MemoryStore::default())
}
fn read(volume: &Volume, offset: u64, size: usize) -> Vec<u8> {
    let mut bytes = vec![0; size];
    volume.read(offset, &mut bytes).unwrap();
    bytes
}

#[test]
fn committed_cross_chunk_write_reopens_without_client_state() {
    let store = store();
    let mut volume = Volume::create(store.clone(), "vm-1", 2 * CHUNK_BYTES as u64).unwrap();
    volume
        .write(CHUNK_BYTES as u64 - 3, b"persist-this")
        .unwrap();
    assert_eq!(volume.commit().unwrap(), 1);
    drop(volume);
    let reopened = Volume::open(store.clone(), "vm-1").unwrap();
    assert_eq!(
        store.0.lock().unwrap().gets,
        0,
        "open must not hydrate the disk"
    );
    assert_eq!(read(&reopened, CHUNK_BYTES as u64 - 3, 12), b"persist-this");
    assert_eq!(read(&reopened, 0, 8), vec![0; 8]);
}

#[test]
fn uncommitted_writes_do_not_survive_handle_loss() {
    let store = store();
    let mut volume = Volume::create(store.clone(), "vm", CHUNK_BYTES as u64).unwrap();
    volume.write(0, b"old").unwrap();
    volume.commit().unwrap();
    volume.write(0, b"new").unwrap();
    assert_eq!(read(&volume, 0, 3), b"new");
    drop(volume);
    assert_eq!(read(&Volume::open(store, "vm").unwrap(), 0, 3), b"old");
}

#[test]
fn failed_uploads_never_publish_partial_disk_and_can_retry() {
    for fail_at in [1, 2] {
        let store = store();
        let mut volume = Volume::create(store.clone(), "vm", 2 * CHUNK_BYTES as u64).unwrap();
        volume.write(CHUNK_BYTES as u64 - 1, b"ab").unwrap();
        store.0.lock().unwrap().failure = Failure::Chunk(fail_at);
        assert!(matches!(volume.commit(), Err(Error::Store)));
        assert_eq!(
            read(
                &Volume::open(store.clone(), "vm").unwrap(),
                CHUNK_BYTES as u64 - 1,
                2
            ),
            [0, 0]
        );
        store.0.lock().unwrap().failure = Failure::None;
        volume.commit().unwrap();
        assert_eq!(
            read(
                &Volume::open(store, "vm").unwrap(),
                CHUNK_BYTES as u64 - 1,
                2
            ),
            b"ab"
        );
    }
}

#[test]
fn ambiguous_publication_requires_reopen_whether_or_not_it_committed() {
    for (failure, expected) in [
        (Failure::BeforeHead, b"old"),
        (Failure::LostHeadReply, b"new"),
    ] {
        let store = store();
        let mut volume = Volume::create(store.clone(), "vm", CHUNK_BYTES as u64).unwrap();
        volume.write(0, b"old").unwrap();
        volume.commit().unwrap();
        volume.write(0, b"new").unwrap();
        store.0.lock().unwrap().failure = failure;
        assert!(matches!(volume.commit(), Err(Error::Uncertain)));
        assert!(matches!(volume.commit(), Err(Error::ReopenRequired)));
        assert!(matches!(
            volume.write(0, b"bad"),
            Err(Error::ReopenRequired)
        ));
        assert_eq!(read(&Volume::open(store, "vm").unwrap(), 0, 3), expected);
    }
}

#[test]
fn competing_writers_cannot_overwrite_a_newer_commit() {
    let store = store();
    let mut first = Volume::create(store.clone(), "vm", CHUNK_BYTES as u64).unwrap();
    let mut second = Volume::open(store.clone(), "vm").unwrap();
    first.write(0, b"winner").unwrap();
    second.write(0, b"loser!").unwrap();
    first.commit().unwrap();
    assert!(matches!(second.commit(), Err(Error::Conflict)));
    assert!(matches!(second.commit(), Err(Error::ReopenRequired)));
    assert_eq!(read(&Volume::open(store, "vm").unwrap(), 0, 6), b"winner");
}

#[test]
fn simultaneous_publications_have_exactly_one_winner() {
    let store = store();
    Volume::create(store.clone(), "vm", CHUNK_BYTES as u64).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let tasks: Vec<_> = (*b"ab")
        .into_iter()
        .map(|value| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut volume = Volume::open(store, "vm").unwrap();
                volume.write(0, &[value]).unwrap();
                barrier.wait();
                volume.commit()
            })
        })
        .collect();
    let results: Vec<_> = tasks.into_iter().map(|task| task.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(Error::Conflict)))
            .count(),
        1
    );
}

#[test]
fn missing_and_corrupt_chunks_are_errors_not_silent_zeros() {
    for remove in [false, true] {
        let store = store();
        let mut volume = Volume::create(store.clone(), "vm", CHUNK_BYTES as u64).unwrap();
        volume.write(0, b"important").unwrap();
        volume.commit().unwrap();
        {
            let mut state = store.0.lock().unwrap();
            if remove {
                state.objects.clear();
            } else {
                state.objects.values_mut().next().unwrap()[0] ^= 1;
            }
        }
        let reopened = Volume::open(store, "vm").unwrap();
        assert!(matches!(reopened.read(0, &mut [0; 1]), Err(Error::Corrupt)));
    }
}

#[test]
fn identity_format_and_range_checks_fail_closed() {
    let store = store();
    assert!(matches!(
        Volume::create(store.clone(), "../escape", CHUNK_BYTES as u64),
        Err(Error::InvalidInput)
    ));
    assert!(matches!(
        Volume::create(store.clone(), "vm", MAX_VOLUME_BYTES + 1),
        Err(Error::InvalidInput)
    ));
    let mut volume = Volume::create(store.clone(), "vm", CHUNK_BYTES as u64).unwrap();
    assert!(matches!(
        Volume::create(store.clone(), "vm", CHUNK_BYTES as u64),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        volume.write(u64::MAX, &[1]),
        Err(Error::InvalidInput)
    ));
    assert!(matches!(
        volume.write(CHUNK_BYTES as u64, &[1]),
        Err(Error::InvalidInput)
    ));
    for body in [
        br#"{"format":2,"volume":"vm","size":65536,"generation":0,"chunks":{}}"#.to_vec(),
        br#"{"format":1,"volume":"other","size":65536,"generation":0,"chunks":{}}"#.to_vec(),
        vec![0; MAX_MANIFEST_BYTES + 1],
    ] {
        store
            .0
            .lock()
            .unwrap()
            .heads
            .get_mut("vm")
            .unwrap()
            .manifest = body;
        assert!(matches!(
            Volume::open(store.clone(), "vm"),
            Err(Error::Corrupt)
        ));
    }
}

#[test]
fn failed_multi_chunk_write_does_not_partially_mutate_pending_data() {
    let store = store();
    let mut volume = Volume::create(store.clone(), "vm", 2 * CHUNK_BYTES as u64).unwrap();
    volume.write(CHUNK_BYTES as u64, b"missing-soon").unwrap();
    volume.commit().unwrap();
    volume.write(0, b"keep").unwrap();
    store.0.lock().unwrap().objects.clear();
    assert!(matches!(
        volume.write(CHUNK_BYTES as u64 - 1, b"xy"),
        Err(Error::Corrupt)
    ));
    assert_eq!(read(&volume, 0, 4), b"keep");
    assert_eq!(read(&volume, CHUNK_BYTES as u64 - 1, 1), [0]);
}

#[test]
fn zero_rewrites_remove_references_and_unchanged_chunks_are_shared() {
    let store = store();
    let mut volume = Volume::create(store.clone(), "vm", 2 * CHUNK_BYTES as u64).unwrap();
    volume.write(0, b"one").unwrap();
    volume.commit().unwrap();
    let hash = volume.manifest.chunks[&0].clone();
    volume.write(CHUNK_BYTES as u64, b"two").unwrap();
    volume.commit().unwrap();
    assert_eq!(volume.manifest.chunks[&0], hash);
    assert_eq!(store.0.lock().unwrap().puts, 2);
    volume.write(0, &vec![0; CHUNK_BYTES]).unwrap();
    volume.commit().unwrap();
    assert!(!volume.manifest.chunks.contains_key(&0));
    assert_eq!(read(&Volume::open(store, "vm").unwrap(), 0, 3), [0; 3]);
    // Old immutable objects remain until a separately qualified GC exists.
}
