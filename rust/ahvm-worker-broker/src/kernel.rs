//! Small syscall boundary, used only in the fixed single-threaded launcher.
//! All paths/IDs are validated against the root-owned broker configuration.
#![allow(unsafe_code)]
use std::{
    ffi::CString,
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    path::Path,
};

fn string(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)
}
fn checked(value: libc::c_long) -> io::Result<libc::c_long> {
    if value < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

#[repr(C)]
#[derive(Default)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
pub fn open(path: &Path, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    let path = string(path)?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: mode as u64,
        resolve: 0x02 | 0x04,
    };
    // No symlinks or proc magic links, including intermediate components.
    let fd = checked(unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

#[repr(C)]
#[derive(Default)]
struct MountAttr {
    set: u64,
    clear: u64,
    propagation: u64,
    userns: u64,
}
const RDONLY: u64 = 1;
const NOSUID: u64 = 2;
const NODEV: u64 = 4;
const NOEXEC: u64 = 8;
const IDMAP: u64 = 0x100000;
const RECURSIVE: u32 = 0x8000;

pub fn private_mount_namespace() -> io::Result<()> {
    checked(unsafe { libc::unshare(libc::CLONE_NEWNS) }.into())?;
    let root = c"/";
    checked(
        unsafe {
            libc::mount(
                std::ptr::null(),
                root.as_ptr(),
                std::ptr::null(),
                libc::MS_PRIVATE | libc::MS_REC,
                std::ptr::null(),
            )
        }
        .into(),
    )?;
    Ok(())
}
pub fn new_user_namespace() -> io::Result<()> {
    checked(unsafe { libc::unshare(libc::CLONE_NEWUSER) }.into())?;
    Ok(())
}
pub fn tmpfs(path: &Path) -> io::Result<()> {
    let path = string(path)?;
    checked(
        unsafe {
            libc::mount(
                c"tmpfs".as_ptr(),
                path.as_ptr(),
                c"tmpfs".as_ptr(),
                libc::MS_NOSUID | libc::MS_NOEXEC,
                c"size=16m,mode=755".as_ptr().cast(),
            )
        }
        .into(),
    )?;
    Ok(())
}
pub fn synthetic_umask() {
    // Single-threaded fixed launcher only. Empty private-root parents must be
    // traversable by the future worker UID; host backing parent remains0700.
    unsafe {
        libc::umask(0o022);
    }
}
pub fn bind(
    source: &OwnedFd,
    target: &Path,
    mapping: Option<&OwnedFd>,
    readonly: bool,
    executable: bool,
) -> io::Result<()> {
    let target = string(target)?;
    let raw = checked(unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            source.as_raw_fd(),
            c"".as_ptr(),
            1 | libc::O_CLOEXEC | libc::AT_EMPTY_PATH | RECURSIVE as i32,
        )
    })
    .map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("open_tree for {}: {e}", target.to_string_lossy()),
        )
    })?;
    let tree = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let attr = MountAttr {
        set: NOSUID
            | NODEV
            | if readonly { RDONLY } else { 0 }
            | if executable { 0 } else { NOEXEC }
            | if mapping.is_some() { IDMAP } else { 0 },
        clear: if readonly { 0 } else { RDONLY },
        userns: mapping.map_or(0, |fd| fd.as_raw_fd() as u64),
        ..Default::default()
    };
    checked(unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | RECURSIVE as i32,
            &attr,
            std::mem::size_of::<MountAttr>(),
        )
    })
    .map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "mount_setattr for {} (idmap={}): {e}",
                target.to_string_lossy(),
                mapping.is_some()
            ),
        )
    })?;
    checked(unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            tree.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            4,
        )
    })
    .map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("move_mount to {}: {e}", target.to_string_lossy()),
        )
    })?;
    Ok(())
}
pub fn readonly_root(path: &Path) -> io::Result<()> {
    let path = string(path)?;
    // Deliberately nonrecursive: exact writable submounts stay writable.
    let attr = MountAttr {
        set: RDONLY | NOSUID | NOEXEC,
        ..Default::default()
    };
    checked(unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            path.as_ptr(),
            0,
            &attr,
            std::mem::size_of::<MountAttr>(),
        )
    })?;
    Ok(())
}
pub fn device(path: &Path, mode: u32, rdev: u64, uid: u32, gid: u32) -> io::Result<()> {
    let path = string(path)?;
    checked(unsafe { libc::mknod(path.as_ptr(), mode | 0o600, rdev) }.into())?;
    checked(unsafe { libc::chown(path.as_ptr(), uid, gid) }.into())?;
    Ok(())
}
pub fn chroot(path: &Path) -> io::Result<()> {
    let path = string(path)?;
    checked(unsafe { libc::chroot(path.as_ptr()) }.into())?;
    std::env::set_current_dir("/")
}
pub fn drop_identity(uid: u32, gid: u32) -> io::Result<()> {
    checked(unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0) }.into())?;
    checked(
        unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_CLEAR_ALL,
                0,
                0,
                0,
            )
        }
        .into(),
    )?;
    for capability in 0..64 {
        let result = unsafe { libc::prctl(libc::PR_CAPBSET_DROP, capability, 0, 0, 0) };
        if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL) {
            return Err(io::Error::last_os_error());
        }
    }
    checked(unsafe { libc::setgroups(0, std::ptr::null()) }.into())?;
    checked(unsafe { libc::setresgid(gid, gid, gid) }.into())?;
    checked(unsafe { libc::setresuid(uid, uid, uid) }.into())?;
    unsafe {
        libc::setfsuid(uid);
        libc::setfsgid(gid);
    }
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = CapHeader {
        version: 0x20080522,
        pid: 0,
    };
    let mut data = [CapData::default(), CapData::default()];
    checked(unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) })?;
    checked(unsafe { libc::syscall(libc::SYS_capget, &header, data.as_mut_ptr()) })?;
    if data
        .iter()
        .any(|c| c.effective != 0 || c.permitted != 0 || c.inheritable != 0)
    {
        return Err(io::Error::other("failed to clear worker capabilities"));
    }
    checked(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }.into())?;
    // The private jail intentionally lacks global proc; use direct credential
    // queries to verify real/effective/saved/filesystem IDs and groups.
    let (mut real, mut effective, mut saved) = (0, 0, 0);
    checked(unsafe { libc::getresuid(&mut real, &mut effective, &mut saved) }.into())?;
    if (real, effective, saved) != (uid, uid, uid) {
        return Err(io::Error::other("failed to drop worker UIDs"));
    }
    checked(unsafe { libc::getresgid(&mut real, &mut effective, &mut saved) }.into())?;
    if (real, effective, saved) != (gid, gid, gid) {
        return Err(io::Error::other("failed to drop worker GIDs"));
    }
    if unsafe { libc::setfsuid(u32::MAX) } as u32 != uid
        || unsafe { libc::setfsgid(u32::MAX) } as u32 != gid
        || checked(unsafe { libc::getgroups(0, std::ptr::null_mut()) }.into())? != 0
    {
        return Err(io::Error::other(
            "failed to clear worker filesystem IDs/groups",
        ));
    }
    Ok(())
}
pub fn close_fds(keep: &[RawFd]) -> io::Result<()> {
    // Fixed launcher only: one thread, all temporary OwnedFd/File values have
    // been dropped before calling. Retained raw ack FD is closed by final exec;
    // callers must exit rather than reuse old RAII descriptors on an error.
    let fds: Vec<_> = fs::read_dir("/proc/self/fd")?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .collect();
    for fd in fds {
        if fd > 2 && !keep.contains(&fd) {
            unsafe {
                libc::close(fd);
            }
        }
    }
    Ok(())
}
pub fn inherit(fd: RawFd) -> io::Result<()> {
    checked(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }.into())?;
    Ok(())
}
pub fn cloexec(fd: RawFd) -> io::Result<()> {
    checked(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }.into())?;
    Ok(())
}
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    checked(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }.into())?;
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}
pub fn chown_fd(fd: RawFd, uid: u32, gid: u32) -> io::Result<()> {
    checked(unsafe { libc::fchown(fd, uid, gid) }.into())?;
    Ok(())
}
pub fn poll(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    Ok(checked(unsafe { libc::poll(&mut pfd, 1, timeout_ms) }.into())? > 0)
}
