//! Root-only portable export. No writes to the source disk or remote namespace.
use super::*;
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

struct Sparse(File);
impl Write for Sparse {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.iter().all(|b| *b == 0) {
            self.0.seek(SeekFrom::Current(bytes.len() as i64))?;
            Ok(bytes.len())
        } else {
            self.0.write(bytes)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

pub(super) fn client(args: &[std::ffi::OsString]) -> Result<()> {
    if args.len() != 3 || !rustix::process::geteuid().is_root() {
        return Err(
            "usage (root): ahvm-volumed export-remote S3.json VOLUME OUTPUT_DIRECTORY".into(),
        );
    }
    let id = args[1].to_str().ok_or("invalid volume ID")?;
    if !crate::valid_id(id) {
        return Err("invalid volume ID".into());
    }
    let destination = Path::new(&args[2]);
    if !destination.is_absolute() || destination.file_name().is_none() {
        return Err("output must be a new absolute directory".into());
    }
    let parent = destination.parent().ok_or("missing output parent")?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
        || parent.canonicalize()? != parent
    {
        return Err("output parent must be a canonical root-owned private directory".into());
    }
    let store = Arc::new(S3Store::new(S3Config::from_file(Path::new(&args[0]))?)?);
    // Reuse the bounded cache/read-ahead used by disk workers. In particular,
    // do not fetch the same metadata page for every exported 64-KiB block.
    let store = Arc::new(crate::cache::CachedStore::new(store, 16 * 1024 * 1024)?);
    // create_dir fails if the target already exists, including a symlink.
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(destination)?;
    let result = (|| -> Result<()> {
        let mut file = Sparse(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(destination.join("disk.raw"))?,
        );
        let report = crate::owned::export_remote(store, id, &mut file)?;
        file.0.set_len(report.logical_bytes)?;
        file.0.sync_all()?;
        let mut manifest = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination.join("complete.tmp"))?;
        serde_json::to_writer(&mut manifest, &report)?;
        manifest.sync_all()?;
        fs::rename(
            destination.join("complete.tmp"),
            destination.join("complete.json"),
        )?;
        File::open(destination)?.sync_all()?;
        File::open(parent)?.sync_all()?;
        println!("{}", serde_json::to_string(&report)?);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination.join("complete.json"));
        // Keep partial disk bytes for explicit operator cleanup, never report success.
    }
    result
}
