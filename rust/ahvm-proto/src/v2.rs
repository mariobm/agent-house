//! v2 framing: `[u32 BE len][u8 ver+type][payload]`.
//!
//! Layout mirrors v1 deliberately (length-prefixed, single segment) but the
//! type byte is split: high nibble = protocol version (currently 2), low
//! nibble = message class. A v3 peer reads the version first and can reject
//! or translate instead of misparsing. Length covers ver+type byte + payload,
//! identical accounting to v1, max 1 MiB.

use std::io::{self, Read, Write};

/// Current protocol version (high nibble of the type byte).
pub const PROTOCOL_VERSION: u8 = 2;
/// Maximum frame body size (ver+type byte + payload), 1 MiB like v1.
pub const MAX_FRAME_SIZE: u32 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    ExecReq = 0x0,
    Auth = 0x1,
    ConfigReq = 0x2,
    ConfigResp = 0x3,
    ExecResp = 0x4,
    Error = 0x5,
}

impl FrameType {
    fn from_nibble(n: u8) -> Option<Self> {
        match n {
            0x0 => Some(Self::ExecReq),
            0x1 => Some(Self::Auth),
            0x2 => Some(Self::ConfigReq),
            0x3 => Some(Self::ConfigResp),
            0x4 => Some(Self::ExecResp),
            0x5 => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub msg_type: FrameType,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Zero bytes available: clean end-of-stream.
    CleanEof,
    ZeroLength,
    TooLarge(u32),
    UnknownVersion(u8),
    UnknownType(u8),
    UnexpectedEof,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::CleanEof => write!(f, "end of stream"),
            Self::ZeroLength => write!(f, "invalid frame: length is 0"),
            Self::TooLarge(n) => write!(f, "frame too large: {n} > {MAX_FRAME_SIZE}"),
            Self::UnknownVersion(v) => write!(f, "unknown protocol version: {v}"),
            Self::UnknownType(t) => write!(f, "unknown frame type: {t:#x}"),
            Self::UnexpectedEof => write!(f, "stream ended mid-frame"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

fn read_exact_eof<R: Read>(r: &mut R, mut buf: &mut [u8]) -> Result<(), Error> {
    let mut got_any = false;
    while !buf.is_empty() {
        match r.read(buf) {
            Ok(0) => {
                return Err(if got_any {
                    Error::UnexpectedEof
                } else {
                    Error::CleanEof
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

/// Encode one frame; single buffer write like v1 (no interleaved partials).
pub fn write_frame<W: Write>(w: &mut W, frame: &Frame) -> Result<(), Error> {
    let body_len = 1 + frame.payload.len() as u32;
    if body_len > MAX_FRAME_SIZE {
        return Err(Error::TooLarge(body_len));
    }
    let mut buf = Vec::with_capacity(4 + body_len as usize);
    buf.extend_from_slice(&body_len.to_be_bytes());
    buf.push((PROTOCOL_VERSION << 4) | (frame.msg_type as u8));
    buf.extend_from_slice(&frame.payload);
    w.write_all(&buf)?;
    Ok(())
}

/// Decode one frame. Clean EOF (zero bytes available) surfaces as
/// [`Error::CleanEof`]; a stream ending mid-frame surfaces as
/// [`Error::UnexpectedEof`]. Callers matching "peer went away" treat both
/// as end-of-stream, mirroring v1 semantics.
pub fn read_frame<R: Read>(r: &mut R) -> Result<Frame, Error> {
    let mut len_buf = [0u8; 4];
    match read_exact_eof(r, &mut len_buf) {
        Ok(()) => {}
        Err(Error::CleanEof) => return Err(Error::CleanEof),
        Err(e) => return Err(e),
    }
    let body_len = u32::from_be_bytes(len_buf);
    if body_len == 0 {
        return Err(Error::ZeroLength);
    }
    if body_len > MAX_FRAME_SIZE {
        return Err(Error::TooLarge(body_len));
    }
    let mut body = vec![0u8; body_len as usize];
    // A clean EOF here is NOT clean: the 4-byte header already arrived, so
    // a frame has started and vanishing now means truncation. Map it to
    // UnexpectedEof; only a zero-byte first read is CleanEof. (Callers used
    // to treat a mid-frame disconnect as an ordinary hangup.)
    read_exact_eof(r, &mut body).map_err(|e| match e {
        Error::CleanEof => Error::UnexpectedEof,
        other => other,
    })?;
    let ver = body[0] >> 4;
    if ver != PROTOCOL_VERSION {
        return Err(Error::UnknownVersion(ver));
    }
    let msg_type = FrameType::from_nibble(body[0] & 0x0f).ok_or(Error::UnknownType(body[0]))?;
    Ok(Frame {
        msg_type,
        payload: body[1..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(t: FrameType, payload: &[u8]) {
        let frame = Frame {
            msg_type: t,
            payload: payload.to_vec(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).unwrap();
        assert_eq!(buf.len(), 4 + 1 + payload.len());
        let back = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn roundtrip_all_types_empty_and_full() {
        for t in [
            FrameType::ExecReq,
            FrameType::Auth,
            FrameType::ConfigReq,
            FrameType::ConfigResp,
            FrameType::ExecResp,
            FrameType::Error,
        ] {
            roundtrip(t, b"");
            roundtrip(t, b"hello-token-bytes");
            roundtrip(t, &vec![0xabu8; 64 * 1024]);
        }
    }

    #[test]
    fn rejects_zero_length() {
        let mut buf = 0u32.to_be_bytes().to_vec();
        assert!(matches!(
            read_frame(&mut Cursor::new(&buf)),
            Err(Error::ZeroLength)
        ));
        buf.clear();
    }

    #[test]
    fn rejects_oversize_and_bad_version() {
        let buf = (MAX_FRAME_SIZE + 1).to_be_bytes().to_vec();
        assert!(matches!(
            read_frame(&mut Cursor::new(&buf)),
            Err(Error::TooLarge(_))
        ));
        let mut buf = 1u32.to_be_bytes().to_vec();
        buf.push(0x10); // version 1, valid v1 AUTH shape
        assert!(matches!(
            read_frame(&mut Cursor::new(&buf)),
            Err(Error::UnknownVersion(1))
        ));
    }

    #[test]
    fn clean_eof_errors() {
        let mut empty: &[u8] = &[];
        assert!(matches!(read_frame(&mut empty), Err(Error::CleanEof)));
        // Header arrived ([len=1]) then disconnect: truncated frame, NOT
        // a clean EOF.
        let mut cut: &[u8] = &[0, 0, 0, 1];
        assert!(matches!(
            read_frame(&mut cut),
            Err(Error::UnexpectedEof)
        ));
        let mut partial: &[u8] = &[0, 0];
        assert!(matches!(
            read_frame(&mut partial),
            Err(Error::UnexpectedEof)
        ));
    }
}
