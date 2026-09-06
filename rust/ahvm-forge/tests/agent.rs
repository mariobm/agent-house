//! Live-agent tests: spawn `ahvm-forge` on an ephemeral port and speak v2.
//! Sessions/PTY are deferred; exec + files + auth are covered here.

use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::TcpStream;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Agent {
    child: Child,
    addr: String,
    dir: std::path::PathBuf,
}

struct AgentConn {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

/// Block until the child prints its bound address (or dies/exhausts).
fn wait_listen_addr(err: &mut BufReader<std::process::ChildStderr>, child: &mut Child) -> String {
    use std::io::BufRead as _;
    let mut line = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        line.clear();
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for forge listen address");
        }
        match err.read_line(&mut line) {
            Ok(0) => {
                let _ = child.try_wait();
                panic!("forge stderr closed before listen address");
            }
            Ok(_) => {
                if let Some(addr) = line.strip_prefix("forge: listening on ") {
                    let addr = addr.trim().to_owned();
                    // Confirm connectable before returning.
                    for _ in 0..50 {
                        if TcpStream::connect(&addr).is_ok() {
                            return addr;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    panic!("forge reported {addr} but it never accepted");
                }
            }
            Err(e) => panic!("reading forge stderr: {e}"),
        }
    }
}

impl Agent {
    fn spawn(token: &str) -> Self {
        Self::spawn_with_timeout(token, 2)
    }

    /// Short timeouts keep the suite fast; the bulk-output test opts into
    /// a longer one explicitly.
    fn spawn_with_timeout(token: &str, exec_timeout_secs: u64) -> Self {        let bin = env!("CARGO_BIN_EXE_ahvm-forge");
        // No port probing: bind :0, spawn, and read the bound address back
        // from the child's stderr. Probing (bind-then-release-then-rebind)
        // lets a parallel test steal the port between release and rebind.
        let dir = std::env::temp_dir().join(format!(
            "forge-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new(bin)
            .env("AHVM_FORGE_LISTEN", "127.0.0.1:0")
            .env("AHVM_FORGE_TOKEN", token)
            .env("AHVM_FORGE_ROOT", &dir)
            .env("AHVM_FORGE_EXEC_TIMEOUT", exec_timeout_secs.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut err = std::io::BufReader::new(child.stderr.take().unwrap());
        let addr = wait_listen_addr(&mut err, &mut child);
        Self { child, addr, dir }
    }

    fn connect(&self) -> AgentConn {
        let w = TcpStream::connect(&self.addr).unwrap();
        w.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let r = BufReader::new(w.try_clone().unwrap());
        AgentConn { r, w }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn auth(c: &mut AgentConn, token: &str) {
    let body = serde_json::json!({ "token": token }).to_string().into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::Auth, payload: body }).unwrap();
}

fn exec(c: &mut AgentConn, argv: &[&str]) -> serde_json::Value {
    let body = serde_json::json!({ "argv": argv }).to_string().into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::ExecReq, payload: body }).unwrap();
    let f = read_frame(&mut c.r).unwrap();
    assert_eq!(f.msg_type, FrameType::ExecResp);
    serde_json::from_slice(&f.payload).unwrap()
}

fn expect_error(c: &mut AgentConn) -> String {
    let f = read_frame(&mut c.r).unwrap();
    assert_eq!(f.msg_type, FrameType::Error);
    let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
    v["message"].as_str().unwrap().to_owned()
}

fn b64str(v: &serde_json::Value, k: &str) -> Vec<u8> {
    B64.decode(v[k].as_str().unwrap()).unwrap()
}

#[test]
fn exec_no_auth_echo() {
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    let out = exec(&mut c, &["echo", "hello"]);
    assert_eq!(out["exit_code"], 0);
    assert_eq!(b64str(&out, "stdout_b64"), b"hello\n");
}

#[test]
fn wrong_token_rejected() {
    let agent = Agent::spawn("secret");
    let mut c = agent.connect();
    auth(&mut c, "wrong");
    let msg = expect_error(&mut c);
    assert!(msg.contains("auth"), "{msg}");
}

#[test]
fn token_auth_exec_and_env() {
    let agent = Agent::spawn("secret");
    let mut c = agent.connect();
    auth(&mut c, "secret");
    let body = serde_json::json!({
        "argv": ["sh", "-c", "printf %s \"$GREETING\""],
        "env": HashMap::from([("GREETING".to_string(), "hi".to_string())]),
    })
    .to_string()
    .into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::ExecReq, payload: body }).unwrap();
    let f = read_frame(&mut c.r).unwrap();
    assert_eq!(f.msg_type, FrameType::ExecResp);
    let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
    assert_eq!(b64str(&v, "stdout_b64"), b"hi");
    assert_eq!(v["exit_code"], 0);
}

