//! Worker supervision: one child VMM process per sandbox.
//!
//! The daemon never links the VMM library. Instead it spawns one worker
//! process per sandbox (the real worker links libkrucible; `cmd/vmm`
//! played this role in Go) and supervises it out-of-band:
//!
//! * [`spawn_worker`] starts the binary detached-ish (stdio nulled, never
//!   waited on here) and records `{id, pid, sock_dir, state_path}` to
//!   `state.json` atomically (tmp file + rename), so a crash can never
//!   leave a torn state file.
//! * [`is_alive`] polls liveness with `kill(pid, 0)` semantics via
//!   `/bin/kill -0` (no libc, no unsafe), refined with a `ps` zombie
//!   check: a correctly-signalled child nobody reaped still answers
//!   `kill(2)` but is dead for our purposes.
//! * [`send_ctl`] speaks the worker's control-socket line protocol: one
//!   newline-terminated command in, one line out, then close (`PAUSE` /
//!   `RESUME` / `STATUS` / `SNAPSHOT <dir>`).
//!
//! Everything here is std-only so it compiles and tests on macOS without
//! KVM or any VMM dependency.

use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Read/write timeout for one control-socket round trip.
const CTL_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on one control response line; the protocol speaks short `OK ...`
/// / `ERR ...` lines, never bulk data.
const MAX_CTL_LINE: usize = 64 * 1024;

/// Supervised worker record. `id` is the sandbox id (the name of the
/// sandbox dir holding `state.json`); `sock_dir` is that dir, which also
/// hosts the worker's control socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub pid: u32,
    pub sock_dir: PathBuf,
    pub state_path: PathBuf,
}

impl Worker {
    /// Atomically persist this record to [`Worker::state_path`].
    pub fn persist(&self) -> std::io::Result<()> {
        write_state_file(&self.state_path, self)
    }

    /// Read back a record written by [`Worker::persist`].
    pub fn load(state_path: impl AsRef<Path>) -> std::io::Result<Self> {
        let raw = fs::read(state_path.as_ref())?;
        serde_json::from_slice(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Path of the worker's control socket inside [`Worker::sock_dir`].
    pub fn control_socket(&self) -> PathBuf {
        self.sock_dir.join("control.sock")
    }

    /// Send one control command to this worker's socket.
    pub fn send(&self, cmd: &str) -> crate::Result<String> {
        send_ctl(self.control_socket(), cmd)
    }
}

/// Spawn the VMM worker binary with the spec path as its argv, record the
/// child in `state.json`, and return the record.
///
/// The child is intentionally detached-ish: stdio is nulled and the handle
/// is dropped without waiting, so the worker survives the spawner. That
/// also means the spawner never reaps it — use [`is_alive`] (zombie-aware)
/// to poll and [`terminate`] to stop it.
pub fn spawn_worker(
    vmm_binary: impl AsRef<OsStr>,
    spec_arg: impl AsRef<Path>,
    state_path: impl AsRef<Path>,
) -> std::io::Result<Worker> {
    let state_path = state_path.as_ref().to_path_buf();
    let child = Command::new(vmm_binary)
        .arg(spec_arg.as_ref())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Dropped without wait: the worker keeps running under the spawner.
    let pid = child.id();
    let sock_dir = state_path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let id = sock_dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "worker".to_string());
    let worker = Worker {
        id,
        pid,
        sock_dir,
        state_path,
    };
    worker.persist()?;
    Ok(worker)
}

fn write_state_file(path: &Path, worker: &Worker) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("tmp");
    let raw = serde_json::to_vec_pretty(worker)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(&tmp, raw)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// True when `pid` names a live process: `kill(pid, 0)` succeeds *and* the
/// process is not a zombie. Implemented via `/bin/kill -0` + `/bin/ps`
/// (absolute paths, std only) so the crate stays unsafe-free.
pub fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let pid_str = pid.to_string();
    let exists = Command::new("/bin/kill")
        .arg("-0")
        .arg(&pid_str)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    exists && !is_zombie(&pid_str)
}

fn is_zombie(pid_str: &str) -> bool {
    Command::new("/bin/ps")
        .args(["-p", pid_str, "-o", "stat="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|o| {
            o.status.success()
                && String::from_utf8_lossy(&o.stdout)
                    .trim_start()
                    .starts_with('Z')
        })
}

/// SIGTERM `pid`. The death itself is asynchronous — poll [`is_alive`]
/// until it goes false.
pub fn terminate(pid: u32) -> crate::Result<()> {
    if pid == 0 {
        return Err(crate::Error::Control(
            "refusing to signal pid 0".to_string(),
        ));
    }
    let status = Command::new("/bin/kill")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(crate::Error::Control(format!(
            "kill {pid} failed with status {status}"
        )))
    }
}

