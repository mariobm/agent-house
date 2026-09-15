pub(super) fn run(args: &[std::ffi::OsString]) -> super::Result<()> {
    use crate::{
        cache::CachedStore,
        nbd,
        owned::OwnedDisk,
        s3::{Config, S3Store},
    };
    use std::{
        io::{BufRead, Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::Path,
        sync::Arc,
        time::Duration,
    };
    if args.len() != 7 {
        return Err("invalid worker arguments".into());
    }
    let mut go = [0];
    std::io::stdin().read_exact(&mut go)?;
    if go != *b"G" {
        return Err("launch cancelled".into());
    }
    let mut cache = CachedStore::new(
        Arc::new(S3Store::with_timeout(
            Config::from_file(Path::new(&args[0]))?,
            Duration::from_secs(3),
        )?),
        super::accounting::CACHE_BYTES as usize,
    )?
    .with_metadata_cache(Path::new(&args[6]))?;
    // Host images are an optional verified read cache; remote-only recovery must
    // still work if the image has been removed from this host.
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&args[5])
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        if meta.is_file() && meta.uid() == 0 && meta.mode() & 0o022 == 0 {
            cache = cache
                .with_local_base(args[4].to_str().ok_or("invalid image hash")?.into(), file)?;
        }
    }
    let store = Arc::new(cache);
    let socket_path = std::path::PathBuf::from(&args[3]);
    let socket = socket_path.as_path();
    let parent = socket.parent().ok_or("private directory required")?;
    if std::fs::symlink_metadata(parent)?.permissions().mode() & 0o077 != 0 {
        return Err("private directory required".into());
    }
    // Acquire and recover before advertising either socket.
    let mut disk = OwnedDisk::open(
        store,
        args[1].to_str().ok_or("invalid volume id")?,
        &Path::new(&args[2]).join("owner"),
    )?;
    let listener = UnixListener::bind(socket)?;
    let control = UnixListener::bind(socket.with_extension("control"))?;
    let handle = disk.clone();
    std::thread::spawn(move || {
        for client in control.incoming() {
            let Ok(mut client) = client else { break };
            let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
            let _ = client.set_write_timeout(Some(Duration::from_secs(5)));
            let mut command = Vec::new();
            if std::io::BufReader::new(&mut client)
                .take(32)
                .read_until(b'\n', &mut command)
                .is_err()
            {
                continue;
            }
            let result = match command.as_slice() {
                b"sync\n" => handle.sync_remote(),
                b"status\n" => handle.status(),
                _ => Err(crate::Error::InvalidInput),
            };
            let response = match result {
                Ok(status) => serde_json::to_vec(&status).unwrap(),
                Err(_) => b"{\"error\":\"storage operation failed\"}".to_vec(),
            };
            let _ = client.write_all(&response);
        }
    });
    let _replication = disk.background();
    let (mut stream, _) = listener.accept()?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    nbd::serve(&mut stream, &mut disk)?;
    Ok(())
}
