//! Opt-in cgroup v2 limits. The host delegates a subtree; guests never see it.
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Host-owned delegated cgroup root, outside the daemon's own leaf cgroup.
#[derive(Debug, Clone)]
pub struct ResourceConfig {
    pub root: PathBuf,
}

impl ResourceConfig {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if !cfg!(target_os = "linux") {
            return Err(io::Error::other(
                "VM resource limits require Linux cgroup v2",
            ));
        }
        let root = fs::canonicalize(&self.root)?;
        if !root.starts_with("/sys/fs/cgroup") || root == Path::new("/sys/fs/cgroup") {
            return Err(io::Error::other(
                "resource root must be a delegated cgroup subtree",
            ));
        }
        let controllers = fs::read_to_string(root.join("cgroup.controllers"))?;
        for required in ["cpu", "memory", "pids"] {
            if !controllers.split_whitespace().any(|s| s == required) {
                return Err(io::Error::other(format!(
                    "missing delegated {required} controller"
                )));
            }
        }
        // Fails if the daemon was incorrectly placed in the subtree root.
        fs::write(root.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
        // Empty groups left by an interrupted failed create are disposable.
        // rmdir is atomic and refuses populated groups; never kill here.
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("vm-") && entry.file_type()?.is_dir()
            {
                let _ = fs::remove_dir(entry.path());
            }
        }
        Ok(())
    }

    pub(crate) fn path(&self, id: &str) -> io::Result<PathBuf> {
        if id.is_empty()
            || id.len() > 64
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            return Err(io::Error::other("invalid resource group id"));
        }
        Ok(self.root.join(format!("vm-{id}")))
    }

    pub(crate) fn prepare(&self, id: &str, cpus: u8, memory_mb: u32) -> io::Result<PathBuf> {
        let path = self.path(id)?;
        fs::create_dir_all(&path)?;
        let limits = limits(cpus, memory_mb)?;
        for (name, value) in limits {
            fs::write(path.join(name), value)?;
        }
        Ok(path)
    }

    pub(crate) fn remove(&self, id: &str) -> io::Result<()> {
        match fs::remove_dir(self.path(id)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e), // Populated groups must never be silently forgotten.
        }
    }
}

fn limits(cpus: u8, memory_mb: u32) -> io::Result<Vec<(&'static str, String)>> {
    if cpus == 0 || memory_mb == 0 {
        return Err(io::Error::other("VM resources must be nonzero"));
    }
    // Snapshot writes charge page cache in addition to guest RAM. Reserve that
    // second copy plus 256 MiB for the VMM and gateway, not merely guest RAM.
    let max = (u64::from(memory_mb) * 2 + 256) * 1024 * 1024;
    Ok(vec![
        ("cpu.max", format!("{} 100000", u64::from(cpus) * 100000)),
        ("memory.max", max.to_string()),
        ("memory.swap.max", "0".into()),
        ("memory.oom.group", "1".into()),
        ("pids.max", "256".into()),
    ])
}

/// Adoption is observation only: never move an arbitrary/reused PID into a
/// group. A live uncontained worker requires an explicit stop before enabling.
pub(crate) fn verify_member(group: &Path, pid: u32) -> io::Result<()> {
    let expected = fs::canonicalize(group)?;
    let actual = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let relative = actual
        .lines()
        .find_map(|s| s.strip_prefix("0::"))
        .ok_or_else(|| io::Error::other("worker has no unified cgroup"))?;
    if Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/')) != expected {
        return Err(io::Error::other(
            "live worker is outside its VM cgroup; stop it before enabling limits",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_include_snapshot_memory_and_bound_cpu_and_tasks() {
        let values = limits(2, 4096).unwrap();
        assert!(values.contains(&("cpu.max", "200000 100000".into())));
        assert!(values.contains(&("memory.max", (8448u64 * 1024 * 1024).to_string())));
        assert!(values.contains(&("pids.max", "256".into())));
        assert!(limits(0, 4096).is_err());
        assert!(limits(1, 0).is_err());
    }
    #[test]
    fn ids_cannot_escape_the_delegated_root() {
        let cfg = ResourceConfig {
            root: "/sys/fs/cgroup/example".into(),
        };
        for id in ["../escape", "/absolute", "", "a/b"] {
            assert!(cfg.path(id).is_err());
        }
        assert_eq!(cfg.path("dev").unwrap(), cfg.root.join("vm-dev"));
        assert!(ResourceConfig {
            root: "/tmp".into()
        }
        .validate()
        .is_err());
    }
}
