//! Host-side image cache. Only verified, complete generations are published.
use crate::{
    distribution::{self, Artifact},
    Result,
};
use clap::Subcommand;
use std::{
    fs,
    path::{Path, PathBuf},
};
#[derive(Subcommand)]
pub enum Images {
    List,
    Available,
    Pull { name: String },
    Default { name: String },
}
const CATALOG: &str = "https://images.ahvm.app/catalog.json";
pub fn root() -> PathBuf {
    std::env::var_os("AHVM_IMAGE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/ahvm-images"))
}
fn valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 100
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}
fn record(root: &Path, name: &str) -> Result<Artifact> {
    if !valid(name) {
        return Err("invalid image name".into());
    }
    Ok(serde_json::from_slice(&fs::read(
        root.join(format!("{name}.json")),
    )?)?)
}
pub fn run(command: Images) -> Result<i32> {
    let root = root();
    match command {
        Images::Available => {
            let catalog = distribution::catalog(CATALOG)?;
            println!("{}", serde_json::to_string_pretty(&catalog.images)?);
        }
        Images::List => {
            let mut records = std::collections::BTreeMap::new();
            if root.exists() {
                for entry in fs::read_dir(&root)? {
                    let path = entry?.path();
                    if path.extension().is_some_and(|e| e == "json") {
                        let name = path.file_stem().unwrap().to_string_lossy();
                        records.insert(name.to_string(), record(&root, &name)?);
                    }
                }
            }
            println!("{}", serde_json::to_string_pretty(&records)?);
        }
        Images::Pull { name } => {
            if !valid(&name) {
                return Err("invalid image name".into());
            }
            let catalog = distribution::catalog(CATALOG)?;
            let artifact = catalog
                .images
                .get(&name)
                .ok_or("image is not in the published catalog")?;
            if artifact.guest_abi != 1 {
                return Err("image guest-agent ABI is not supported by this runtime".into());
            }
            let new_directory = !root.exists();
            fs::create_dir_all(&root)?;
            #[cfg(unix)]
            if new_directory {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
            }
            let lock = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(root.join(".lock"))?;
            fs2::FileExt::lock_exclusive(&lock)?;
            let destination = root.join(format!("{}.ext4", artifact.sha256));
            if !destination.exists() {
                let mut compressed = tempfile::NamedTempFile::new_in(&root)?;
                eprintln!("Downloading {name} {}", artifact.version);
                distribution::download(artifact, compressed.as_file_mut())?;
                let mut unpacked = tempfile::NamedTempFile::new_in(&root)?;
                distribution::unpack_gzip(
                    compressed.path(),
                    unpacked.as_file_mut(),
                    artifact.unpacked_size,
                )?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    unpacked
                        .as_file()
                        .set_permissions(fs::Permissions::from_mode(0o644))?;
                }
                unpacked.persist_noclobber(&destination)?;
            }
            let mut record = tempfile::NamedTempFile::new_in(&root)?;
            serde_json::to_writer_pretty(&mut record, artifact)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                record
                    .as_file()
                    .set_permissions(fs::Permissions::from_mode(0o644))?;
            }
            record.as_file().sync_all()?;
            record.persist(root.join(format!("{name}.json")))?;
            if !root.join("default.ext4").exists() {
                set_default(&root, &name)?;
            }
            println!("{name}\t{}", artifact.version);
        }
        Images::Default { name } => {
            set_default(&root, &name)?;
            println!("Default image: {name}");
        }
    }
    Ok(0)
}
fn set_default(root: &Path, name: &str) -> Result<()> {
    let artifact = record(root, name)?;
    if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid cached image digest".into());
    }
    let target = root.join(format!("{}.ext4", artifact.sha256));
    if !target.is_file() {
        return Err("image is not downloaded".into());
    }
    #[cfg(unix)]
    {
        let dir = tempfile::tempdir_in(root)?;
        let link = dir.path().join("default");
        std::os::unix::fs::symlink(&target, &link)?;
        fs::rename(link, root.join("default.ext4"))?;
    }
    #[cfg(not(unix))]
    {
        return Err("image installation requires a Unix host".into());
    }
    Ok(())
}
