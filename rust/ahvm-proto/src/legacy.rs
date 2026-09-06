//! Migration reader for Go-era (v1) frames: `[u32 BE len][u8 type][payload]`.
//!
//! Exists solely for migration tooling (read old snapshots/logs, translate
//! to v2). New code MUST NOT emit v1 frames. See [`crate::v2`] for the
//! current design.

use crate::v2::{Frame, FrameType, MAX_FRAME_SIZE};
use std::io::{self, Read};

/// Legacy v1 type codes (Go `pkg/agent/proto`).
pub mod v1codes {
    pub const EXEC_REQ: u8 = 0x10;
    pub const AUTH: u8 = 0x11;
    pub const CONFIG_REQ: u8 = 0x12;
    pub const CONFIG_RESP: u8 = 0x13;
}

fn map_type(t: u8) -> Option<FrameType> {
    match t {
        v1codes::EXEC_REQ => Some(FrameType::ExecReq),
        v1codes::AUTH => Some(FrameType::Auth),
        v1codes::CONFIG_REQ => Some(FrameType::ConfigReq),
        v1codes::CONFIG_RESP => Some(FrameType::ConfigResp),
        _ => None,
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    ZeroLength,
    TooLarge(u32),
    UnknownType(u8),
    UnexpectedEof,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::ZeroLength => write!(f, "invalid frame: length is 0"),
            Self::TooLarge(n) => write!(f, "frame too large: {n}"),
            Self::UnknownType(t) => write!(f, "unknown v1 frame type: {t:#x}"),
            Self::UnexpectedEof => write!(f, "stream ended mid-frame"),
        }
    }
}

impl std::error::Error for Error {}

fn read_full_eof<R: Read>(r: &mut R, mut buf: &mut [u8]) -> Result<(), Error> {
    let mut got_any = false;
    while !buf.is_empty() {
        match r.read(buf) {
            Ok(0) => {
                return Err(if got_any {
                    Error::UnexpectedEof
                } else {
                    Error::Io(io::Error::from(io::ErrorKind::UnexpectedEof))
                });
            }
            Ok(n) => {
                got_any = true;
                buf = &mut buf[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(())
}

/// Read one Go-era frame and translate it to a v2 [`Frame`].
pub fn read_legacy_frame<R: Read>(r: &mut R) -> Result<Frame, Error> {
    let mut len_buf = [0u8; 4];
    read_full_eof(r, &mut len_buf).map_err(|e| match e {
        Error::Io(inner) => Error::Io(inner),
        other => other,
    })?;
    let body_len = u32::from_be_bytes(len_buf);
    if body_len == 0 {
        return Err(Error::ZeroLength);
    }
    if body_len > MAX_FRAME_SIZE {
        return Err(Error::TooLarge(body_len));
    }
    let mut body = vec![0u8; body_len as usize];
    read_full_eof(r, &mut body)?;
    let msg_type = map_type(body[0]).ok_or(Error::UnknownType(body[0]))?;
    Ok(Frame {
        msg_type,
        payload: body[1..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Golden vector: exact bytes the Go implementation emits for
    /// `WriteFrame(AUTH, "abc")` — `[len=4 BE][0x11][abc]`.
    #[test]
    fn reads_go_golden_auth_frame() {
        let golden = [0u8, 0, 0, 4, 0x11, b'a', b'b', b'c'];
        let frame = read_legacy_frame(&mut Cursor::new(&golden)).unwrap();
        assert_eq!(frame.msg_type, FrameType::Auth);
        assert_eq!(frame.payload, b"abc");
    }

    #[test]
    fn reads_go_golden_empty_config_req() {
        let golden = [0u8, 0, 0, 1, 0x12];
        let frame = read_legacy_frame(&mut Cursor::new(&golden)).unwrap();
        assert_eq!(frame.msg_type, FrameType::ConfigReq);
        assert!(frame.payload.is_empty());
    }
}
