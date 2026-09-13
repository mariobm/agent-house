//! Small real-S3 probe: obsolete generations, restart, current/peer reads.
use ahvm_volume::{
    indexed::IndexedVolume,
    nbd::Disk,
    owned::OwnedDisk,
    reclaim,
    s3::{Config, S3Store},
    ObjectStore, CHUNK_BYTES,
};
use std::{fs, io::Read, os::unix::fs::PermissionsExt, sync::Arc, time::Instant};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = std::env::args()
        .nth(1)
        .ok_or("usage: live_reclaim_probe CONFIG.json")?;
    let store: Arc<dyn ObjectStore> = Arc::new(S3Store::new(Config::from_file(config.as_ref())?)?);
    let mut ids = Vec::new();
    for _ in 0..2 {
        let mut bytes = [0; 32];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        ids.push(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
    println!("qualification IDs: {} {}", ids[0], ids[1]);
    let root = std::env::temp_dir().join(format!("ahvm-live-gc-{}", std::process::id()));
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    for id in &ids {
        let mut image = IndexedVolume::create(store.clone(), id, CHUNK_BYTES as u64)?;
        image.write(0, &vec![7; CHUNK_BYTES])?;
        image.commit()?;
        OwnedDisk::enroll(store.clone(), id)?;
        fs::create_dir(root.join(id))?;
        fs::set_permissions(root.join(id), fs::Permissions::from_mode(0o700))?;
    }
    let mut disk = OwnedDisk::open(store.clone(), &ids[0], &root.join(&ids[0]))?;
    disk.write(0, &vec![8; CHUNK_BYTES])?;
    disk.flush()?;
    assert!(disk.collect_offline(None, 16, || false).is_err());
    disk.sync_remote()?;
    assert_eq!(store.list_chunks(&ids[0], 128)?.len(), 4);
    let start = Instant::now();
    let first = disk.collect_offline(None, 1, || false)?;
    drop(disk);
    let mut disk = OwnedDisk::open(store.clone(), &ids[0], &root.join(&ids[0]))?;
    let mut after = first.next_after;
    let mut done = false;
    for _ in 0..16 {
        let p = disk.collect_offline(after.as_deref(), 1, || false)?;
        if p.complete {
            done = true;
            break;
        }
        after = p.next_after;
    }
    assert!(done);
    assert_eq!(store.list_chunks(&ids[0], 128)?.len(), 2);
    let mut byte = [0];
    disk.read(0, &mut byte)?;
    assert_eq!(byte, [8]);
    drop(disk);
    let mut peer = OwnedDisk::open(store.clone(), &ids[1], &root.join(&ids[1]))?;
    peer.read(0, &mut byte)?;
    assert_eq!(byte, [7]);
    drop(peer);
    println!(
        "PASS offline collection in {:.2}s; 4 objects -> 2, current/peer data intact",
        start.elapsed().as_secs_f64()
    );
    for id in &ids {
        OwnedDisk::retire(store.clone(), id, &root.join(id))?;
        let mut done = false;
        for _ in 0..16 {
            if reclaim::sweep(store.as_ref(), id, 16)?.complete {
                done = true;
                break;
            }
        }
        assert!(done);
    }
    fs::remove_dir_all(root)?;
    println!("test chunks cleaned; two permanent retirement markers retained");
    Ok(())
}
