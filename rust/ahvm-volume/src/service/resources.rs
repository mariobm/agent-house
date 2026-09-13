//! Separate, root-owned cgroups for disk workers. Never move adopted processes.
use super::*;
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Resources {
    pub root: PathBuf,
    pub memory_bytes: u64,
    pub cpu_quota_us: u64,
    pub tasks: u64,
}
fn finite(path: &Path, name: &str) -> Result<u64> {
    let value = fs::read_to_string(path.join(name))?.trim().parse::<u64>()?;
    if value == 0 {
        return Err("resource ceiling must be positive".into());
    }
    Ok(value)
}
fn cpu(path: &Path) -> Result<()> {
    let value = fs::read_to_string(path.join("cpu.max"))?;
    let mut parts = value.split_whitespace();
    if parts.next().ok_or("missing CPU limit")?.parse::<u64>()? == 0
        || parts.next().ok_or("missing CPU period")?.parse::<u64>()? == 0
    {
        return Err("invalid CPU ceiling".into());
    }
    Ok(())
}
fn member(pid: u32) -> Result<PathBuf> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("unified cgroup required")?;
    Ok(Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}
impl Resources {
    pub fn validate(&self, devices: usize) -> Result<()> {
        if self.root.canonicalize()? != self.root
            || !self.root.starts_with("/sys/fs/cgroup")
            || self.root == Path::new("/sys/fs/cgroup")
            || fs::metadata(&self.root)?.uid() != 0
            || fs::metadata(&self.root)?.permissions().mode() & 0o022 != 0
        {
            return Err("root-owned delegated storage cgroup required".into());
        }
        if self.memory_bytes < 256 * 1024 * 1024
            || !self.memory_bytes.is_multiple_of(4096)
            || self.cpu_quota_us < 1000
            || self.cpu_quota_us > 3_200_000
            || !(16..=1024).contains(&self.tasks)
        {
            return Err("invalid storage worker limits".into());
        }
        // Reserve the maximum worker footprint for every configured NBD slot.
        // Leave a little space for the separate keeper process.
        let required = self
            .memory_bytes
            .checked_mul(devices as u64)
            .and_then(|n| n.checked_add(16 * 1024 * 1024))
            .ok_or("worker memory overflow")?;
        if finite(&self.root, "memory.max")? < required
            || finite(&self.root, "pids.max")? < self.tasks * devices as u64 + 1
        {
            return Err("storage worker pool exceeds aggregate ceilings".into());
        }
        cpu(&self.root)?;
        if fs::read_to_string(self.root.join("memory.swap.max"))?.trim() != "0" {
            return Err("storage worker swap must be disabled".into());
        }
        // Imports and collection run in the supervisor, so it also needs finite
        // ceilings. It must live outside the persistent worker subtree.
        let supervisor = member(std::process::id())?;
        if supervisor.starts_with(&self.root) {
            return Err("supervisor must be outside worker subtree".into());
        }
        finite(&supervisor, "memory.max")?;
        finite(&supervisor, "pids.max")?;
        cpu(&supervisor)?;
        fs::write(
            self.root.join("cgroup.subtree_control"),
            "+cpu +memory +pids",
        )?;
        Ok(())
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        if !valid_id(id) {
            return Err("invalid volume cgroup id".into());
        }
        Ok(self.root.join(format!("vol-{id}")))
    }
    pub fn prepare(&self, id: &str) -> Result<PathBuf> {
        let path = self.path(id)?;
        fs::create_dir_all(&path)?;
        if path.canonicalize()? != path {
            return Err("invalid worker cgroup path".into());
        }
        for (name, value) in [
            ("memory.max", self.memory_bytes.to_string()),
            ("memory.swap.max", "0".into()),
            ("memory.oom.group", "1".into()),
            ("cpu.max", format!("{} 100000", self.cpu_quota_us)),
            ("pids.max", self.tasks.to_string()),
        ] {
            fs::write(path.join(name), value)?;
        }
        Ok(path)
    }
    pub fn enter(&self, id: &str, child: &std::process::Child) -> Result<()> {
        // Child is owned and waiting on its launch pipe. Until reaped its PID
        // cannot be reused, even if it exits before this write.
        fs::write(
            self.prepare(id)?.join("cgroup.procs"),
            child.id().to_string(),
        )?;
        Ok(())
    }
    pub fn verify(&self, id: &str, process: &Process) -> Result<()> {
        if process.alive()? {
            let group = self.path(id)?;
            if member(process.pid)? != group {
                return Err(
                    "live storage process outside expected cgroup; stop it before enabling limits"
                        .into(),
                );
            }
            for (name, value) in [
                ("memory.max", self.memory_bytes.to_string()),
                ("cpu.max", format!("{} 100000", self.cpu_quota_us)),
                ("pids.max", self.tasks.to_string()),
                ("memory.swap.max", "0".into()),
                ("memory.oom.group", "1".into()),
            ] {
                if fs::read_to_string(group.join(name))?.trim() != value {
                    return Err(
                        "live storage process has different limits; stop it before changing limits"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }
    pub fn remove(&self, id: &str) -> Result<()> {
        match fs::remove_dir(self.path(id)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsafe_roots_and_foreign_members_are_refused() {
        let resources = Resources {
            root: std::env::temp_dir(),
            memory_bytes: 512 * 1024 * 1024,
            cpu_quota_us: 100000,
            tasks: 128,
        };
        assert!(resources.validate(1).is_err());
        assert!(resources.path("../escape").is_err());
        let process = Process::read(std::process::id()).unwrap().unwrap();
        assert!(resources.verify(&"a".repeat(64), &process).is_err());
    }

    // Separate qualification from the ordinary root filesystem tests: cgroup
    // delegation is not available in every CI/container environment.
    #[test]
    #[ignore = "requires AHVM_TEST_VOLUME_CGROUP and a bounded supervisor cgroup"]
    fn delegated_worker_launch_membership_and_cleanup() {
        let resources = Resources {
            root: std::env::var_os("AHVM_TEST_VOLUME_CGROUP")
                .expect("explicit qualification subtree")
                .into(),
            memory_bytes: 512 * 1024 * 1024,
            cpu_quota_us: 100000,
            tasks: 128,
        };
        resources.validate(1).unwrap();
        assert!(resources.validate(1024).is_err());
        let id = "d".repeat(64);
        let mut child = ChildGuard::new(
            Command::new("/bin/sh")
                .arg("-c")
                .arg("read gate; exec /bin/sleep 30")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        resources.enter(&id, &child).unwrap();
        let process = Process::read(child.id()).unwrap().unwrap();
        resources.verify(&id, &process).unwrap();
        assert!(resources.verify(&"e".repeat(64), &process).is_err());
        let path = resources.path(&id).unwrap();
        assert_eq!(
            fs::read_to_string(path.join("memory.max")).unwrap().trim(),
            resources.memory_bytes.to_string()
        );
        assert_eq!(
            fs::read_to_string(path.join("cpu.max")).unwrap().trim(),
            "100000 100000"
        );
        assert_eq!(
            fs::read_to_string(path.join("pids.max")).unwrap().trim(),
            "128"
        );
        assert!(resources.remove(&id).is_err());
        fs::write(path.join("pids.max"), "129").unwrap();
        assert!(resources.verify(&id, &process).is_err());
        fs::write(path.join("pids.max"), "128").unwrap();
        child.stdin.take().unwrap().write_all(b"G\n").unwrap();
        resources.verify(&id, &process).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        resources.remove(&id).unwrap();
        assert!(!path.exists());
    }
}