#[test]
fn exec_missing_binary_reports_127() {
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    let out = exec(&mut c, &["definitely-not-a-real-binary-xyz"]);
    assert_eq!(out["exit_code"], 127);
}

#[test]
fn exec_killed_reports_kill_bucket() {
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    let out = exec(&mut c, &["sh", "-c", "kill -9 $$"]);
    assert_eq!(out["exit_code"], 124);
}

#[test]
fn file_write_read_list_roundtrip() {
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    let data = B64.encode(b"file-contents-123");
    let body = serde_json::json!({ "op": "write", "path": "sub/dir/note.txt", "data_b64": data })
        .to_string()
        .into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::FileReq, payload: body }).unwrap();
    let f = read_frame(&mut c.r).unwrap();
    assert_eq!(f.msg_type, FrameType::FileResp);

    let body = serde_json::json!({ "op": "read", "path": "sub/dir/note.txt", "offset": 5, "limit": 8 })
        .to_string()
        .into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::FileReq, payload: body }).unwrap();
    let f = read_frame(&mut c.r).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
    assert_eq!(b64str(&v, "data_b64"), b"contents");

    let body = serde_json::json!({ "op": "list", "path": "sub/dir", "offset": 0, "limit": 100 }).to_string().into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::FileReq, payload: body }).unwrap();
    let f = read_frame(&mut c.r).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
    assert_eq!(v["entries"][0]["name"], "note.txt");
    assert_eq!(v["next_offset"], serde_json::Value::Null);
}

#[test]
fn file_traversal_rejected() {
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    for evil in ["../evil.txt", "sub/../../evil.txt"] {
        let body = serde_json::json!({ "op": "read", "path": evil, "offset": 0, "limit": 8 })
            .to_string()
            .into_bytes();
        write_frame(&mut c.w, &Frame { msg_type: FrameType::FileReq, payload: body }).unwrap();
        let msg = expect_error(&mut c);
        assert!(msg.contains("jail"), "{msg}");
    }
}

#[test]
fn output_is_capped() {
    // Bulk drain needs headroom; every other test runs on the short timeout.
    let agent = Agent::spawn_with_timeout("", 15);
    let mut c = agent.connect();
    // 30 MiB of output against the 256 KiB single-response cap.
    let out = exec(&mut c, &["sh", "-c", "head -c 30000000 /dev/zero | tr '\\0' x"]);
    assert_eq!(out["truncated"], true);
    assert_eq!(b64str(&out, "stdout_b64").len(), 256 << 10);
}

fn file_op(c: &mut AgentConn, body: serde_json::Value) -> serde_json::Value {
    let body = body.to_string().into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::FileReq, payload: body }).unwrap();
    let f = read_frame(&mut c.r).unwrap();
    assert_eq!(f.msg_type, FrameType::FileResp);
    serde_json::from_slice(&f.payload).unwrap()
}

#[test]
fn write_beats_planted_symlink_tmp() {
    // Regression: predictable `<name>.ahvm-tmp` let a pre-planted symlink
    // redirect the write outside the jail (and destroyed the planter file).
    let agent = Agent::spawn("");
    // Plant the exact legacy temp name as a symlink pointing outside.
    let outside = agent.dir.join("outside.txt");
    std::fs::write(&outside, b"untouched").unwrap();
    std::os::unix::fs::symlink(&outside, agent.dir.join("note.ahvm-tmp")).unwrap();
    let mut c = agent.connect();
    let data = B64.encode(b"hello");
    let v = file_op(
        &mut c,
        serde_json::json!({ "op": "write", "path": "note.txt", "data_b64": data }),
    );
    assert_eq!(v["bytes"], 5);
    // Target written through the unique temp, not the planted link...
    let got = std::fs::read(agent.dir.join("note.txt")).unwrap();
    assert_eq!(got, b"hello");
    // ...and the outside file plus the planted link are intact.
    assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
    assert!(std::fs::symlink_metadata(agent.dir.join("note.ahvm-tmp"))
        .unwrap()
        .is_symlink());
}

