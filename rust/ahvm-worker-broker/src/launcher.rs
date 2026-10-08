//! Fixed root launcher: a fresh exec, one thread, never a pre_exec sandbox.
use super::{
    config::{self, Config},
    kernel,
};
use ahvm_engine::{WorkerRole, WorkerSandbox};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Seek, SeekFrom},
    os::{
        fd::{OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Mount {
    source: PathBuf,
    target: PathBuf,
    readonly: bool,
    executable: bool,
    mapped: bool,
    dev: u64,
    ino: u64,
    uid: u32,
    directory: bool,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub cfg: Config,
    pub id: String,
    pub role: WorkerRole,
    pub uid: u32,
    pub gid: u32,
    pub generation: u64,
    pub spec: serde_json::Value,
    pub spec_path: PathBuf,
    pub binary: PathBuf,
    mounts: Vec<Mount>,
    devices: Vec<PathBuf>,
}
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

pub fn plan(
    cfg: &Config,
    id: &str,
    role: WorkerRole,
    uid: u32,
    gid: u32,
    generation: u64,
) -> io::Result<Plan> {
    config::id(id)?;
    let dir = cfg.data_dir.join(id);
    let directory = fs::File::from(kernel::open(&dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)?);
    if directory.metadata()?.uid() != cfg.daemon_uid {
        return Err(invalid("VM directory must belong to daemon"));
    }
    let spec_path = dir.join(if role == WorkerRole::Vmm {
        "spec.json"
    } else {
        "net.json"
    });
    let spec: serde_json::Value =
        serde_json::from_slice(&config::daemon_file(&spec_path, cfg.daemon_uid)?)
            .map_err(io::Error::other)?;
    let policy: WorkerSandbox = serde_json::from_value(
        spec.get("worker_sandbox")
            .cloned()
            .ok_or_else(|| invalid("missing worker filesystem policy"))?,
    )
    .map_err(io::Error::other)?;
    let gpu = spec.get("gpu").and_then(|v| v.as_bool()).unwrap_or(false);
    let binary = match role {
        WorkerRole::Vmm if gpu => cfg
            .gpu_bin
            .clone()
            .ok_or_else(|| invalid("GPU isolation is not configured; no alternate launch path"))?,
        WorkerRole::Vmm => cfg.vmm_bin.clone(),
        WorkerRole::Netd => cfg
            .netd_bin
            .clone()
            .ok_or_else(|| invalid("network worker is not configured"))?,
    };
    let sockets = dir.join("sock");
    let network = dir.join("net");
    let mut allowed_rw = vec![
        dir.join("root.qcow2"),
        sockets.clone(),
        dir.join("tmp"),
        dir.join("runtime"),
    ];
    let mut devices = vec![PathBuf::from("/dev/null"), PathBuf::from("/dev/urandom")];
    if role == WorkerRole::Vmm {
        if spec.get("trusted_host_socket_access") != Some(&serde_json::Value::Bool(false)) {
            return Err(invalid("broker workers may not enable host socket access"));
        }
        for field in ["rootfs_dir", "kernel_image"] {
            if spec
                .get(field)
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty())
            {
                return Err(invalid(
                    "standalone host filesystem/kernel options are outside broker policy",
                ));
            }
        }
        for field in ["mounts", "volumes"] {
            if spec
                .get(field)
                .and_then(|v| v.as_array())
                .is_some_and(|v| !v.is_empty())
            {
                return Err(invalid("standalone host mounts are outside broker policy"));
            }
        }
        for (field, expected) in [
            ("vsock_control_uds", sockets.join("c.sock")),
            ("vsock_forward_uds", sockets.join("f.sock")),
            ("control_socket_uds", sockets.join("control.sock")),
            ("net_uds", network.join("net.sock")),
        ] {
            if let Some(path) = spec
                .get(field)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                if Path::new(path) != expected {
                    return Err(invalid("worker socket path differs from fixed VM layout"));
                }
            }
        }
        if spec
            .get("vsock_config_uds")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty())
        {
            return Err(invalid("standalone config socket is outside broker policy"));
        }
        let disk = PathBuf::from(
            spec.get("root_disk")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid("missing worker disk"))?,
        );
        if disk != dir.join("root.qcow2") {
            if !cfg.devices.contains(&disk) || disk.parent() != Some(Path::new("/dev")) {
                return Err(invalid(
                    "worker disk is outside its local overlay or configured NBD pool",
                ));
            }
            allowed_rw.push(disk.clone());
            devices.push(disk);
        }
        devices.push("/dev/kvm".into());
        if gpu {
            devices.extend(
                cfg.devices
                    .iter()
                    .filter(|p| p.starts_with("/dev/dri"))
                    .cloned(),
            );
        }
    } else {
        if spec.get("socket").and_then(|v| v.as_str()) != network.join("net.sock").to_str() {
            return Err(invalid("gateway socket path differs from fixed VM layout"));
        }
        allowed_rw = vec![network.clone()];
    }
    let mut common_ro: Vec<_> = cfg.lib_path.split(':').map(PathBuf::from).collect();
    for path in [
        "/lib",
        "/lib64",
        "/usr/lib",
        "/usr/lib64",
        "/etc/localtime",
        "/proc/cpuinfo",
        "/sys/module/kvm_intel/parameters/nested",
        "/sys/module/kvm_amd/parameters/nested",
    ] {
        if Path::new(path).exists() {
            common_ro.push(path.into());
        }
    }
    if gpu {
        let gpu_root = binary
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| invalid("GPU binary layout invalid"))?
            .join("gpu");
        config::root_owned(&gpu_root)?;
        common_ro.push(gpu_root);
        for path in [
            "/usr/share/drirc.d",
            "/usr/share/glvnd",
            "/etc/drirc",
            "/etc/machine-id",
        ] {
            if Path::new(path).exists() {
                common_ro.push(path.into());
            }
        }
    }
    let mut mounts = BTreeMap::new();
    for path in &policy.read_only {
        if !config::clean(path) {
            return Err(invalid("unnormalized worker read path"));
        }
        // The validated spec is copied into tmpfs. Do not expose the supervisor
        // directory, state, logs or later snapshot metadata from this binding.
        if path == &dir || path == &spec_path || path == Path::new("/proc/self/fd") {
            continue;
        }
        let snapshot = path == &dir.join("bundle")
            || path.parent() == Some(cfg.data_dir.join("snapshots").as_path());
        let own = snapshot || path == &network && role == WorkerRole::Vmm;
        let source = if own {
            // No symlink/magic-link components in daemon-controlled sources.
            let opened = fs::File::from(kernel::open(path, libc::O_PATH, 0)?);
            if !opened.metadata()?.is_dir() || opened.metadata()?.uid() != cfg.daemon_uid {
                return Err(invalid(
                    "snapshot/network source is not a daemon-owned directory",
                ));
            }
            path.clone()
        } else {
            path.canonicalize()?
        };
        let image = role == WorkerRole::Vmm
            && cfg.image_roots.iter().any(|root| {
                &source == root || root.is_dir() && source.starts_with(root) && source.is_file()
            });
        if !common_ro.contains(path) && !own && !image {
            return Err(invalid("canonical worker read source is outside configured libraries/images and selected snapshot"));
        }
        if common_ro.contains(path) {
            config::root_owned(&source)?;
        }
        let mapped =
            source.starts_with(&cfg.data_dir) || fs::metadata(&source)?.uid() == cfg.daemon_uid;
        mounts.insert(
            path.clone(),
            Mount {
                source,
                target: path.clone(),
                readonly: true,
                executable: common_ro.contains(path),
                mapped,
                dev: 0,
                ino: 0,
                uid: 0,
                directory: false,
            },
        );
    }
    for path in &policy.read_write {
        if !config::clean(path) || !(allowed_rw.contains(path) || devices.contains(path)) {
            return Err(invalid(
                "worker write path is outside its fixed role policy",
            ));
        }
        if devices.contains(path) {
            continue;
        }
        let source = fs::File::from(kernel::open(path, libc::O_RDONLY, 0)?);
        let meta = source.metadata()?;
        if meta.uid() != cfg.daemon_uid
            || (!meta.is_dir() && (!meta.is_file() || meta.nlink() != 1))
        {
            return Err(invalid(
                "worker writable source must be a daemon-owned directory or unlinked regular file",
            ));
        }
        mounts.insert(
            path.clone(),
            Mount {
                source: path.clone(),
                target: path.clone(),
                readonly: false,
                executable: false,
                mapped: true,
                dev: 0,
                ino: 0,
                uid: 0,
                directory: false,
            },
        );
    }
    for path in &policy.unix_connect {
        if path != &network || role != WorkerRole::Vmm {
            return Err(invalid(
                "worker Unix socket grant is outside its own gateway",
            ));
        }
    }
    // Fixed ELF input and its fixed interpreter, never a daemon-supplied script.
    let mut elf = fs::File::open(&binary)?;
    let mut header = [0u8; 64];
    elf.read_exact(&mut header)?;
    if header.get(..6) != Some(b"\x7fELF\x02\x01") {
        return Err(invalid("worker must be a fixed ELF64 little-endian binary"));
    }
    let phoff = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let size = u16::from_le_bytes(header[54..56].try_into().unwrap()) as u64;
    let count = u16::from_le_bytes(header[56..58].try_into().unwrap()) as u64;
    if size < 56 || count > 1024 {
        return Err(invalid("invalid ELF program headers"));
    }
    for n in 0..count {
        let offset = phoff
            .checked_add(n * size)
            .ok_or_else(|| invalid("ELF header overflow"))?;
        elf.seek(SeekFrom::Start(offset))?;
        let mut ph = [0u8; 56];
        elf.read_exact(&mut ph)?;
        if ph[..4] == 3u32.to_le_bytes() {
            let start = u64::from_le_bytes(ph[8..16].try_into().unwrap());
            let length = u64::from_le_bytes(ph[32..40].try_into().unwrap()) as usize;
            if length == 0 || length > 4096 {
                return Err(invalid("ELF interpreter too long"));
            }
            elf.seek(SeekFrom::Start(start))?;
            let mut text = vec![0; length];
            elf.read_exact(&mut text)?;
            if text.pop() != Some(0) {
                return Err(invalid("ELF interpreter is not terminated"));
            }
            let target = PathBuf::from(std::str::from_utf8(&text).map_err(io::Error::other)?);
            if !config::clean(&target) {
                return Err(invalid("ELF interpreter must be absolute"));
            }
            let source = target.canonicalize()?;
            config::root_owned(&source)?;
            if !common_ro
                .iter()
                .any(|root| root.is_dir() && target.starts_with(root))
            {
                mounts.insert(
                    target.clone(),
                    Mount {
                        source,
                        target,
                        readonly: true,
                        executable: true,
                        mapped: false,
                        dev: 0,
                        ino: 0,
                        uid: 0,
                        directory: false,
                    },
                );
            }
        }
    }
    mounts.insert(
        binary.clone(),
        Mount {
            source: binary.clone(),
            target: binary.clone(),
            readonly: true,
            executable: true,
            mapped: false,
            dev: 0,
            ino: 0,
            uid: 0,
            directory: false,
        },
    );
    for mount in mounts.values_mut() {
        let file = fs::File::from(kernel::open(&mount.source, libc::O_PATH, 0)?);
        let meta = file.metadata()?;
        if !meta.is_dir()
            && (!meta.is_file() || meta.nlink() != 1 && (!mount.readonly || meta.uid() != 0))
        {
            return Err(invalid("source must be a directory or regular file; writable and nonroot sources cannot have hardlinks"));
        }
        mount.dev = meta.dev();
        mount.ino = meta.ino();
        mount.uid = meta.uid();
        mount.directory = meta.is_dir();
    }
    Ok(Plan {
        cfg: cfg.clone(),
        id: id.into(),
        role,
        uid,
        gid,
        generation,
        spec,
        spec_path,
        binary,
        mounts: mounts.into_values().collect(),
        devices,
    })
}

