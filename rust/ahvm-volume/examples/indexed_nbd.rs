//! Import a raw image or serve an existing format-2 disk on a private Unix socket.
#[cfg(unix)]
fn main() {
    use ahvm_volume::{
        batched::BatchedDisk,
        cache::CachedStore,
        indexed::IndexedVolume,
        nbd,
        s3::{Config, S3Store},
    };
    use std::{
        io::Read,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::Path,
        sync::Arc,
        time::Duration,
    };
    let run = || -> Result<(), Box<dyn std::error::Error>> {
        let a: Vec<_> = std::env::args().skip(1).collect();
        if a.len() != 4 || !["import", "serve"].contains(&a[0].as_str()) {
            return Err(
                "Usage: indexed_nbd import|serve PRIVATE_CONFIG VOLUME_ID RAW_IMAGE|PRIVATE_SOCKET"
                    .into(),
            );
        }
        let store = Arc::new(CachedStore::new(
            Arc::new(S3Store::with_timeout(
                Config::from_file(Path::new(&a[1]))?,
                Duration::from_secs(3),
            )?),
            64 * 1024 * 1024,
        )?);
        if a[0] == "import" {
            let mut file = std::fs::File::open(&a[3])?;
            let size = file.metadata()?.len();
            let mut volume = IndexedVolume::create_import(store, &a[2], size)?;
            let mut bytes = vec![0; 8 * 1024 * 1024];
            let mut offset = 0;
            while offset < size {
                let n = bytes.len().min((size - offset) as usize);
                file.read_exact(&mut bytes[..n])?;
                if bytes[..n].iter().any(|b| *b != 0) {
                    volume.write(offset, &bytes[..n])?;
                    volume.commit()?;
                }
                offset += n as u64;
            }
            volume.finish_import()?;
            eprintln!("import ready: {size} bytes");
        } else {
            let socket = Path::new(&a[3]);
            let parent = socket.parent().ok_or("private directory required")?;
            if std::fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
                return Err("socket directory must be private".into());
            }
            let listener = UnixListener::bind(socket)?;
            let volume = IndexedVolume::open(store, &a[2])?;
            eprintln!("ready: indexed root disk");
            let (mut stream, _) = listener.accept()?;
            stream.set_write_timeout(Some(Duration::from_secs(30)))?;
            nbd::serve(&mut stream, &mut BatchedDisk::new(volume))?;
        }
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("indexed disk: {e}");
        std::process::exit(1);
    }
}
#[cfg(not(unix))]
fn main() {
    eprintln!("requires Unix");
    std::process::exit(1);
}
