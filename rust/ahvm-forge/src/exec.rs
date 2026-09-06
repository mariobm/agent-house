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
    let cap = cfg.max_output_bytes.min(STREAM_CAP);
    // Drain both pipes on dedicated threads BEFORE waiting: the child
    // blocks past 64 KiB if nobody reads, so wait-then-drain deadlocks on
    // any output larger than the pipe buffer.
    let out_thr = std::thread::spawn({
        let stdout = child.stdout.take();
        move || drain(stdout, cap)
    });
    let err_thr = std::thread::spawn({
        let stderr = child.stderr.take();
        move || drain(stderr, cap)
    });
    let deadline = Instant::now() + Duration::from_secs(cfg.exec_timeout_secs);
    let mut status = None;
    while Instant::now() < deadline {
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
        let _ = child.kill();
        status = child.wait().ok();
    }
    let (stdout, t1) = out_thr.join().unwrap_or_default();
    let (stderr, t2) = err_thr.join().unwrap_or_default();
    let exit_code = status.and_then(|s| s.code()).unwrap_or(124);
    ExecResp {
        exit_code,
        stdout_b64: crate::agent::b64(&stdout),
        stderr_b64: crate::agent::b64(&stderr),
        truncated: t1 || t2,
    }
}

fn drain(pipe: Option<impl Read>, cap: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    if let Some(mut pipe) = pipe {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let room = cap.saturating_sub(out.len());
                    if room == 0 {
                        truncated = true;
                    } else {
                        out.extend_from_slice(&chunk[..n.min(room)]);
                        if n > room {
                            truncated = true;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }
    (out, truncated)
}
