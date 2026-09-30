//! Private-S3 qualification of packed writes, offline compaction and bulk deletion.
use ahvm_volume::{
    cache::CachedStore,
    indexed::IndexedVolume,
    nbd::Disk,
    owned::OwnedDisk,
    reclaim,
    s3::{Config, S3Store},
    ObjectStore, CHUNK_BYTES, MAX_OBJECT_BYTES,
};
use std::{fs, io::Read, os::unix::fs::PermissionsExt, sync::Arc, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: packed_probe CONFIG.json")?;
    let mut config = Config::from_file(path.as_ref())?;
    config.prefix.push_str("/packed-qualification");
    let store: Arc<dyn ObjectStore> = Arc::new(S3Store::new(config.clone())?);
    let mut random = [0; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let id = random
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let directory = std::env::temp_dir().join(format!("ahvm-packed-probe-{id}"));
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    println!("qualification volume: {id}");
    let start = Instant::now();
    let mut image = IndexedVolume::create(store.clone(), &id, (16 * CHUNK_BYTES) as u64)?;
    for i in 0..16 {
        image.write((i * CHUNK_BYTES) as u64, &vec![i as u8 + 1; CHUNK_BYTES])?;
    }
    image.commit()?;
    let initial = store.list_chunks(&id, 1000)?;
    assert_eq!(
        initial.len(),
        2,
        "sixteen blocks require one pack plus one page"
    );
    let sizes = initial
        .iter()
        .map(|h| store.chunk(&id, h).map(|b| b.len()))
        .collect::<Result<Vec<_>, _>>()?;
    assert!(sizes.contains(&MAX_OBJECT_BYTES));
    assert!(sizes.contains(&(128 * 1024)));
    drop(image);
    let fresh: Arc<dyn ObjectStore> = Arc::new(S3Store::new(config)?);
    let cache = Arc::new(CachedStore::new(fresh, 2 * MAX_OBJECT_BYTES)?);
    let reopened = IndexedVolume::open(cache.clone(), &id)?;
    for index in 0..16 {
        let mut byte = [0];
        reopened.read(index * CHUNK_BYTES as u64, &mut byte)?;
        assert_eq!(byte, [index as u8 + 1]);
    }
    assert!(cache.bytes() <= 2 * MAX_OBJECT_BYTES);
    OwnedDisk::enroll(store.clone(), &id)?;
    let mut disk = OwnedDisk::open(store.clone(), &id, &directory)?;
    for i in 0..15 {
        disk.write((i * CHUNK_BYTES) as u64, &vec![i as u8 + 33; CHUNK_BYTES])?;
    }
    disk.flush()?;
    disk.sync_remote()?;
    let mut after = None;
    for _ in 0..16 {
        let p = disk.collect_offline(after.as_deref(), 1000, || false)?;
        if p.complete {
            break;
        }
        after = p.next_after;
    }
    assert_eq!(
        store.list_chunks(&id, 1000)?.len(),
        3,
        "partial old pack must remain live"
    );
    let mut cursor = None;
    let mut rewritten = 0;
    let mut compacted = false;
    for _ in 0..64 {
        let progress = disk.compact_offline(cursor, 2 * MAX_OBJECT_BYTES, || false)?;
        rewritten += progress.rewritten_blocks;
        cursor = progress.next_page;
        if cursor.is_none() {
            compacted = true;
            break;
        }
    }
    assert!(compacted);
    assert_eq!(
        rewritten, 1,
        "only the remaining block of the old pack is rewritten"
    );
    after = None;
    for _ in 0..16 {
        let p = disk.collect_offline(after.as_deref(), 1000, || false)?;
        if p.complete {
            break;
        }
        after = p.next_after;
    }
    let compacted_objects = store.list_chunks(&id, 1000)?;
    assert_eq!(compacted_objects.len(), 3);
    let sizes = compacted_objects
        .iter()
        .map(|h| store.chunk(&id, h).map(|b| b.len()))
        .collect::<Result<Vec<_>, _>>()?;
    assert!(
        sizes.contains(&CHUNK_BYTES),
        "sparse pack is replaced by a single block"
    );
    assert!(
        !sizes.contains(&MAX_OBJECT_BYTES),
        "old full pack is reclaimed"
    );
    let before = disk.status()?;
    drop(disk);
    let mut disk = OwnedDisk::open(store.clone(), &id, &directory)?;
    let after_status = disk.status()?;
    assert_eq!(before.local_sequence, after_status.local_sequence);
    assert_eq!(before.remote_sequence, after_status.remote_sequence);
    for i in 0..16 {
        let mut bytes = vec![0; CHUNK_BYTES];
        disk.read((i * CHUNK_BYTES) as u64, &mut bytes)?;
        assert!(bytes
            .iter()
            .all(|b| *b == if i < 15 { i as u8 + 33 } else { 16 }));
    }
    drop(disk);
    OwnedDisk::discard_retired_local(&id, &directory)?;
    OwnedDisk::retire(store.clone(), &id, &directory)?;
    let mut done = false;
    for _ in 0..16 {
        if reclaim::sweep(store.as_ref(), &id, 1000)?.complete {
            done = true;
            break;
        }
    }
    assert!(done);
    let keys = (0..1000).map(|i| format!("{i:064x}")).collect::<Vec<_>>();
    let bulk = Instant::now();
    store.delete_chunks(&id, &keys)?;
    assert!(store.list_chunks(&id, 1000)?.is_empty());
    fs::remove_dir_all(directory)?;
    println!("PASS in {:.2}s: 16 blocks -> 1 pack, partial-pack retention, compaction/reopen data intact; 1,000-key S3 delete in {:.2}s; retirement marker retained", start.elapsed().as_secs_f64(), bulk.elapsed().as_secs_f64());
    Ok(())
}
