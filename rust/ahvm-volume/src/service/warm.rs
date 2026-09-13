//! Explicit operator prewarming runs inside the already bounded supervisor.
use super::*;
impl Service {
    pub(super) fn hash_image(
        file: &mut File,
        imports: &mut BTreeMap<ImageIdentity, String>,
        bytes: &mut [u8],
    ) -> Result<String> {
        use sha2::{Digest, Sha256};
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        let identity = (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        );
        if let Some(hash) = imports.get(&identity) {
            return Ok(hash.clone());
        }
        let mut hasher = Sha256::new();
        loop {
            let n = file.read(bytes)?;
            if n == 0 {
                break;
            }
            hasher.update(&bytes[..n]);
        }
        let hash = format!("{:x}", hasher.finalize());
        if imports.len() >= 64 {
            imports.clear();
        }
        imports.insert(identity, hash.clone());
        Ok(hash)
    }
    pub(super) fn warm_base(&self, image: &Path) -> Result<serde_json::Value> {
        let image = image.canonicalize()?;
        if !self
            .config
            .image_roots
            .iter()
            .any(|root| image.starts_with(root))
        {
            return Err("image outside configured roots".into());
        }
        trusted_image_path(&image)?;
        let mut file = File::open(&image)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        if !metadata.is_file()
            || size == 0
            || size > self.config.limits.max_volume_bytes
            || size > crate::indexed::MAX_SIZE
            || !size.is_multiple_of(crate::CHUNK_BYTES as u64)
        {
            return Err("invalid base image size/type".into());
        }
        let mut imports = self.imports.lock().map_err(|_| "import lock poisoned")?;
        let mut bytes = vec![0; crate::indexed::WRITE_LIMIT];
        let hash = Self::hash_image(&mut file, &mut imports, &mut bytes)?;
        let reference = self.import_base(&mut file, &hash, size, &mut bytes)?;
        Ok(serde_json::json!({"ok":true,"base":reference,"logical_bytes":size}))
    }
}
pub(super) fn client(args: &[std::ffi::OsString]) -> Result<()> {
    if args.len() != 2 || !rustix::process::geteuid().is_root() {
        return Err("usage (root): ahvm-volumed warm CONFIG.json IMAGE.ext4".into());
    }
    let config: Config = read(Path::new(&args[0]))?;
    let socket = config
        .socket_dir
        .as_ref()
        .unwrap_or(&config.root)
        .join("service.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut conn = loop {
        match UnixStream::connect(&socket) {
            Ok(conn) => break conn,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(50))
            }
            Err(error) => return Err(error.into()),
        }
    };
    if rustix::net::sockopt::socket_peercred(&conn)?.uid.as_raw() != 0 {
        return Err("volume supervisor must run as root".into());
    }
    conn.set_write_timeout(Some(Duration::from_secs(3)))?;
    conn.set_read_timeout(Some(Duration::from_secs(3600)))?;
    let image = Path::new(&args[1]).canonicalize()?;
    writeln!(
        conn,
        "{}",
        serde_json::json!({"version":1,"operation":"warm-base","volume_id":"","sandbox_dir":"","image":image})
    )?;
    let mut line = String::new();
    io::BufReader::new(conn.take(4097)).read_line(&mut line)?;
    if line.len() > 4096 {
        return Err("oversized preparation reply".into());
    }
    let reply: serde_json::Value = serde_json::from_str(&line)?;
    if reply["ok"] != true {
        return Err("base preparation failed; inspect supervisor logs".into());
    }
    println!("{reply}");
    Ok(())
}
