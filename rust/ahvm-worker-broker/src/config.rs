use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub socket: PathBuf,
    pub state_dir: PathBuf,
    pub jail_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cgroup_root: PathBuf,
    pub daemon_uid: u32,
    pub daemon_gid: u32,
    pub uid_base: u32,
    pub gid_base: u32,
    /// Lifetime launch capacity, not simultaneous VM count. Never reused.
    pub identity_count: u32,
    pub vmm_bin: PathBuf,
    pub netd_bin: Option<PathBuf>,
    pub gpu_bin: Option<PathBuf>,
    pub lib_path: String,
    #[serde(default)]
    pub image_roots: Vec<PathBuf>,
    /// Explicit NBD/render nodes. KVM/null/urandom are fixed builtin grants.
    #[serde(default)]
    pub devices: Vec<PathBuf>,
}

pub fn clean(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}
pub fn id(id: &str) -> io::Result<()> {
    if id.is_empty()
        || id.len() > 64
        || id == "."
        || id == ".."
        || id == "snapshots"
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    {
        return Err(io::Error::other("invalid worker id"));
    }
    Ok(())
}

pub fn root_owned(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        let meta = fs::symlink_metadata(ancestor)?;
        // Root-owned sticky /tmp parents cannot be renamed by other users.
        if meta.uid() != 0
            || meta.file_type().is_symlink()
            || (meta.mode() & 0o022 != 0 && !(meta.is_dir() && meta.mode() & 0o1000 != 0))
        {
            return Err(io::Error::other(format!(
                "trusted path must be root-owned and not writable by other users: {}",
                ancestor.display()
            )));
        }
    }
    Ok(())
}
pub fn private_root(path: &Path) -> io::Result<()> {
    root_owned(path)?;
    let meta = fs::metadata(path)?;
    if !meta.is_dir() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other("broker state/jail root requires mode0700"));
    }
    Ok(())
}
pub fn daemon_file(path: &Path, uid: u32) -> io::Result<Vec<u8>> {
    let file = fs::File::from(super::kernel::open(path, libc::O_RDONLY, 0)?);
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != uid
        || meta.nlink() != 1
        || meta.mode() & 0o022 != 0
        || meta.len() > 1024 * 1024
    {
        return Err(io::Error::other("spec must be a bounded daemon-owned regular file without hardlinks or other-writer access"));
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(io::Error::other("worker spec too large"));
    }
    Ok(bytes)
}

impl Config {
    pub fn load(path: &Path) -> io::Result<Self> {
        root_owned(path)?;
        let bytes = fs::read(path)?;
        if bytes.len() > 65536 {
            return Err(io::Error::other("broker config too large"));
        }
        let mut cfg: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        for path in [
            &cfg.socket,
            &cfg.state_dir,
            &cfg.jail_dir,
            &cfg.data_dir,
            &cfg.cgroup_root,
            &cfg.vmm_bin,
        ] {
            if !clean(path) {
                return Err(io::Error::other(
                    "broker paths must be absolute and normalized",
                ));
            }
        }
        if cfg.daemon_uid == 0
            || cfg.daemon_gid == 0
            || cfg.uid_base < 65536
            || cfg.gid_base < 65536
            || cfg.identity_count == 0
            || cfg.identity_count > 16_777_216
            || cfg
                .uid_base
                .checked_add(cfg.identity_count)
                .is_none_or(|end| end >= i32::MAX as u32)
            || cfg
                .gid_base
                .checked_add(cfg.identity_count)
                .is_none_or(|end| end >= i32::MAX as u32)
        {
            return Err(io::Error::other("invalid reserved worker identity range"));
        }
        for (file, base) in [("/etc/passwd", cfg.uid_base), ("/etc/group", cfg.gid_base)] {
            let content = fs::read_to_string(file)?;
            for line in content.lines() {
                if line
                    .split(':')
                    .nth(2)
                    .and_then(|s| s.parse::<u32>().ok())
                    .is_some_and(|uid| (base..base + cfg.identity_count).contains(&uid))
                {
                    return Err(io::Error::other(
                        "worker range overlaps an existing account",
                    ));
                }
            }
        }
        for (file, base) in [("/etc/subuid", cfg.uid_base), ("/etc/subgid", cfg.gid_base)] {
            let content = match fs::read_to_string(file) {
                Ok(s) => s,
                Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(e),
            };
            for line in content.lines() {
                let mut fields = line.split(':');
                let _ = fields.next();
                if let (Some(start), Some(count)) = (
                    fields.next().and_then(|s| s.parse::<u64>().ok()),
                    fields.next().and_then(|s| s.parse::<u64>().ok()),
                ) {
                    if start < u64::from(base + cfg.identity_count)
                        && start.saturating_add(count) > u64::from(base)
                    {
                        return Err(io::Error::other("worker range overlaps subordinate IDs"));
                    }
                }
            }
        }
        private_root(&cfg.state_dir)?;
        private_root(&cfg.jail_dir)?;
        root_owned(
            cfg.socket
                .parent()
                .ok_or_else(|| io::Error::other("missing socket parent"))?,
        )?;
        cfg.data_dir = cfg.data_dir.canonicalize()?;
        if fs::metadata(&cfg.data_dir)?.uid() != cfg.daemon_uid {
            return Err(io::Error::other("worker data root must belong to daemon"));
        }
        cfg.cgroup_root = cfg.cgroup_root.canonicalize()?;
        if !cfg.cgroup_root.starts_with("/sys/fs/cgroup")
            || cfg.cgroup_root == Path::new("/sys/fs/cgroup")
        {
            return Err(io::Error::other(
                "worker cgroup root must be a delegated subtree",
            ));
        }
        fn binary(binary: &mut PathBuf) -> io::Result<()> {
            *binary = binary.canonicalize()?;
            root_owned(binary)?;
            let meta = fs::metadata(binary)?;
            if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
                return Err(io::Error::other("worker binary is not an executable file"));
            }
            Ok(())
        }
        binary(&mut cfg.vmm_bin)?;
        if let Some(path) = &mut cfg.netd_bin {
            binary(path)?;
        }
        if let Some(path) = &mut cfg.gpu_bin {
            binary(path)?;
        }
        let mut libraries = Vec::new();
        for library in cfg.lib_path.split(':') {
            if library.is_empty() || !clean(Path::new(library)) {
                return Err(io::Error::other(
                    "worker library path must contain only fixed absolute roots",
                ));
            }
            let path = Path::new(library).canonicalize()?;
            root_owned(&path)?;
            // Preserve no writable/symlink alias between validation and exec.
            libraries.push(path.to_string_lossy().into_owned());
        }
        cfg.lib_path = libraries.join(":");
        for image in &mut cfg.image_roots {
            *image = image.canonicalize()?;
        }
        for device in &cfg.devices {
            let name = device
                .file_name()
                .map(|s| s.to_string_lossy())
                .unwrap_or_default();
            let digits = |prefix: &str| {
                name.strip_prefix(prefix).is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit())
                })
            };
            if !clean(device)
                || !(device.parent() == Some(Path::new("/dev/dri")) && digits("renderD")
                    || device.parent() == Some(Path::new("/dev")) && digits("nbd"))
            {
                return Err(io::Error::other(
                    "only explicit NBD and render nodes may be configured",
                ));
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_and_paths_cannot_escape() {
        for bad in ["", ".", "..", "snapshots", "a/b", "bad\n", "../../root"] {
            assert!(id(bad).is_err());
        }
        assert!(id("safe-name.1").is_ok());
        assert!(!clean(Path::new("/etc/../root")));
        assert!(!clean(Path::new("relative")));
    }
}
