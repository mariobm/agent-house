//! Command execution: spawn, wait with a wall-clock cap, capture both
//! streams up to a per-stream budget that fits one response frame.
//!
//! Bulk output does NOT fit here by design (frames cap at 1 MiB incl.
//! base64 overhead): single-shot exec captures [`STREAM_CAP`] bytes per
//! stream and sets `truncated`. Anything bigger uses sessions (next chunk),
//! which stream output across frames.

use crate::agent::{ExecReq, ExecResp};
use crate::config::Config;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Per-stream capture budget for single-frame responses (256 KiB raw ->
/// ~350 KiB base64 JSON, comfortably inside the 1 MiB frame cap).
pub const STREAM_CAP: usize = 256 << 10;

pub fn run(req: &ExecReq, cfg: &Config) -> ExecResp {
    if req.argv.is_empty() {
        return ExecResp {
            exit_code: 127,
            stdout_b64: String::new(),
            stderr_b64: crate::agent::b64(b"empty argv"),
            truncated: false,
        };
    }
    let mut cmd = Command::new(&req.argv[0]);
    cmd.args(&req.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    use std::os::unix::process::CommandExt;
    // Own process group: on timeout we kill the whole tree, not just the
    // direct child — descendants holding the pipes would otherwise wedge
    // the drain threads forever (e.g. `sh -c 'sleep 3 & wait'`).
    // Unix-only (daemon runs Linux; macOS dev too). Windows needs a Job
    // object port if forge ever targets it.
    //
    // SAFETY: setpgid(0, 0) in the child between fork and exec touches no
    // shared state; async-signal-safe; the only failure mode is a failed
    // exec, which Rust turns into a spawn error.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecResp {
                exit_code: 127,
                stdout_b64: String::new(),
                stderr_b64: crate::agent::b64(format!("spawn: {e}").as_bytes()),
                truncated: false,
            };
        }
    };
    // Group id == leader pid after the setpgid above; signals the whole tree.
    let pgid = child.id() as i32;
    let cap = cfg.max_output_bytes.min(STREAM_CAP);
    // Drain both pipes on dedicated threads BEFORE waiting: the child
    // blocks past 64 KiB if nobody reads, so wait-then-drain deadlocks on
    // any output larger than the pipe buffer. Threads append into shared
    // capped buffers, so a response can always carry partial output even
    // when readers never see EOF.
    let out_buf = SharedBuf::default();
    let err_buf = SharedBuf::default();
    std::thread::spawn({
        let stdout = child.stdout.take();
        let shared = out_buf.clone();
        move || drain_shared(stdout, cap, &shared)
    });
    std::thread::spawn({
        let stderr = child.stderr.take();
        let shared = err_buf.clone();
        move || drain_shared(stderr, cap, &shared)
    });
    let exec_deadline = Instant::now() + Duration::from_secs(cfg.exec_timeout_secs);
    let mut status = None;
    let mut timed_out = false;
    while Instant::now() < exec_deadline {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    if status.is_none() {
        // Direct child still alive past the deadline: kill the whole process
        // group, not just the leader, so orphaned grandchildren can't hold
        // the pipes open.
        timed_out = true;
        kill_group(pgid);
        status = child.wait().ok();
    }
    if status.is_none() {
        // Direct child still alive past the deadline: kill the whole process
        // group, not just the leader, so orphaned grandchildren can't hold
        // the pipes open.
        timed_out = true;
        kill_group(pgid);
        status = child.wait().ok();
        // Readers may still be draining a large tail (or blocked on pipes
        // held by strays the kill missed): bound the wait, then respond
        // with whatever was captured.
        let end = Instant::now() + Duration::from_secs(5);
        while !(out_buf.done() && err_buf.done()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    } else {
        // Parent exited on its own. Pipes should EOF immediately — unless a
        // stray descendant inherited them (`echo out; sleep 30 &`). Brief
        // grace for in-flight tail, then kill the group and take partial
        // output rather than hanging on the orphan.
        let end = Instant::now() + Duration::from_secs(2);
        while !(out_buf.done() && err_buf.done()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if !(out_buf.done() && err_buf.done()) {
        // Pipes still open: someone is alive in the group (or an unrelated
        // double-forked inheritor — killpg is still scoped to our pgid).
        timed_out = true;
        kill_group(pgid);
        let _ = child.wait();
        let end = Instant::now() + Duration::from_secs(2);
        while !(out_buf.done() && err_buf.done()) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let (stdout, t1) = out_buf.take();
    let (stderr, t2) = err_buf.take();
    let exit_code = status.and_then(|s| s.code()).unwrap_or(124);
    ExecResp {
        exit_code,
        stdout_b64: crate::agent::b64(&stdout),
        stderr_b64: crate::agent::b64(&stderr),
        truncated: t1 || t2 || timed_out,
    }
}

/// Send SIGKILL to a process group.
///
/// SAFETY: targets only `pgid`, which the caller sets to the spawned
/// leader's pid (group id == leader pid after the setpgid in `run`). Worst
/// case the group is already gone and the kill fails. Callers only invoke
/// this while pipes are provably held open past a deadline, which means
/// group members are almost surely still alive — narrowing the pid-reuse
/// window to a just-observed live group.
#[allow(unsafe_code)]
fn kill_group(pgid: i32) {
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Shared capture buffer: reader threads append until EOF or the byte cap;
/// the main thread can take partial contents at any deadline.
#[derive(Debug, Default, Clone)]
struct SharedBuf {
    inner: std::sync::Arc<std::sync::Mutex<SharedState>>,
}

#[derive(Debug, Default)]
struct SharedState {
    buf: Vec<u8>,
    truncated: bool,
    done: bool,
}

impl SharedBuf {
    fn done(&self) -> bool {
        self.inner.lock().map(|s| s.done).unwrap_or(true)
    }

    fn take(self) -> (Vec<u8>, bool) {
        match self.inner.lock() {
            Ok(s) => (s.buf.clone(), s.truncated),
            Err(_) => (Vec::new(), true),
        }
    }
}

fn drain_shared(pipe: Option<impl Read>, cap: usize, shared: &SharedBuf) {
    let (mut buf, mut truncated) = (Vec::new(), false);
    if let Some(mut pipe) = pipe {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(buf.len());
                    if room == 0 {
                        truncated = true;
                    } else {
                        buf.extend_from_slice(&chunk[..n.min(room)]);
                        if n > room {
                            truncated = true;
                        }
                    }
                }
                Err(_) => {
                    truncated = true;
                    break;
                }
            }
        }
    }
    if let Ok(mut s) = shared.inner.lock() {
        s.buf = buf;
        s.truncated = truncated;
        s.done = true;
    }
}
