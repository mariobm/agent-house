//! Import a raw image or serve an existing format-2 disk on a private Unix socket.
#[cfg(unix)]
#[path = "support/metrics.rs"]
mod metrics;
#[cfg(unix)]
fn main() {
    use ahvm_volume::{
        batched::BatchedDisk,
        cache::CachedStore,
        indexed::IndexedVolume,
        local::LocalDisk,
        nbd,
        s3::{Config, S3Store},
    };
    use std::{
        io::{Read, Write},
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
        path::Path,
        sync::Arc,
        time::Duration,
    };
    let run = || -> Result<(), Box<dyn std::error::Error>> {
        let a: Vec<_> = std::env::args().skip(1).collect();
        if a.first().map(String::as_str) == Some("sync") && a.len() == 2 {
            let mut socket = UnixStream::connect(&a[1])?;
            socket.set_read_timeout(Some(Duration::from_secs(300)))?;
            socket.write_all(b"sync\n")?;
            let mut response = String::new();
            socket.take(4096).read_to_string(&mut response)?;
            let json: serde_json::Value = serde_json::from_str(&response)?;
            if json.get("error").is_some() {
                return Err("remote sync failed".into());
            }
            println!("{response}");
            return Ok(());
        }
        if !matches!(
            a.first().map(String::as_str),
            Some("import" | "serve" | "serve-strict")
        ) || a.len() != if a[0] == "serve" { 5 } else { 4 }
        {
            return Err("Usage: indexed_nbd import|serve-strict CONFIG ID IMAGE|SOCKET; serve CONFIG ID SOCKET JOURNAL_DIR; sync CONTROL_SOCKET".into());
        }
        let store = Arc::new(CachedStore::new(
            metrics::Measured::wrap(Arc::new(S3Store::with_timeout(
                Config::from_file(Path::new(&a[1]))?,
                Duration::from_secs(3),
            )?)),
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
            if a[0] == "serve" {
                let mut disk = LocalDisk::open(store, &a[2], Path::new(&a[4]))?;
                let control = UnixListener::bind(socket.with_extension("control"))?;
                let handle = disk.clone();
                std::thread::spawn(move || {
                    for client in control.incoming() {
                        let Ok(mut client) = client else { break };
                        let _ = client.set_read_timeout(Some(Duration::from_secs(5)));
                        let _ = client.set_write_timeout(Some(Duration::from_secs(5)));
                        let mut command = Vec::new();
                        use std::io::BufRead;
                        if std::io::BufReader::new(&mut client)
                            .take(32)
                            .read_until(b'\n', &mut command)
                            .is_err()
                        {
                            continue;
                        }
                        let result = match command.as_slice() {
                            b"sync\n" => handle.sync_remote(),
                            b"status\n" => Ok(handle.status()),
                            _ => Err(ahvm_volume::Error::InvalidInput),
                        };
                        let response = match result {
                            Ok(status) => serde_json::to_string(&status).unwrap(),
                            Err(_) => "{\"error\":\"remote sync failed\"}".into(),
                        };
                        let _ = client.write_all(response.as_bytes());
                    }
                });
                let _replication = disk.background();
                eprintln!("ready: local fsync with eventual R2 replication");
                let (mut stream, _) = listener.accept()?;
                stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                nbd::serve(&mut stream, &mut disk)?;
            } else {
                let volume = IndexedVolume::open(store, &a[2])?;
                eprintln!("ready: strict remote fsync");
                let (mut stream, _) = listener.accept()?;
                stream.set_write_timeout(Some(Duration::from_secs(30)))?;
                nbd::serve(&mut stream, &mut BatchedDisk::new(volume))?;
            }
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
