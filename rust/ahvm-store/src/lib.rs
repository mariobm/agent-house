//! ahvm-store: durable metadata (users, sandboxes, snapshots, volumes).
//!
//! Phase 1 target: rusqlite over the Go schema with a one-shot migration
//! path. Must open Go-written databases. See `docs/PLAN-rust-rewrite.md`.
