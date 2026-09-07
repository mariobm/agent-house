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
///
/// `starttime` is the process start time (Linux `/proc` clock ticks since
/// boot) captured at spawn. After a supervisor restart, pids may have been
/// reused by unrelated processes: adoption must verify identity, never
/// trust the pid alone. `None` means unknown (legacy records, non-Linux);
/// callers fall back to plain liveness there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub pid: u32,
    pub sock_dir: PathBuf,
    pub state_path: PathBuf,
    #[serde(default)]
    pub starttime: Option<u64>,
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
/// child in `state.json`, and return a supervised handle.
///
/// The supervisor OWNS the [`std::process::Child`] handle: dropping a
/// `LiveWorker` without terminating leaks a zombie (nobody else can reap
/// our child). Use [`LiveWorker::terminate`] (kill + reap) or
/// [`LiveWorker::try_reap`] (reap-if-exited). If persisting the record fails,
/// the freshly spawned child is killed AND reaped before returning Err —
/// never orphaned.
///
/// [`is_alive`] remains for adopted pids (recovery after a supervisor
/// restart, where the OS reparented the worker and `wait` would ECHILD).
pub fn spawn_worker(
    vmm_binary: impl AsRef<OsStr>,
    spec_arg: impl AsRef<Path>,
    state_path: impl AsRef<Path>,
) -> std::io::Result<LiveWorker> {
    spawn_worker_cfg(&SpawnConfig {
        vmm_binary: vmm_binary.as_ref(),
        spec_arg: spec_arg.as_ref(),
        state_path: state_path.as_ref(),
        hermetic: false,
        env: &[],
        stderr_log: None,
    })
}

