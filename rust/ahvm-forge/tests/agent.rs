//! Live-agent tests: spawn `ahvm-forge` on an ephemeral port and speak v2.
//! Sessions/PTY are deferred; exec + files + auth are covered here.

use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use std::collections::HashMap;
use std::io::BufReader;
use std::net::{TcpListener, TcpStream};
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

impl Agent {
    fn spawn(token: &str) -> Self {
        let bin = env!("CARGO_BIN_EXE_ahvm-forge");
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap().to_string();
        drop(probe);
        let dir = std::env::temp_dir().join(format!(
            "forge-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new(bin)
            .env("AHVM_FORGE_LISTEN", &addr)
            .env("AHVM_FORGE_TOKEN", token)
            .env("AHVM_FORGE_ROOT", &dir)
            .env("AHVM_FORGE_EXEC_TIMEOUT", "20")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if TcpStream::connect(&addr).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            if child.try_wait().unwrap().is_some() {
                panic!("forge exited early");
            }
        }
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

    let body = serde_json::json!({ "op": "list", "path": "sub/dir" }).to_string().into_bytes();
    write_frame(&mut c.w, &Frame { msg_type: FrameType::FileReq, payload: body }).unwrap();
    let f = read_frame(&mut c.r).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
    assert_eq!(v["entries"][0]["name"], "note.txt");
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
    let agent = Agent::spawn("");
    let mut c = agent.connect();
    // 30 MiB of output against the 256 KiB single-response cap.
    let out = exec(&mut c, &["sh", "-c", "head -c 30000000 /dev/zero | tr '\\0' x"]);
    assert_eq!(out["truncated"], true);
    assert_eq!(b64str(&out, "stdout_b64").len(), 256 << 10);
}
