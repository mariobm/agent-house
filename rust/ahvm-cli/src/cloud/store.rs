use super::{Credentials, StoreKind};
use crate::Result;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
pub struct Metadata {
    pub endpoint: String,
    pub kind: StoreKind,
}
pub struct Store {
    dir: PathBuf,
    _lock: fs::File,
}
impl Store {
    pub fn open() -> Result<Self> {
        let base = match std::env::var_os("AHVM_CONFIG_DIR") {
            Some(path) => PathBuf::from(path),
            None => {
                PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unset")?).join(".config/ahvm")
            }
        };
        Self::at(base.join("cloud"))
    }
    fn at(dir: PathBuf) -> Result<Self> {
        if !dir.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(&dir)?;
        }
        private(&dir)?;
        let path = dir.join("lock");
        if path.symlink_metadata().is_ok() {
            private(&path)?;
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(path)?;
        lock.try_lock_exclusive()
            .map_err(|_| "another AHVM cloud command is running; retry when it finishes")?;
        Ok(Self { dir, _lock: lock })
    }
    pub fn metadata(&self) -> Result<Option<Metadata>> {
        let path = self.dir.join("login.json");
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&read(&path)?)?))
    }
    fn entry(&self, metadata: &Metadata) -> Result<keyring::Entry> {
        let account = format!(
            "{:x}",
            Sha256::digest(format!(
                "{}:{}",
                self.dir.canonicalize()?.display(),
                metadata.endpoint
            ))
        );
        keyring::Entry::new("app.ahvm.cli", &account)
            .map_err(|_| "could not open OS credential store".into())
    }
    pub fn load(&self, metadata: &Metadata) -> Result<Credentials> {
        let bytes = match metadata.kind {
            StoreKind::Keyring => self.entry(metadata)?.get_password()
                .map_err(|_| "cannot read cloud credentials from OS credential store; unlock it or log in again")?.into_bytes(),
            StoreKind::File => read(&self.dir.join("credentials.json"))?,
        };
        // Never propagate parse errors that might include secret field contents.
        serde_json::from_slice(&bytes)
            .map_err(|_| "invalid cloud credential record; log in again".into())
    }
    pub fn save(&self, metadata: &Metadata, credentials: &Credentials) -> Result<()> {
        let data = serde_json::to_string(credentials)?;
        match metadata.kind {
            StoreKind::Keyring => self.entry(metadata)?.set_password(&data)
                .map_err(|_| "could not save to OS credential store; unlock it, or retry with --credential-store file")?,
            StoreKind::File => atomic(&self.dir, "credentials.json", data.as_bytes())?,
        }
        atomic(&self.dir, "login.json", &serde_json::to_vec(metadata)?)
    }
    pub fn clear(&self, metadata: &Metadata) -> Result<()> {
        match metadata.kind {
            StoreKind::Keyring => match self.entry(metadata)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => {}
                Err(_) => {
                    return Err("cloud login revoked, but OS credential removal failed".into())
                }
            },
            StoreKind::File => {
                fs::remove_file(self.dir.join("credentials.json"))?;
            }
        }
        fs::remove_file(self.dir.join("login.json"))?;
        Ok(())
    }
}
fn private(path: &Path) -> Result<()> {
    let meta = path.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        return Err("cloud credential paths must not be symlinks".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(
                "cloud credential directory/files must be private (directory 700, files 600)"
                    .into(),
            );
        }
    }
    Ok(())
}
fn read(path: &Path) -> Result<Vec<u8>> {
    private(path)?;
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(16385).read_to_end(&mut bytes)?;
    if bytes.len() > 16384 {
        return Err("cloud credential record is too large".into());
    }
    Ok(bytes)
}
fn atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(dir.join(name))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fallback_roundtrip_lock_and_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("cloud");
        let store = Store::at(dir.clone()).unwrap();
        assert!(Store::at(dir.clone()).is_err());
        let metadata = Metadata {
            endpoint: "https://dashboard.ahvm.app".into(),
            kind: StoreKind::File,
        };
        let creds = Credentials {
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
            expires_at: 1,
        };
        store.save(&metadata, &creds).unwrap();
        assert_eq!(store.load(&metadata).unwrap().refresh_token, "test-refresh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.join("credentials.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        store.clear(&metadata).unwrap();
        assert!(store.metadata().unwrap().is_none());
    }
    #[cfg(unix)]
    #[test]
    fn refuses_symlink_or_public_directory() {
        use std::os::unix::{fs::symlink, fs::PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("cloud");
        symlink(temp.path(), &dir).unwrap();
        assert!(Store::at(dir.clone()).is_err());
        fs::remove_file(&dir).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Store::at(dir).is_err());
    }
}
