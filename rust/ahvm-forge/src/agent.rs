//! Per-connection dispatch: optional AUTH gate, then one request loop
//! serving EXEC_REQ and FILE_REQ frames until EOF or protocol violation.

use crate::config::Config;
use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use std::io::{self, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;

/// Accepted connection: TCP in tests/dev, AF_VSOCK in the guest. Both are
/// byte-stream sockets, so one enum with Read/Write/try_clone keeps the
/// protocol logic transport-agnostic.
#[derive(Debug)]
pub enum Conn {
    Tcp(TcpStream),
    Vsock(UnixStream),
}

impl From<TcpStream> for Conn {
    fn from(s: TcpStream) -> Self {
        Self::Tcp(s)
    }
}

impl Conn {
    pub fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Tcp(s) => s.try_clone().map(Self::Tcp),
            Self::Vsock(s) => s.try_clone().map(Self::Vsock),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            Self::Vsock(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            Self::Vsock(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            Self::Vsock(s) => s.flush(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct AuthBody {
    token: String,
}

#[derive(Debug, Deserialize)]
pub struct ExecReq {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExecResp {
    pub exit_code: i32,
    pub stdout_b64: String,
    pub stderr_b64: String,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileReq {
    Read {
        path: String,
        offset: u64,
        limit: u64,
    },
    Write {
        path: String,
        data_b64: String,
    },
    List {
        path: String,
        offset: u64,
        limit: u64,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileResp {
    Read {
        data_b64: String,
        eof: bool,
    },
    Write {
        bytes: u64,
    },
    List {
        entries: Vec<DirEntry>,
        next_offset: Option<u64>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SessionReq {
    Create {
        argv: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        cwd: Option<String>,
        #[serde(default)]
        pty: bool,
    },
    Attach {
        session_id: String,
        #[serde(default)]
        from_seq: u64,
    },
    Input {
        session_id: String,
        data_b64: String,
    },
    Kill {
        session_id: String,
    },
    Resize {
        session_id: String,
        rows: u16,
        cols: u16,
    },
    Delete {
        session_id: String,
    },
    List,
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SessionResp {
    Started {
        session_id: String,
    },
    InputAcked {
        bytes: u64,
    },
    Killed {
        session_id: String,
    },
    Resized {
        session_id: String,
    },
    Deleted {
        session_id: String,
    },
    Listed {
        sessions: Vec<crate::sessions::SessionInfo>,
    },
}

#[derive(Debug, Serialize)]
pub struct SessionData {
    pub session_id: String,
    pub seq: u64,
    pub data_b64: String,
    pub eof: bool,
    pub exit_code: Option<i32>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

fn err_frame(message: String) -> Frame {
    let body = serde_json::json!({ "message": message })
        .to_string()
        .into_bytes();
    Frame {
        msg_type: FrameType::Error,
        payload: body,
    }
}

fn parse<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, Frame> {
    serde_json::from_slice(payload).map_err(|e| err_frame(format!("bad request: {e}")))
}

pub fn handle(stream: Conn, cfg: &Config) {
    let mut r = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut w = stream;

    // Auth gate: token configured => first frame MUST be AUTH with the token.
    if !cfg.token.is_empty() {
        let frame = match read_frame(&mut r) {
            Ok(f) => f,
            Err(e) => {
                let _ = write_frame(&mut w, &err_frame(format!("auth required: {e}")));
                return;
            }
        };
        let ok = frame.msg_type == FrameType::Auth
            && parse::<AuthBody>(&frame.payload)
                .map(|b| constant_eq(&b.token, &cfg.token))
                .unwrap_or(false);
        if !ok {
            let _ = write_frame(&mut w, &err_frame("auth required".into()));
            return;
        }
    }

    loop {
        let frame = match read_frame(&mut r) {
            Ok(f) => f,
            Err(ahvm_proto::v2::Error::CleanEof) => return,
            Err(e) => {
                let _ = write_frame(&mut w, &err_frame(format!("read: {e}")));
                return;
            }
        };
        // One serialization per response: the payload is encoded once here,
        // so handlers never double-encode to pre-check sizes. Anything past
        // the frame budget becomes an explicit error frame — the connection
        // stays usable, unlike a failed write that just drops it.
        let (msg_type, body): (FrameType, Vec<u8>) = match frame.msg_type {
            FrameType::ExecReq => match parse::<ExecReq>(&frame.payload) {
                Ok(req) => {
                    let out = crate::exec::run(&req, cfg);
                    let body = serde_json::to_vec(&out).expect("serialize exec resp");
                    (FrameType::ExecResp, body)
                }
                Err(f) => {
                    send_frame(&mut w, f);
                    continue;
                }
            },
            FrameType::FileReq => match parse::<FileReq>(&frame.payload) {
                Ok(req) => match crate::files::serve(&req, cfg) {
                    Ok(resp) => {
                        let body = serde_json::to_vec(&resp).expect("serialize file resp");
                        (FrameType::FileResp, body)
                    }
                    Err(message) => {
                        send_frame(&mut w, err_frame(message));
                        continue;
                    }
                },
                Err(f) => {
                    send_frame(&mut w, f);
                    continue;
                }
            },
            FrameType::SessionReq => match parse::<SessionReq>(&frame.payload) {
                Ok(req) => {
                    // Attach streams to EOF but keeps the connection alive
                    // for the next request; only a broken pipe closes it.
                    if serve_session(&mut r, &mut w, req) {
                        return;
                    }
                    continue;
                }
                Err(f) => {
                    send_frame(&mut w, f);
                    continue;
                }
            },
            other => {
                send_frame(
                    &mut w,
                    err_frame(format!("unexpected frame type {other:?}")),
                );
                continue;
            }
        };
        if 1 + body.len() > ahvm_proto::MAX_FRAME_SIZE as usize {
            // Pagination/clamps should prevent this; if it fires, the
            // handler needs a smaller page, not a dropped connection.
            return send_frame(
                &mut w,
                err_frame(format!(
                    "response too large ({} bytes): retry with a smaller limit",
                    body.len()
                )),
            );
        }
        if write_frame(
            &mut w,
            &Frame {
                msg_type,
                payload: body,
            },
        )
        .is_err()
        {
            return;
        }
    }
}

fn send_frame(w: &mut Conn, frame: Frame) {
    let _ = write_frame(w, &frame);
}

/// Serve one session request. Returns true when the connection should close
/// (client went away); false to keep serving requests on it.
fn serve_session(_r: &mut BufReader<Conn>, w: &mut Conn, req: SessionReq) -> bool {
    let mgr = crate::sessions::manager();
    let reply = |msg_type: FrameType, body: Vec<u8>| Frame {
        msg_type,
        payload: body,
    };
    match req {
        SessionReq::Create {
            argv,
            env,
            cwd,
            pty,
        } => {
            match mgr.create(argv, env, cwd, pty) {
                Ok(id) => {
                    let body = serde_json::to_vec(&SessionResp::Started { session_id: id })
                        .expect("serialize");
                    send_frame(w, reply(FrameType::SessionResp, body));
                }
                Err(message) => send_frame(w, err_frame(message)),
            }
            false
        }
        SessionReq::Input {
            session_id,
            data_b64,
        } => {
            use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
            let out = (|| {
                let data = B64.decode(&data_b64).map_err(|e| format!("base64: {e}"))?;
                let s = mgr
                    .get(&session_id)
                    .ok_or_else(|| format!("no such session: {session_id}"))?;
                s.write_stdin(&data)?;
                Ok::<u64, String>(data.len() as u64)
            })();
            match out {
                Ok(n) => {
                    let body = serde_json::to_vec(&SessionResp::InputAcked { bytes: n })
                        .expect("serialize");
                    send_frame(w, reply(FrameType::SessionResp, body));
                }
                Err(message) => send_frame(w, err_frame(message)),
            }
            false
        }
        SessionReq::Kill { session_id } => {
            match mgr.kill(&session_id) {
                Ok(()) => {
                    let body =
                        serde_json::to_vec(&SessionResp::Killed { session_id }).expect("serialize");
                    send_frame(w, reply(FrameType::SessionResp, body));
                }
                Err(message) => send_frame(w, err_frame(message)),
            }
            false
        }
        SessionReq::Resize {
            session_id,
            rows,
            cols,
        } => {
            match mgr.resize(&session_id, rows, cols) {
                Ok(()) => {
                    let body = serde_json::to_vec(&SessionResp::Resized { session_id })
                        .expect("serialize");
                    send_frame(w, reply(FrameType::SessionResp, body));
                }
                Err(message) => send_frame(w, err_frame(message)),
            }
            false
        }
        SessionReq::Delete { session_id } => {
            match mgr.delete(&session_id) {
                Ok(()) => {
                    let body = serde_json::to_vec(&SessionResp::Deleted { session_id })
                        .expect("serialize");
                    send_frame(w, reply(FrameType::SessionResp, body));
                }
                Err(message) => send_frame(w, err_frame(message)),
            }
            false
        }
        SessionReq::List => {
            let body = serde_json::to_vec(&SessionResp::Listed {
                sessions: mgr.list(),
            })
            .expect("serialize");
            send_frame(w, reply(FrameType::SessionResp, body));
            false
        }
        SessionReq::Attach {
            session_id,
            from_seq,
        } => {
            let Some(s) = mgr.get(&session_id) else {
                send_frame(w, err_frame(format!("no such session: {session_id}")));
                return false;
            };
            let mut sent = from_seq;
            loop {
                let (chunk, next, exit, truncated) = s.read_from(sent);
                let drained = next == s.total();
                // EOF only when process exited AND pumps drained AND ring drained
                let eof = exit.is_some() && drained && s.pumps_done();
                if chunk.is_empty() && !eof {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    continue;
                }
                // seq is actual chunk start, not requested offset (wrong after eviction)
                let actual_seq = next.saturating_sub(chunk.len() as u64);
                let data = SessionData {
                    session_id: session_id.clone(),
                    seq: actual_seq,
                    data_b64: b64(&chunk),
                    eof,
                    exit_code: if eof { exit } else { None },
                    truncated,
                };
                let body = serde_json::to_vec(&data).expect("serialize session data");
                if write_frame(
                    w,
                    &Frame {
                        msg_type: FrameType::SessionData,
                        payload: body,
                    },
                )
                .is_err()
                {
                    return true; // reader went away; nothing more to do
                }
                if eof {
                    return false;
                }
                sent = next;
            }
        }
    }
}

fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

pub(crate) fn b64(data: &[u8]) -> String {
    B64.encode(data)
}