#[test]
fn exec_timeout_kills_descendants() {
    // `sleep 45 & wait`: killing only the direct child leaves the
    // grandchild holding the pipes, which used to wedge the response forever.
    // Unique sleep duration doubles as a leak marker (see assert_no_stray).
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    c.w
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let start = std::time::Instant::now();
    let body = serde_json::json!({ "argv": ["sh", "-c", "sleep 45 & wait"] })
        .to_string()
        .into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::ExecReq, payload: body }).unwrap();
    let f = read_frame(&mut c.r).unwrap();
    assert_eq!(f.msg_type, FrameType::ExecResp);
    let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
    assert_eq!(v["exit_code"], 124);
    assert_eq!(v["truncated"], true);
    assert!(start.elapsed() < Duration::from_secs(12), "took {:?}", start.elapsed());
    assert_no_stray("sleep 45");
}

/// Fail if a process matching `pattern` (full command line) is observable.
/// The group kill must reap descendants; anything left is a leak that would
/// accumulate across requests.
fn assert_no_stray(pattern: &str) {
    for _ in 0..30 {
        let out = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(pattern)
            .output()
            .expect("pgrep missing");
        if out.stdout.is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("leaked process matching {pattern:?}");
}

#[test]
fn exec_exited_parent_with_live_descendant_still_responds() {
    // Parent exits at once, grandchild keeps the pipe open: the response
    // must still arrive (bounded drain), not hang on the orphan.
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    c.w
        .set_read_timeout(Some(Duration::from_secs(25)))
        .unwrap();
    let body = serde_json::json!({ "argv": ["sh", "-c", "echo out; sleep 46 &"] })
        .to_string()
        .into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::ExecReq, payload: body }).unwrap();
    let start = std::time::Instant::now();
    let f = read_frame(&mut c.r).unwrap();
    assert_eq!(f.msg_type, FrameType::ExecResp);
    let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
    // Parent exited 0 but a stray held the pipe: partial output preserved,
    // flagged truncated, and answered in ~2s grace — not at stray death.
    assert_eq!(b64str(&v, "stdout_b64"), b"out\n");
    assert_eq!(v["truncated"], true);
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "took {:?}",
        start.elapsed()
    );
    assert_no_stray("sleep 46");
}

#[test]
fn large_listing_pages_instead_of_dropping() {
    // 5,000 long filenames: used to exceed the 1 MiB frame and kill the
    // connection with no response. Now pages through with next_offset.
    let agent = Agent::spawn("");
    let big = agent.dir.join("big");
    std::fs::create_dir_all(&big).unwrap();
    for i in 0..5000 {
        std::fs::write(big.join(format!("file-{i:05}-with-a-long-name-to-bloat-the-listing.txt")), b"x")
            .unwrap();
    }
    let mut c = agent.connect();
    let mut seen = 0usize;
    let mut offset = 0u64;
    loop {
        let v = file_op(
            &mut c,
            serde_json::json!({ "op": "list", "path": "big", "offset": offset, "limit": 1000 }),
        );
        let n = v["entries"].as_array().unwrap().len();
        seen += n;
        match v["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
        assert!(seen < 6000, "paging looped");
    }
    assert_eq!(seen, 5000);
}

#[test]
fn long_filename_write_works() {
    // Regression: temp names embedding the full stem overflowed NAME_MAX
    // on long destinations (and retried the permanent error 100x).
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    let long = "n".repeat(240);
    let data = B64.encode(b"long-name-ok");
    let v = file_op(
        &mut c,
        serde_json::json!({ "op": "write", "path": long, "data_b64": data }),
    );
    assert_eq!(v["bytes"], 12);
    assert_eq!(std::fs::read(agent.dir.join(&long)).unwrap(), b"long-name-ok");
}

#[test]
fn failed_write_leaves_no_tmp() {
    // Writing over an existing directory fails at rename; the temp file
    // must not be left behind to accumulate across failures.
    let agent = Agent::spawn("");
    std::fs::create_dir_all(agent.dir.join("adir")).unwrap();
    let mut c = agent.connect();
    let data = B64.encode(b"x");
    let body = serde_json::json!({ "op": "write", "path": "adir", "data_b64": data })
        .to_string()
        .into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::FileReq, payload: body }).unwrap();
    let msg = expect_error(&mut c);
    assert!(msg.contains("rename"), "{msg}");
    let leftovers: Vec<_> = std::fs::read_dir(&agent.dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().starts_with(".ahvm-tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "stray temps: {leftovers:?}");
    // Connection still usable after the error.
    let out = exec(&mut c, &["echo", "alive"]);
    assert_eq!(out["exit_code"], 0);
}
