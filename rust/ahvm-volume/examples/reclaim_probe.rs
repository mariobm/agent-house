//! Small real-S3 qualification. Uses fresh random IDs; no VMs or existing volumes.
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
        .ok_or("usage: reclaim_probe CONFIG.json")?;
    let store: Arc<dyn ObjectStore> = Arc::new(S3Store::new(Config::from_file(config.as_ref())?)?);
    let mut ids = Vec::new();
    for _ in 0..2 {
        let mut bytes = [0; 32];
        fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        ids.push(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
    println!("qualification IDs: {} {}", ids[0], ids[1]);
    let root = std::env::temp_dir().join(format!("ahvm-reclaim-probe-{}", std::process::id()));
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let start = Instant::now();
    for id in &ids {
        let mut image = IndexedVolume::create(store.clone(), id, CHUNK_BYTES as u64)?;
        image.write(0, &vec![7; CHUNK_BYTES])?;
        image.commit()?;
        OwnedDisk::enroll(store.clone(), id)?;
        fs::create_dir(root.join(id))?;
        fs::set_permissions(root.join(id), fs::Permissions::from_mode(0o700))?;
    }
    let mut peer = OwnedDisk::open(store.clone(), &ids[1], &root.join(&ids[1]))?;
    assert!(OwnedDisk::retire(store.clone(), &ids[1], &root.join(&ids[1])).is_err());
    OwnedDisk::retire(store.clone(), &ids[0], &root.join(&ids[0]))?;
    let first = reclaim::sweep(store.as_ref(), &ids[0], 1)?;
    assert_eq!(first.deleted_objects, 1);
    assert!(!first.complete);
    // Rebuild transport as if the cleaner process restarted after a partial pass.
    let reopened = S3Store::new(Config::from_file(config.as_ref())?)?;
    let mut complete = false;
    for _ in 0..16 {
        if reclaim::sweep(&reopened, &ids[0], 1)?.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    assert!(OwnedDisk::open(store.clone(), &ids[0], &root.join(&ids[0])).is_err());
    let mut byte = [0];
    peer.read(0, &mut byte)?;
    assert_eq!(byte, [7]);
    drop(peer);
    OwnedDisk::retire(store.clone(), &ids[1], &root.join(&ids[1]))?;
    complete = false;
    for _ in 0..16 {
        if reclaim::sweep(store.as_ref(), &ids[1], 1)?.complete {
            complete = true;
            break;
        }
    }
    assert!(complete);
    for id in &ids {
        assert!(store.list_chunks(id, 1)?.is_empty());
    }
    fs::remove_dir_all(root)?;
    println!(
        "PASS in {:.2}s; both chunk prefixes empty; two retirement markers retained",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