pub fn mapping(uid: u32, gid: u32, daemon_uid: u32, daemon_gid: u32) -> io::Result<OwnedFd> {
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--idmap")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut ready = [0];
        child
            .stdout
            .as_mut()
            .ok_or_else(|| invalid("idmap pipe missing"))?
            .read_exact(&mut ready)
            .map_err(|e| invalid(&format!("create mount idmap user namespace: {e}")))?;
        if ready != *b"R" {
            return Err(invalid("idmap helper failed"));
        }
        let base = PathBuf::from(format!("/proc/{}", child.id()));
        // Map only the artifact owner. Workers remain in the initial userns.
        // No host UID0 mapping or CAP_SETFCAP is needed in this flat jail.
        fs::write(base.join("uid_map"), format!("{daemon_uid} {uid} 1\n")).map_err(|e| {
            invalid(&format!(
                "write idmap uid_map (effective CAP_SETUID required): {e}"
            ))
        })?;
        fs::write(base.join("setgroups"), "deny\n")?;
        fs::write(base.join("gid_map"), format!("{daemon_gid} {gid} 1\n")).map_err(|e| {
            invalid(&format!(
                "write idmap gid_map (effective CAP_SETGID required): {e}"
            ))
        })?;
        Ok(OwnedFd::from(fs::File::open(base.join("ns/user"))?))
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn open_source(mount: &Mount, daemon_uid: u32) -> io::Result<OwnedFd> {
    let fd = kernel::open(&mount.source, libc::O_PATH, 0)?;
    let meta = fs::File::from(fd.try_clone()?).metadata()?;
    if meta.dev() != mount.dev
        || meta.ino() != mount.ino
        || meta.uid() != mount.uid
        || meta.is_dir() != mount.directory
        || !meta.is_dir()
            && (!meta.is_file() || meta.nlink() != 1 && (!mount.readonly || meta.uid() != 0))
        || !mount.readonly && meta.uid() != daemon_uid
    {
        return Err(invalid("mount source changed after broker validation"));
    }
    Ok(fd)
}

pub fn execute(plan: Plan, ack: RawFd) -> io::Result<()> {
    let started = Instant::now();
    kernel::close_fds(&[ack])?;
    kernel::synthetic_umask();
    let ns = mapping(plan.uid, plan.gid, plan.cfg.daemon_uid, plan.cfg.daemon_gid)?;
    let group = plan
        .cfg
        .cgroup_root
        .join(format!("vm-{}", plan.id))
        .join(plan.role.name());
    // Move before any untrusted worker code. Process PID/starttime stay stable.
    fs::write(group.join("cgroup.procs"), "0")?;
    kernel::private_mount_namespace()?;
    let root = plan.cfg.jail_dir.join(format!(
        "{}-{}-{}",
        plan.id,
        plan.role.name(),
        plan.generation
    ));
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    kernel::tmpfs(&root)?;
    let target = |path: &Path| root.join(path.strip_prefix("/").expect("validated absolute path"));
    // Open sources before any mount can hide another source path. Construct all
    // placeholders in the empty root before readonly ancestors are attached.
    let mut sources = Vec::new();
    for mount in &plan.mounts {
        let fd = open_source(mount, plan.cfg.daemon_uid)?;
        let meta = fs::File::from(fd.try_clone()?).metadata()?;
        let path = target(&mount.target);
        if meta.is_dir() {
            fs::create_dir_all(&path)?;
        } else {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::File::create(&path)?;
        }
        sources.push(fd);
    }
    fs::create_dir_all(target(Path::new("/proc/self/fd")))?;
    fs::create_dir_all(target(plan.spec_path.parent().unwrap()))?;
    // Copy the exact validated bytes. A daemon rewrite cannot change inputs
    // after the broker validated allowed paths/binary/host-socket policy.
    fs::write(
        target(&plan.spec_path),
        serde_json::to_vec(&plan.spec).map_err(io::Error::other)?,
    )?;
    fs::set_permissions(target(&plan.spec_path), fs::Permissions::from_mode(0o444))?;
    for device in &plan.devices {
        let file = fs::File::from(kernel::open(device, libc::O_PATH, 0)?);
        let meta = file.metadata()?;
        let kind = if meta.file_type().is_char_device() {
            libc::S_IFCHR
        } else if meta.file_type().is_block_device() {
            libc::S_IFBLK
        } else {
            return Err(invalid("configured device is not a device node"));
        };
        let path = target(device);
        fs::create_dir_all(path.parent().unwrap())?;
        kernel::device(&path, kind, meta.rdev(), plan.uid, plan.gid)?;
    }
    let proc = kernel::open(
        Path::new(&format!("/proc/{}/fd", std::process::id())),
        libc::O_PATH,
        0,
    )?;
    kernel::bind(
        &proc,
        &target(Path::new("/proc/self/fd")),
        None,
        true,
        false,
    )?;
    for (mount, source) in plan.mounts.iter().zip(&sources) {
        kernel::bind(
            source,
            &target(&mount.target),
            mount.mapped.then_some(&ns),
            mount.readonly,
            mount.executable,
        )?;
    }
    drop(proc);
    drop(sources);
    drop(ns);
    kernel::readonly_root(&root)?;
    kernel::chroot(&root)?;
    // Credential changes make pre-exec proc inodes root-owned while dumpable0.
    // Close setup FDs as root before dropping IDs. Final non-setid ELF exec
    // establishes the worker mm/FD view and normal dumpability; no privileged
    // descriptor survives to it, and distinct host IDs/Landlock deny peers.
    kernel::close_fds(&[ack])?;
    kernel::drop_identity(plan.uid, plan.gid)?;
    kernel::cloexec(ack)?;
    eprintln!(
        "worker-broker: role={} host_uid={} host_gid={} generation={} setup_us={}",
        plan.role.name(),
        plan.uid,
        plan.gid,
        plan.generation,
        started.elapsed().as_micros()
    );
    let dir = plan.cfg.data_dir.join(&plan.id);
    let mut command = Command::new(&plan.binary);
    command
        .arg(&plan.spec_path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8");
    if plan.role == WorkerRole::Vmm {
        command
            .env("HOME", dir.join("runtime"))
            .env("TMPDIR", dir.join("tmp"))
            .env("LD_LIBRARY_PATH", &plan.cfg.lib_path);
        if plan
            .spec
            .get("gpu")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let gpu = plan.binary.parent().unwrap().parent().unwrap().join("gpu");
            command
                .env(
                    "LD_LIBRARY_PATH",
                    format!("{}/lib:{}", gpu.display(), plan.cfg.lib_path),
                )
                .env("__EGL_VENDOR_LIBRARY_FILENAMES", gpu.join("egl.json"))
                .env("LIBGL_DRIVERS_PATH", gpu.join("dri"))
                .env("MESA_SHADER_CACHE_DIR", dir.join("runtime/mesa-cache"));
        }
    } else {
        command.env("LD_LIBRARY_PATH", &plan.cfg.lib_path);
    }
    Err(command.exec())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swapped_writable_source_is_rejected_before_bind() {
        let root = std::env::temp_dir().join(format!(
            "ahvm-source-swap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("disk");
        fs::write(&path, b"approved").unwrap();
        let meta = fs::metadata(&path).unwrap();
        let mount = Mount {
            source: path.clone(),
            target: path.clone(),
            readonly: false,
            executable: false,
            mapped: true,
            dev: meta.dev(),
            ino: meta.ino(),
            uid: meta.uid(),
            directory: false,
        };
        drop(open_source(&mount, meta.uid()).unwrap());
        fs::rename(&path, root.join("old-inode")).unwrap();
        fs::write(&path, b"swapped-peer-inode").unwrap();
        assert!(open_source(&mount, meta.uid())
            .unwrap_err()
            .to_string()
            .contains("source changed"));
        fs::remove_dir_all(root).unwrap();
    }
}
