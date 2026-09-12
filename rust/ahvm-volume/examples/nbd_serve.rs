//! Single-volume experimental Unix-socket server. Not installed with AHVM.
#[cfg(unix)]
fn main() {
    use ahvm_volume::{
        nbd::{self, Disk},
        s3::{Config, S3Store},
        Result, Volume, MAX_VOLUME_BYTES,
    };
    use std::{
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::Path,
        sync::Arc,
    };
    #[derive(Debug)]
    struct Logged(Volume);
    impl Disk for Logged {
        fn size(&self) -> u64 {
            self.0.size()
        }
        fn read(&mut self, at: u64, out: &mut [u8]) -> Result<()> {
            self.0.read(at, out)
        }
        fn write(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
            self.0.write(at, bytes)
        }
        fn flush(&mut self) -> Result<()> {
            let result = self.0.commit();
            match &result {
                Ok(generation) => eprintln!("flush committed generation={generation}"),
                Err(_) => eprintln!("flush failed"),
            }
            result.map(|_| ())
        }
    }
    let run = || -> std::result::Result<(), Box<dyn std::error::Error>> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if args.len() != 4 || !["create", "open"].contains(&args[3].as_str()) {
            return Err(
                "Usage: nbd_serve PRIVATE_CONFIG VOLUME_ID PRIVATE_SOCKET create|open".into(),
            );
        }
        let socket = Path::new(&args[2]);
        let parent = socket.parent().ok_or("socket needs private parent")?;
        if std::fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
            return Err("socket directory must be private".into());
        }
        // Bind first; never remove or replace another process's socket.
        let listener = UnixListener::bind(socket)?;
        let store = Arc::new(S3Store::new(Config::from_file(Path::new(&args[0]))?)?);
        let volume = if args[3] == "create" {
            Volume::create(store, &args[1], MAX_VOLUME_BYTES)?
        } else {
            Volume::open(store, &args[1])?
        };
        eprintln!("ready: bounded experimental NBD export");
        let (mut stream, _) = listener.accept()?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(30)))?;
        nbd::serve(&mut stream, &mut Logged(volume))?;
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("nbd experiment: {e}");
        std::process::exit(1);
    }
}
#[cfg(not(unix))]
fn main() {
    eprintln!("NBD experiment requires Unix");
    std::process::exit(1);
}
