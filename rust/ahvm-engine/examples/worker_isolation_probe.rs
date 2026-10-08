//! Fixed ELF adversarial probe for the *actual broker launcher*, never installed
//! as a production worker. The root qualification fixture selects this binary.
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ahvm_engine::WorkerSandbox;
    use std::{
        fs,
        io::{Read, Write},
        os::{
            fd::AsRawFd,
            unix::{
                fs::PermissionsExt,
                net::{UnixListener, UnixStream},
            },
        },
        path::{Path, PathBuf},
        time::Duration,
    };
    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("probe requires sealed spec")?,
    );
    let spec: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
    let probe = spec
        .get("isolation_probe")
        .ok_or("qualification-only probe settings missing")?;
    let policy: WorkerSandbox = serde_json::from_value(spec["worker_sandbox"].clone())?;
    let daemon_uid = probe["daemon_uid"].as_u64().ok_or("missing daemon uid")? as u32;
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    assert_ne!(uid, 0);
    assert_ne!(uid, daemon_uid);
    assert_eq!(uid, rustix::process::geteuid().as_raw());
    assert_eq!(gid, rustix::process::getegid().as_raw());
    let netd = spec.get("ethernet_contract").is_some();
    policy.restrict(netd)?;
    let dir = path.parent().unwrap();
    let out = dir.join(if netd { "net" } else { "runtime" });
    let mut denied = serde_json::Map::new();
    let mut deny = |label: &str, error: std::io::Error| {
        denied.insert(label.into(), serde_json::json!(error.raw_os_error()));
    };
    for (label, target) in [
        (
            "peer_contents",
            probe["peer_file"].as_str().ok_or("peer file missing")?,
        ),
        (
            "daemon_state",
            probe["daemon_file"].as_str().ok_or("daemon file missing")?,
        ),
    ] {
        deny(label, fs::read(target).unwrap_err());
        deny(
            &format!("{label}_chmod"),
            fs::set_permissions(target, fs::Permissions::from_mode(0o777)).unwrap_err(),
        );
    }
    deny(
        "peer_pathname_uds",
        UnixStream::connect(probe["peer_socket"].as_str().ok_or("peer socket missing")?)
            .unwrap_err(),
    );
    deny(
        "sealed_spec_write",
        fs::write(&path, b"forged").unwrap_err(),
    );
    deny(
        "sealed_spec_chmod",
        fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap_err(),
    );
    assert!(fs::hard_link(&path, out.join("metadata-link")).is_err());
    fs::write(out.join("owned-artifact"), b"daemon-owned-on-host")?;
    let peer_file = Path::new(probe["peer_file"].as_str().unwrap());
    std::os::unix::fs::symlink(peer_file, out.join("escape"))?;
    deny("symlink_escape", fs::read(out.join("escape")).unwrap_err());
    let target = probe["target_pid"].as_u64().ok_or("target pid missing")? as i32;
    let pid = rustix::process::Pid::from_raw(target).unwrap();
    assert_eq!(
        rustix::process::kill_process(pid, rustix::process::Signal::CONT),
        Err(rustix::io::Errno::PERM)
    );
    deny(
        "target_proc",
        fs::read(format!("/proc/{target}/environ")).unwrap_err(),
    );
    #[allow(unsafe_code)]
    {
        // Read-only probes against this fixture's synthetic process/buffer.
        let mut buffer = [0u8; 8];
        let local = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        };
        let remote = libc::iovec {
            iov_base: probe["target_memory"].as_u64().unwrap_or(1) as *mut libc::c_void,
            iov_len: buffer.len(),
        };
        assert_eq!(
            unsafe { libc::process_vm_readv(target, &local, 1, &remote, 1, 0) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        );
        let text = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        assert_eq!(
            unsafe {
                libc::setxattr(
                    text.as_ptr(),
                    c"user.forged".as_ptr(),
                    c"x".as_ptr().cast(),
                    1,
                    0,
                )
            },
            -1
        );
        deny("sealed_spec_xattr", std::io::Error::last_os_error());
    }
    let tcp = probe["host_tcp"]
        .as_str()
        .ok_or("host tcp missing")?
        .parse()?;
    if netd {
        drop(std::net::TcpStream::connect_timeout(
            &tcp,
            Duration::from_secs(1),
        )?);
    } else {
        deny(
            "host_tcp",
            std::net::TcpStream::connect_timeout(&tcp, Duration::from_secs(1)).unwrap_err(),
        );
        deny(
            "host_tcp_bind",
            std::net::TcpListener::bind("127.0.0.1:0").unwrap_err(),
        );
    }
    let mut disk = if netd {
        None
    } else {
        Some(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(spec["root_disk"].as_str().unwrap())?,
        )
    };
    let staged = out.join("bundle.new");
    let held_dir = if netd || probe.get("snapshot_proof") == Some(&serde_json::Value::Bool(false)) {
        None
    } else {
        fs::create_dir(&staged)?;
        for name in ["memory.img", "checkpoint.bin", "manifest.json"] {
            fs::write(staged.join(name), b"untrusted-native-payload")?;
        }
        Some(fs::File::open(&staged)?)
    };
    let socket = if netd {
        dir.join("net/net.sock")
    } else {
        dir.join("sock")
            .join(probe["socket_name"].as_str().unwrap_or("control.sock"))
    };
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let report = serde_json::json!({"uid":uid,"gid":gid,"role":if netd {"netd"} else {"vmm"},"denied":denied,"disk_open":disk.is_some(),"own_artifact":true});
    fs::write(out.join("proof.json"), serde_json::to_vec_pretty(&report)?)?;
    loop {
        let (mut conn, _) = listener.accept()?;
        conn.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut command = [0u8; 32];
        let count = conn.read(&mut command)?;
        match &command[..count] {
            b"PING" => conn.write_all(b"OK")?,
            b"PUBLISH" => {
                let held = held_dir.as_ref().ok_or("gateway has no snapshot")?;
                #[allow(unsafe_code)]
                {
                    let fd = unsafe {
                        libc::openat(
                            held.as_raw_fd(),
                            c"backing-image.json".as_ptr(),
                            libc::O_RDONLY | libc::O_CLOEXEC,
                        )
                    };
                    assert_eq!(fd, -1);
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ENOENT)
                    );
                    assert_eq!(
                        unsafe {
                            libc::fchmodat(
                                held.as_raw_fd(),
                                c"backing-image.json".as_ptr(),
                                0o777,
                                0,
                            )
                        },
                        -1
                    );
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ENOENT)
                    );
                }
                assert!(fs::read(dir.join("bundle.pending/backing-image.json")).is_err());
                conn.write_all(b"PASS retained staging FD cannot name trusted metadata")?;
            }
            b"CONNECT" => {
                let mut net = UnixStream::connect(dir.join("net/net.sock"))?;
                net.write_all(b"PING")?;
                let mut reply = [0; 2];
                net.read_exact(&mut reply)?;
                assert_eq!(&reply, b"OK");
                conn.write_all(b"OK")?;
            }
            b"FORK" => {
                #[allow(unsafe_code)]
                let child = unsafe { libc::fork() };
                assert!(child >= 0);
                if child == 0 {
                    loop {
                        std::thread::sleep(Duration::from_secs(60));
                    }
                }
                conn.write_all(child.to_string().as_bytes())?;
            }
            b"EXIT" => {
                drop(disk.take());
                return Ok(());
            }
            _ => return Err("unknown fixture command".into()),
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("worker_isolation_probe requires Linux");
}
