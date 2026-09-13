//! Ownership-aware worker for the isolated host-service qualification.
//! The supervisor owns NBD attachment and VM fencing. Dropping this process
//! retains remote ownership; a restart must use the original private directory.
#[cfg(unix)]
fn main() {
    if let Err(error) = run() {
        eprintln!("owned worker: {error}");
        std::process::exit(1);
    }
}
#[cfg(unix)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use ahvm_volume::{
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
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err("usage: owned_nbd enroll CONFIG ID | serve CONFIG ID SOCKET OWNER_DIR".into());
    }
    let store = Arc::new(CachedStore::new(
        Arc::new(S3Store::with_timeout(
            Config::from_file(Path::new(&args[1]))?,
            Duration::from_secs(3),
        )?),
        64 * 1024 * 1024,
    )?);
    if args[0] == "enroll" && args.len() == 3 {
        OwnedDisk::enroll(store, &args[2])?;
        return Ok(());
    }
    if args[0] != "serve" || args.len() != 5 {
        return Err("invalid arguments".into());
    }
    let socket = Path::new(&args[3]);
    let parent = socket.parent().ok_or("private directory required")?;
    if std::fs::symlink_metadata(parent)?.permissions().mode() & 0o077 != 0 {
        return Err("private directory required".into());
    }
    // Acquire and recover before advertising either socket.
    let mut disk = OwnedDisk::open(store, &args[2], Path::new(&args[4]))?;
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
                _ => Err(ahvm_volume::Error::InvalidInput),
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
#[cfg(not(unix))]
fn main() {
    eprintln!("requires Unix");
    std::process::exit(1);
}