/// Send one control command (`PAUSE`, `RESUME`, `STATUS`,
/// `SNAPSHOT <dir>`, ...) to the worker socket at `sock_path`: one line
/// in, one line out (without the terminator), then close. 5s read/write
/// timeouts; anything off-protocol (connect failure, timeout, overlong or
/// non-UTF8 reply) is an error.
pub fn send_ctl(sock_path: impl AsRef<Path>, cmd: &str) -> crate::Result<String> {
    let path = sock_path.as_ref();
    let cmd = cmd.trim_end_matches(['\r', '\n']);
    if cmd.is_empty() {
        return Err(crate::Error::Control("empty control command".to_string()));
    }
    if cmd.bytes().any(|b| b == b'\n' || b == b'\r') {
        return Err(crate::Error::Control(
            "control command must be a single line".to_string(),
        ));
    }
    let mut stream = UnixStream::connect(path)
        .map_err(|e| crate::Error::Control(format!("connect {}: {e}", path.display())))?;
    stream.set_read_timeout(Some(CTL_TIMEOUT))?;
    stream.set_write_timeout(Some(CTL_TIMEOUT))?;
    stream.write_all(cmd.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                if out.len() >= MAX_CTL_LINE {
                    return Err(crate::Error::Control("control reply too long".to_string()));
                }
                out.push(byte[0]);
            }
            Err(e) => return Err(crate::Error::Io(e)),
        }
    }
    if out.last() == Some(&b'\r') {
        out.pop();
    }
    String::from_utf8(out)
        .map_err(|e| crate::Error::Control(format!("control reply is not utf-8: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Instant;

    #[test]
    fn spawn_sleep_kill_reap_cycle() {
        let dir = crate::test_scratch("worker-spawn");
        let state = dir.join("sb-1").join("state.json");
        // "60" travels as the child's argv (sleep's duration), exercising
        // the spec-path-as-argv convention without a real VMM binary.
        let worker = spawn_worker("/bin/sleep", "60", &state).unwrap();
        assert_eq!(worker.id, "sb-1");
        assert!(state.exists());
        assert!(is_alive(worker.pid));

        // state.json roundtrips through the spawned record.
        assert_eq!(Worker::load(&state).unwrap(), worker);

        terminate(worker.pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_alive(worker.pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!is_alive(worker.pid), "sleep should be dead after SIGTERM");
    }

    #[test]
    fn is_alive_edge_cases() {
        assert!(!is_alive(0));
        assert!(!is_alive(999_999_999));
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn terminate_refuses_pid_zero() {
        assert!(terminate(0).is_err());
    }

    #[test]
    fn worker_state_write_read_roundtrip() {
        let dir = crate::test_scratch("worker-state");
        let state = dir.join("sb-9").join("state.json");
        let worker = Worker {
            id: "sb-9".to_string(),
            pid: 1234,
            sock_dir: dir.join("sb-9"),
            state_path: state.clone(),
        };
        worker.persist().unwrap();
        assert_eq!(Worker::load(&state).unwrap(), worker);
        assert_eq!(
            worker.control_socket(),
            dir.join("sb-9").join("control.sock")
        );
    }

    #[test]
    fn control_client_speaks_line_protocol() {
        let dir = crate::test_scratch("worker-ctl");
        let sock = dir.join("control.sock");
        let server = UnixListener::bind(&sock).unwrap();
        let handle = thread::spawn(move || {
            let (conn, _) = server.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.trim_end(), "STATUS");
            conn.try_clone()
                .unwrap()
                .write_all(b"OK running vcpus=2\r\n")
                .unwrap();
        });
        let reply = send_ctl(&sock, "STATUS").unwrap();
        assert_eq!(reply, "OK running vcpus=2");
        handle.join().unwrap();
    }

    #[test]
    fn control_client_rejects_bad_input_and_missing_socket() {
        let dir = crate::test_scratch("worker-ctl-err");
        let missing = dir.join("nope.sock");
        assert!(send_ctl(&missing, "STATUS").is_err());
        assert!(send_ctl(&missing, "").is_err());
        assert!(send_ctl(&missing, "A\nB").is_err());
    }
}
