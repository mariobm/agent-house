//! Independent-process R2 ownership qualification. Uses only a disposable volume.
#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ahvm_volume::{
        indexed::IndexedVolume,
        nbd::Disk,
        owned::OwnedDisk,
        s3::{Config, S3Store},
        Error,
    };
    use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc, time::Duration};
    let a: Vec<_> = std::env::args().skip(1).collect();
    if a.len() != 4 {
        return Err("usage: ownership_probe CONFIG VOLUME PRIVATE_DIR init|hold|resume-release|verify-release|blocked".into());
    }
    let raw = Arc::new(S3Store::with_timeout(
        Config::from_file(Path::new(&a[0]))?,
        Duration::from_secs(3),
    )?);
    let dir = Path::new(&a[2]);
    if !dir.exists() {
        std::fs::create_dir(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if a[3] == "init" {
        IndexedVolume::create(raw.clone(), &a[1], 1024 * 1024)?;
        OwnedDisk::enroll(raw, &a[1])?;
        println!("ENROLLED");
        return Ok(());
    }
    if a[3] == "blocked" {
        if !matches!(OwnedDisk::open(raw, &a[1], dir), Err(Error::Conflict)) {
            return Err("competing/stale owner was not refused".into());
        }
        println!("OWNER-BLOCKED");
        return Ok(());
    }
    let mut disk = OwnedDisk::open(raw, &a[1], dir)?;
    let marker = b"local-journal-survives-owner-process-kill";
    if a[3] == "hold" {
        disk.write(65536, marker)?;
        disk.flush()?;
        assert!(disk.status()?.pending_bytes > 0);
        println!("LOCAL-DURABLE-READY");
        use std::io::Write;
        std::io::stdout().flush()?;
        loop {
            std::thread::park()
        }
    }
    if !matches!(a[3].as_str(), "resume-release" | "verify-release") {
        return Err("unknown operation".into());
    }
    let mut bytes = vec![0; marker.len()];
    disk.read(65536, &mut bytes)?;
    assert_eq!(bytes, marker);
    disk.release()?;
    assert!(disk.write(0, b"stale").is_err());
    assert!(disk.flush().is_err());
    println!("VERIFIED-DRAINED-RELEASED");
    Ok(())
}
#[cfg(not(unix))]
fn main() {
    panic!("Unix required")
}