/// How to spawn a worker. [`spawn_worker`] is the plain form (inherited
/// environment, stdio nulled); the krucible backend uses the hermetic form
/// so daemon credentials and host config never leak into workers.
#[derive(Debug)]
pub struct SpawnConfig<'a> {
    pub vmm_binary: &'a OsStr,
    pub spec_arg: &'a Path,
    pub state_path: &'a Path,
    /// Clear the environment and set only `env` (plus nothing else).
    pub hermetic: bool,
    /// `KEY=VALUE` entries (the whole environment when hermetic).
    /// Malformed entries (no `=`) are ignored.
    pub env: &'a [String],
    /// Redirect worker stderr here instead of null. A dead-on-arrival
    /// worker must leave evidence instead of failing silently.
    pub stderr_log: Option<&'a Path>,
}
/// [`spawn_worker`] with environment and stderr control. The krucible
/// backend uses the hermetic form so daemon credentials and host config
/// never leak into workers (see [`SpawnConfig`]). Record discipline is
/// identical: persist `state.json` or kill+reap before returning Err.
pub fn spawn_worker_cfg(cfg: &SpawnConfig) -> std::io::Result<LiveWorker> {
    let state_path = cfg.state_path.to_path_buf();
    let mut cmd = Command::new(cfg.vmm_binary);
    cmd.arg(cfg.spec_arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    if cfg.hermetic {
        cmd.env_clear();
        for kv in cfg.env {
            if let Some((k, v)) = kv.split_once('=') {
                cmd.env(k, v);
            }
        }
    }
    match cfg.stderr_log {
        Some(log) => {
            cmd.stderr(fs::File::create(log)?);
        }
        None => {
            cmd.stderr(Stdio::null());
        }
    }
    let mut child = cmd.spawn()?;
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
        starttime: process_starttime(pid),
    };
    if let Err(e) = worker.persist() {
        // Record unwritten: no future owner can find this child. Kill and
        // reap it here so neither an orphan nor a zombie escapes.
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    Ok(LiveWorker {
        record: worker,
        child: Some(child),
    })
}

/// A [`Worker`] record plus ownership of the child handle. The ONLY type
/// allowed to reap the worker.
#[derive(Debug)]
pub struct LiveWorker {
    pub record: Worker,
    child: Option<std::process::Child>,
}

impl LiveWorker {
    pub fn id(&self) -> &str {
        &self.record.id
    }

    pub fn pid(&self) -> u32 {
        self.record.pid
    }

    /// Send one control command to this worker's socket.
    pub fn send(&self, cmd: &str) -> crate::Result<String> {
        send_ctl(self.record.control_socket(), cmd)
    }

    /// Non-blocking reap: `Some(status)` if the child already exited (no
    /// zombie left behind), `None` if still running.
    pub fn try_reap(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self.child.as_mut() {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    /// Stop the worker and reap it: SIGKILL, then blocking wait (bounded).
    /// After this returns Ok, no zombie remains regardless of prior state.
    /// Idempotent: a second call on an already-reaped worker is Ok.
    pub fn terminate(&mut self) -> std::io::Result<()> {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill(); // already-dead is fine
            let _ = child.wait(); // reap: never a zombie afterwards
        }
        Ok(())
    }
}

impl Drop for LiveWorker {
    /// Last-resort hygiene: a supervisor that forgets terminate() still
    /// reaps an exited child instead of leaking a zombie. A RUNNING child is
    /// deliberately left alive (supervisor crash must not kill VMs — the
    /// recovery path re-adopts by pid), but an exited one is reaped here.
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.try_wait();
        }
    }
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

/// Process start time for pid-identity checks (see [`Worker::starttime`]).
/// Linux: field 22 of `/proc/<pid>/stat` (clock ticks since boot). `None`
/// when the pid does not exist (or the platform has no `/proc`).
/// NEVER use this alone for liveness (a live pid may be someone else);
/// pair it with the recorded value.
pub fn process_starttime(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // `pid (comm) rest...` — comm may contain spaces and parens, so
        // split after the LAST ')'.
        let rest = stat.rsplit(')').next()?;
        // Fields after comm: state(1) ppid(2) ... starttime(22) is the
        // 20th whitespace field here (22 minus pid and comm).
        rest.split_whitespace().nth(19)?.parse().ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
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

/// SIGTERM an adopted pid (recovery path — no owned handle to wait on).
/// Prefer [`LiveWorker::terminate`] for supervised workers: this cannot reap,
/// so callers must poll [`is_alive`] until it goes false. Refuses pid 0.
pub fn terminate_adopted(pid: u32) -> crate::Result<()> {
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

    #[test]
    fn spawn_sleep_kill_reap_cycle() {
        let dir = crate::test_scratch("worker-spawn");
        let state = dir.join("sb-1").join("state.json");
        // "60" travels as the child's argv (sleep's duration), exercising
        // the spec-path-as-argv convention without a real VMM binary.
        let mut worker = spawn_worker("/bin/sleep", "60", &state).unwrap();
        assert_eq!(worker.id(), "sb-1");
        assert!(state.exists());
        assert!(is_alive(worker.pid()));
        assert!(worker.try_reap().unwrap().is_none());

        // state.json roundtrips through the spawned record.
        assert_eq!(Worker::load(&state).unwrap(), worker.record);

        worker.terminate().unwrap();
        assert!(!is_alive(worker.pid()), "sleep should be dead after terminate");
        // Idempotent: second terminate is Ok, try_reap finds nothing.
        worker.terminate().unwrap();
        assert!(worker.try_reap().unwrap().is_none());
    }

    #[test]
    fn repeated_cycles_leave_no_zombies() {
        // Guards the old leak (dropped Child handle, never waited): spawn
        // and terminate repeatedly, then assert no zombie of ours remains.
        // A hang here would flag a reaped-handle regression instead.
        for i in 0..10 {
            let dir = crate::test_scratch(&format!("worker-cycle-{i}"));
            let state = dir.join("sb").join("state.json");
            let mut worker = spawn_worker("/bin/sleep", "60", &state).unwrap();
            worker.terminate().unwrap();
        }
        let out = std::process::Command::new("/bin/ps")
            .args(["-o", "stat=,command="])
            .output()
            .unwrap();
        let zombies = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.trim_start().starts_with('Z') && l.contains("sleep 60"))
            .count();
        assert_eq!(zombies, 0, "leaked sleep zombies");
    }

    #[test]
    fn process_starttime_roundtrips_on_self() {
        // Our own pid is alive by definition; its starttime must be
        // stable across reads (Linux) or uniformly unknown elsewhere.
        let me = std::process::id();
        let t1 = process_starttime(me);
        let t2 = process_starttime(me);
        assert_eq!(t1, t2);
        #[cfg(target_os = "linux")]
        assert!(t1.is_some(), "own pid must have a starttime on Linux");
        // A pid that cannot exist has no starttime on any platform.
        assert_eq!(process_starttime(999_999_999), None);
    }

    #[test]
    fn is_alive_edge_cases() {
        assert!(!is_alive(0));
        assert!(!is_alive(999_999_999));
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn terminate_adopted_refuses_pid_zero() {
        assert!(terminate_adopted(0).is_err());
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
            starttime: None,
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
