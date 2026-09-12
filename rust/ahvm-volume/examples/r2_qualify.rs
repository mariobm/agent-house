//! Explicit opt-in qualification. Writes only to the supplied fresh volume ID.
//! Run without arguments for usage. No credentials are printed.
use ahvm_volume::{
    s3::{Config, S3Store},
    Error, Head, ObjectStore, Result, Volume, CHUNK_BYTES,
};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[derive(Debug)]
struct Faults {
    store: Arc<S3Store>,
    fail_chunk: AtomicUsize,
    lose_reply: bool,
}
impl ObjectStore for Faults {
    fn head(&self, id: &str) -> Result<Option<Head>> {
        self.store.head(id)
    }
    fn chunk(&self, id: &str, hash: &str) -> Result<Vec<u8>> {
        self.store.chunk(id, hash)
    }
    fn put_chunk(&self, id: &str, hash: &str, bytes: &[u8]) -> Result<()> {
        if self.fail_chunk.fetch_add(1, Ordering::SeqCst) == 1 {
            return Err(Error::Store);
        }
        self.store.put_chunk(id, hash, bytes)
    }
    fn publish(&self, id: &str, previous: Option<&str>, manifest: &[u8]) -> Result<String> {
        let revision = self.store.publish(id, previous, manifest)?;
        if self.lose_reply {
            return Err(Error::Uncertain);
        }
        Ok(revision)
    }
}
fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(Error::Corrupt)
    }
}
fn child(config: &str, id: &str, mode: &str) -> Result<()> {
    let status = Command::new(std::env::current_exe().map_err(|_| Error::Store)?)
        .args([config, id, mode])
        .status()
        .map_err(|_| Error::Store)?;
    require(status.success())
}
fn run(args: &[String]) -> Result<()> {
    let [file, id, mode] = args else {
        return Err(Error::InvalidInput);
    };
    let store = Arc::new(S3Store::new(Config::from_file(Path::new(file))?)?);
    match mode.as_str() {
        "seed" => {
            let mut volume = Volume::create(store, id, (CHUNK_BYTES * 4) as u64)?;
            volume.write(CHUNK_BYTES as u64 - 2, b"persistent")?;
            volume.commit()?;
        }
        "verify" => {
            let volume = Volume::open(store, id)?;
            let mut bytes = [0; 10];
            volume.read(CHUNK_BYTES as u64 - 2, &mut bytes)?;
            require(&bytes == b"persistent")?;
        }
        "contender" => {
            let mut volume = Volume::open(store, id)?;
            println!("READY");
            std::io::stdout().flush().map_err(|_| Error::Store)?;
            let mut gate = String::new();
            std::io::stdin()
                .read_line(&mut gate)
                .map_err(|_| Error::Store)?;
            require(gate.trim() == "GO")?;
            volume.write(0, &std::process::id().to_le_bytes())?;
            match volume.commit() {
                Ok(_) => println!("WIN"),
                Err(Error::Conflict) => println!("CONFLICT"),
                Err(e) => return Err(e),
            }
        }
        "qualify" => {
            // Seed process exits completely before the independent reader starts.
            child(file, id, "seed")?;
            child(file, id, "verify")?;
            require(matches!(
                Volume::create(store.clone(), id, (CHUNK_BYTES * 4) as u64),
                Err(Error::Conflict)
            ))?;
            println!("PASS fresh-process recovery and create-if-absent");
            let prior = store.head(id)?.ok_or(Error::NotFound)?;
            let faults = Arc::new(Faults {
                store: store.clone(),
                fail_chunk: AtomicUsize::new(0),
                lose_reply: false,
            });
            let mut volume = Volume::open(faults, id)?;
            volume.write((CHUNK_BYTES * 2) as u64, &vec![9; CHUNK_BYTES * 2])?;
            require(matches!(volume.commit(), Err(Error::Store)))?;
            require(store.head(id)?.ok_or(Error::NotFound)?.revision == prior.revision)?;
            child(file, id, "verify")?;
            // Retry verifies the immutable object left by the interrupted upload.
            volume.commit()?;
            println!("PASS partial upload, prior-head preservation and immutable retry");
            let mut workers = Vec::new();
            for _ in 0..2 {
                let mut worker = Command::new(std::env::current_exe().map_err(|_| Error::Store)?)
                    .args([file, id, "contender"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .map_err(|_| Error::Store)?;
                let mut reader = BufReader::new(worker.stdout.take().ok_or(Error::Store)?);
                let mut line = String::new();
                reader.read_line(&mut line).map_err(|_| Error::Store)?;
                require(line.trim() == "READY")?;
                workers.push((worker, reader));
            }
            for (worker, _) in &mut workers {
                worker
                    .stdin
                    .take()
                    .ok_or(Error::Store)?
                    .write_all(b"GO\n")
                    .map_err(|_| Error::Store)?;
            }
            let mut results = Vec::new();
            for (mut worker, mut reader) in workers {
                let mut line = String::new();
                reader.read_line(&mut line).map_err(|_| Error::Store)?;
                require(worker.wait().map_err(|_| Error::Store)?.success())?;
                results.push(line.trim().to_owned());
            }
            results.sort();
            require(results == ["CONFLICT", "WIN"])?;
            println!("PASS simultaneous independent-process compare-and-swap");
            let faults = Arc::new(Faults {
                store: store.clone(),
                fail_chunk: AtomicUsize::new(2),
                lose_reply: true,
            });
            let mut volume = Volume::open(faults, id)?;
            volume.write(0, b"lost-reply")?;
            require(matches!(volume.commit(), Err(Error::Uncertain)))?;
            require(matches!(volume.commit(), Err(Error::ReopenRequired)))?;
            child(file, id, "verify-lost")?;
            println!("PASS reconciliation after injected lost publication reply");
            println!("Qualification passed. Fixture objects remain under this volume ID for explicit cleanup.");
        }
        "verify-lost" => {
            let volume = Volume::open(store, id)?;
            let mut bytes = [0; 10];
            volume.read(0, &mut bytes)?;
            require(&bytes == b"lost-reply")?;
        }
        _ => return Err(Error::InvalidInput),
    }
    Ok(())
}
fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!("Usage: r2_qualify PRIVATE_CONFIG FRESH_VOLUME_ID qualify|seed|verify|verify-lost\nExperimental protocol only; does not create a VM. Config must use a dedicated private test bucket.");
        std::process::exit(2);
    }
    if let Err(e) = run(&args) {
        eprintln!("Qualification failed: {e}");
        std::process::exit(1);
    }
}
