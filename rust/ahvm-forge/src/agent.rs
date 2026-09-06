//! Per-connection dispatch: optional AUTH gate, then one request loop
//! serving EXEC_REQ and FILE_REQ frames until EOF or protocol violation.

use crate::config::Config;
use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use std::io::BufReader;
use std::net::TcpStream;

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
    Read { path: String, offset: u64, limit: u64 },
    Write { path: String, data_b64: String },
    List { path: String, offset: u64, limit: u64 },
}

#[derive(Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FileResp {
    Read { data_b64: String, eof: bool },
    Write { bytes: u64 },
    List { entries: Vec<DirEntry>, next_offset: Option<u64> },
}

#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

fn err_frame(message: String) -> Frame {
    let body = serde_json::json!({ "message": message }).to_string().into_bytes();
    Frame {
        msg_type: FrameType::Error,
        payload: body,
    }
}

fn parse<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, Frame> {
    serde_json::from_slice(payload).map_err(|e| err_frame(format!("bad request: {e}")))
}

pub fn handle(stream: TcpStream, cfg: &Config) {
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
            other => {
                send_frame(&mut w, err_frame(format!("unexpected frame type {other:?}")));
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
        if write_frame(&mut w, &Frame { msg_type, payload: body }).is_err() {
            return;
        }
    }
}

fn send_frame(w: &mut TcpStream, frame: Frame) {
    let _ = write_frame(w, &frame);
}

fn constant_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub(crate) fn b64(data: &[u8]) -> String {
    B64.encode(data)
}
