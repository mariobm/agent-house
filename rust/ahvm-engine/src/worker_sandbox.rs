//! Host-authored filesystem policy applied by a Linux worker before libkrun starts threads.

use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSandbox {
    pub read_only: Vec<PathBuf>,
    pub read_write: Vec<PathBuf>,
    /// Exact readonly directories containing the VM's gateway pathname UDS.
    #[serde(default)]
    pub unix_connect: Vec<PathBuf>,
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::{
            linux::net::SocketAddrExt,
            unix::net::{SocketAddr, UnixListener},
        },
        process::Command,
    };

    #[test]
    fn kernel_isolation() {
        if std::env::var("AHVM_WORKER_SANDBOX_TEST").as_deref() != Ok("1") {
            eprintln!("SKIP: set AHVM_WORKER_SANDBOX_TEST=1 on Linux with Landlock ABI 6+");
            return;
        }
        let dir = crate::test_scratch("worker-landlock");
        for name in ["vm-a", "vm-b"] {
            fs::create_dir_all(dir.join(name)).unwrap();
        }
        fs::write(dir.join("base"), b"base").unwrap();
        fs::write(dir.join("vm-b/disk"), b"peer").unwrap();
        fs::write(dir.join("daemon.db"), b"daemon").unwrap();
        for name in ["sock", "tmp", "runtime"] {
            fs::create_dir(dir.join("vm-a").join(name)).unwrap();
        }
        fs::write(dir.join("vm-a/disk"), b"disk").unwrap();
        for name in [
            "state.json",
            "spec.json",
            "sandbox.json",
            "backing-image.json",
        ] {
            fs::write(dir.join("vm-a").join(name), b"trusted-host-metadata").unwrap();
        }
        let socket = UnixListener::bind(dir.join("vm-b/control.sock")).unwrap();
        let abstract_name = format!("ahvm-worker-test-{}", std::process::id());
        let abstract_socket =
            UnixListener::bind_addr(&SocketAddr::from_abstract_name(&abstract_name).unwrap())
                .unwrap();
        let private_memory = *b"private-test";
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let policy = WorkerSandbox {
            unix_connect: Vec::new(),
            read_only: vec![dir.join("base"), dir.join("vm-a")],
            read_write: vec![
                dir.join("vm-a/disk"),
                dir.join("vm-a/sock"),
                dir.join("vm-a/tmp"),
                dir.join("vm-a/runtime"),
            ],
        };
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "worker_sandbox::tests::sandbox_child",
                "--nocapture",
            ])
            .env("AHVM_SANDBOX_CHILD_DIR", &dir)
            .env("AHVM_SANDBOX_CHILD_PARENT", std::process::id().to_string())
            .env(
                "AHVM_SANDBOX_CHILD_MEMORY",
                (private_memory.as_ptr() as usize).to_string(),
            )
            .env("AHVM_SANDBOX_CHILD_ABSTRACT", abstract_name)
            .env(
                "AHVM_SANDBOX_CHILD_TCP",
                tcp.local_addr().unwrap().to_string(),
            )
            .env(
                "AHVM_SANDBOX_CHILD_POLICY",
                serde_json::to_string(&policy).unwrap(),
            )
            .output()
            .unwrap();
        eprintln!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        drop(socket);
        drop(abstract_socket);
        fs::remove_dir_all(dir).unwrap();
        assert!(output.status.success());
    }

    #[test]
    fn sandbox_child() {
        let Some(dir) = std::env::var_os("AHVM_SANDBOX_CHILD_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let parent: i32 = std::env::var("AHVM_SANDBOX_CHILD_PARENT")
            .unwrap()
            .parse()
            .unwrap();
        let policy: WorkerSandbox =
            serde_json::from_str(&std::env::var("AHVM_SANDBOX_CHILD_POLICY").unwrap()).unwrap();
        let started = std::time::Instant::now();
        let pathname_sockets = policy.restrict(false).unwrap();
        println!("policy_setup_us={}", started.elapsed().as_micros());
        let own = dir.join("vm-a");
        assert_eq!(fs::read(dir.join("base")).unwrap(), b"base");
        fs::write(own.join("disk"), b"own").unwrap();
        fs::create_dir(own.join("runtime/bundle.new")).unwrap();
        fs::write(own.join("runtime/bundle.new/memory.img"), b"snapshot").unwrap();
        let listener = UnixListener::bind(own.join("sock/control.sock")).unwrap();
        assert!(std::os::unix::net::UnixStream::connect(own.join("sock/control.sock")).is_ok());
        drop(listener);
        for name in [
            "state.json",
            "spec.json",
            "sandbox.json",
            "backing-image.json",
        ] {
            let path = own.join(name);
            assert_eq!(
                fs::write(&path, b"forged-backing-or-policy")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(fs::read(&path).unwrap(), b"trusted-host-metadata");
        }
        for path in [
            dir.join("base"),
            dir.join("daemon.db"),
            dir.join("vm-b/disk"),
        ] {
            assert_eq!(
                fs::write(&path, b"blocked").unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                fs::remove_file(&path).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        for path in [
            dir.join("daemon.db"),
            dir.join("vm-b/disk"),
            PathBuf::from(format!("/proc/{parent}/environ")),
            PathBuf::from(format!("/proc/{parent}/mem")),
        ] {
            assert_eq!(
                fs::read(path).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        std::os::unix::fs::symlink(dir.join("vm-b/disk"), own.join("tmp/escape")).unwrap();
        assert_eq!(
            fs::read(own.join("tmp/escape")).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(fs::hard_link(dir.join("vm-b/disk"), own.join("tmp/peer-link")).is_err());
        let target = rustix::process::Pid::from_raw(parent).unwrap();
        assert_eq!(
            rustix::process::kill_process(target, rustix::process::Signal::CONT),
            Err(rustix::io::Errno::PERM)
        );
        assert!(
            std::net::TcpStream::connect(std::env::var("AHVM_SANDBOX_CHILD_TCP").unwrap()).is_err()
        );
        assert!(std::net::TcpListener::bind("127.0.0.1:0").is_err());
        assert!(std::net::UdpSocket::bind("127.0.0.1:0").is_ok());
        let abstract_addr =
            SocketAddr::from_abstract_name(std::env::var("AHVM_SANDBOX_CHILD_ABSTRACT").unwrap())
                .unwrap();
        assert!(std::os::unix::net::UnixStream::connect_addr(&abstract_addr).is_err());
        let address: usize = std::env::var("AHVM_SANDBOX_CHILD_MEMORY")
            .unwrap()
            .parse()
            .unwrap();
        let mut output = [0u8; 12];
        let local = libc::iovec {
            iov_base: output.as_mut_ptr().cast(),
            iov_len: output.len(),
        };
        let remote = libc::iovec {
            iov_base: address as *mut libc::c_void,
            iov_len: output.len(),
        };
        // Read-only probe against this test's parent and its live synthetic buffer.
        #[allow(unsafe_code)]
        let result = unsafe { libc::process_vm_readv(parent, &local, 1, &remote, 1, 0) };
        assert_eq!(result, -1);
        assert_eq!(
            io::Error::last_os_error().kind(),
            io::ErrorKind::PermissionDenied
        );
        let peer = std::os::unix::net::UnixStream::connect(dir.join("vm-b/control.sock"));
        if pathname_sockets {
            assert!(peer.is_err());
        } else {
            assert!(
                peer.is_ok(),
                "ABI 6-8 pathname socket gap must remain visible in qualification"
            );
        }
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join("vm-b/disk"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            fs::metadata(dir.join("vm-b/disk"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        println!("PASS own files/snapshot/socket; denied peer file contents, daemon state, symlink/hardlink escape, parent proc, process memory, signals, host TCP and abstract UDS; pathname UDS restricted={pathname_sockets}; UDP and peer chmod gaps remain");
    }
}

impl WorkerSandbox {
    /// Check the required ABI without changing the caller's access rights.
    #[cfg(target_os = "linux")]
    pub fn check_support() -> io::Result<()> {
        use landlock::{
            Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, Scope, ABI,
        };
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V6))
            .and_then(|rules| rules.scope(Scope::from_all(ABI::V6)))
            .and_then(|rules| rules.create())
            .map_err(|e| {
                io::Error::other(format!(
                    "requires enabled Landlock ABI 6 (Linux 6.12+): {e}"
                ))
            })?;
        Ok(())
    }

    /// Requires Landlock ABI 6: file access, device ioctls, ptrace isolation,
    /// cross-domain signals and abstract Unix sockets. Returns whether pathname
    /// Unix sockets are restricted too (ABI 9); older kernels cannot enforce that.
    #[cfg(target_os = "linux")]
    pub fn restrict(&self, allow_host_tcp: bool) -> io::Result<bool> {
        use landlock::{
            Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
            RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, ABI,
        };

        let file_access = AccessFs::from_file(ABI::V6);
        let mut builder = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V6))
            .map_err(io::Error::other)?
            .scope(Scope::from_all(ABI::V6))
            .map_err(io::Error::other)?;
        if !allow_host_tcp {
            builder = builder
                .handle_access(AccessNet::from_all(ABI::V6))
                .map_err(io::Error::other)?;
        }
        let mut rules = builder.create().map_err(io::Error::other)?;
        for (paths, access) in [
            (&self.read_only, AccessFs::from_read(ABI::V6)),
            (&self.read_write, AccessFs::from_all(ABI::V6)),
        ] {
            for path in paths {
                let allowed = if path.is_dir() {
                    access
                } else {
                    access & file_access
                };
                rules = rules
                    .add_rule(PathBeneath::new(
                        PathFd::new(path).map_err(io::Error::other)?,
                        allowed,
                    ))
                    .map_err(io::Error::other)?;
            }
        }

        // Open optional rule FDs before the first layer makes unrelated paths inaccessible.
        let mut sockets = Ruleset::default()
            .handle_access(AccessFs::ResolveUnix)
            .map_err(io::Error::other)?
            .create()
            .map_err(io::Error::other)?;
        for path in self.read_write.iter().chain(&self.unix_connect) {
            if !path.is_dir() {
                continue;
            }
            sockets = sockets
                .add_rule(PathBeneath::new(
                    PathFd::new(path).map_err(io::Error::other)?,
                    AccessFs::ResolveUnix,
                ))
                .map_err(io::Error::other)?;
        }
        let status = rules.restrict_self().map_err(io::Error::other)?;
        if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
            return Err(io::Error::other(
                "worker filesystem sandbox was not fully enforced",
            ));
        }
        Ok(sockets.restrict_self().map_err(io::Error::other)?.ruleset
            == RulesetStatus::FullyEnforced)
    }
}
