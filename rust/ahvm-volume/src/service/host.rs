use super::*;
use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub pid: u32,
    pub start: u64,
    pub boot: String,
}
impl Process {
    pub fn read(pid: u32) -> Result<Option<Self>> {
        let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .ok_or("invalid process stat")?
            .1
            .split_whitespace()
            .collect();
        if fields.first() == Some(&"Z") {
            return Ok(None);
        }
        Ok(Some(Self {
            pid,
            start: fields.get(19).ok_or("invalid process stat")?.parse()?,
            boot: fs::read_to_string("/proc/sys/kernel/random/boot_id")?
                .trim()
                .into(),
        }))
    }
    pub fn alive(&self) -> Result<bool> {
        Ok(Self::read(self.pid)?.as_ref() == Some(self))
    }
    pub fn stop(&self) -> Result<()> {
        let pid = Pid::from_raw(i32::try_from(self.pid)?).ok_or("invalid PID")?;
        let fd = match pidfd_open(pid, PidfdFlags::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::SRCH) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if !self.alive()? {
            return Ok(());
        }
        for (sig, seconds) in [(Signal::TERM, 5), (Signal::KILL, 10)] {
            match pidfd_send_signal(&fd, sig) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => (),
                Err(e) => return Err(e.into()),
            }
            let deadline = Instant::now() + Duration::from_secs(seconds);
            while self.alive()? && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if !self.alive()? {
                return Ok(());
            }
        }
        Err("process did not exit".into())
    }
}
#[derive(Debug)]
pub struct Lock(File);
impl Lock {
    pub fn take(path: &Path) -> Result<Self> {
        let f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)?;
        f.try_lock()
            .map_err(|_| "already controlled by another process")?;
        Ok(Self(f))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
pub fn private_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err("absolute directory required".into());
    }
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o077 != 0 {
        return Err("root-owned private directory required".into());
    }
    Ok(())
}
pub fn save(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&tmp)?;
    serde_json::to_writer(&mut f, value)?;
    f.sync_all()?;
    fs::rename(tmp, path)?;
    File::open(path.parent().ok_or("missing parent")?)?.sync_all()?;
    Ok(())
}
pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = File::open(path)?;
    let mut data = Vec::new();
    file.take(16385).read_to_end(&mut data)?;
    if data.len() > 16384 {
        return Err("record too large".into());
    }
    serde_json::from_slice(&data).map_err(|_| "invalid record".into())
}
pub fn nbd_pid(device: &Path) -> Result<Option<u32>> {
    let path = Path::new("/sys/block")
        .join(device.file_name().ok_or("invalid device")?)
        .join("pid");
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s.trim().parse()?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn consumers(device: &Path) -> Result<Vec<u32>> {
    let dev = fs::metadata(device)?.rdev();
    let mut result = Vec::new();
    for proc in fs::read_dir("/proc")? {
        let proc = proc?;
        let Ok(pid) = proc.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let fds = match fs::read_dir(proc.path().join("fd")) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for fd in fds {
            match fs::metadata(fd?.path()) {
                Ok(m) if m.file_type().is_block_device() && m.rdev() == dev => {
                    result.push(pid);
                    break;
                }
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(result)
}
pub fn unused(device: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let nbd = nbd_pid(device)?;
        if consumers(device)?.iter().all(|p| Some(*p) == nbd) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("disk still held by a process".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Closing the launch pipe cancels an uncommitted child. Every exit path reaps
/// it; committed storage children deliberately remain alive across our drop.
#[derive(Debug)]
pub struct ChildGuard(Option<std::process::Child>);
impl ChildGuard {
    pub fn new(child: std::process::Child) -> Self {
        Self(Some(child))
    }
}
impl std::ops::Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().unwrap()
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            drop(child.stdin.take());
            if !matches!(child.try_wait(), Ok(Some(_))) {
                thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }
}

/// A non-root API client must not turn image import into a root file-read proxy.
/// Restrict sources to admin-managed, immutable paths (including ancestors).
pub fn trusted_image_path(path: &Path) -> Result<()> {
    for part in path.ancestors() {
        let m = fs::symlink_metadata(part)?;
        let sticky_root_dir = m.is_dir() && m.mode() & 0o1000 != 0;
        if m.uid() != 0 || m.file_type().is_symlink() || (m.mode() & 0o022 != 0 && !sticky_root_dir)
        {
            return Err("image paths must be controlled by root".into());
        }
    }
    Ok(())
}
