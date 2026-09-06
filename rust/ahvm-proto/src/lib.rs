//! Agent wire protocol, v2.
//!
//! Clean break from the Go implementation's framing: v2 keeps the good
//! properties (tiny header, single-write frames, stream-safe parsing) and
//! changes the rest — explicit version nibble so future revisions negotiate
//! instead of breaking. No legacy readers, no migration shims: pre-release
//! project, no production data to carry.
//!
//! Design notes live with the dashboard-first API plan: the daemon API and
//! this transport are both redesigned, similar in spirit to v1 where it was
//! good, better where it wasn't.

pub mod v2;

pub use v2::{read_frame, write_frame, Frame, FrameType, MAX_FRAME_SIZE, PROTOCOL_VERSION};
