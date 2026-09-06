//! Agent wire protocol, v2.
//!
//! The Go implementation's framing (`[u32 BE len][u8 type][payload]`, 1 MiB
//! cap) is documented in `docs/PLAN-rust-rewrite.md` as migration reference
//! only: v2 is free to be better. What v2 keeps: tiny header, single-write
//! frames, stream-safe parsing. What v2 changes: explicit version nibble so
//! future revisions negotiate instead of breaking, and a legacy reader
//! ([`legacy`]) that parses Go-era frames for migration tooling.
//!
//! Design notes live with the dashboard-first API plan: the daemon API and
//! this transport both get redesigned, with shims at the boundary.

pub mod legacy;
pub mod v2;

pub use v2::{Frame, FrameType, MAX_FRAME_SIZE, PROTOCOL_VERSION};
