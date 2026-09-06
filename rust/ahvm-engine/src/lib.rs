//! ahvm-engine: VMM lifecycle (create/snapshot/restore/thermal/recovery).
//!
//! Phase 3 target: links `libkrucible` as a native crate (no cgo), one
//! worker process per VM. Replaces `pkg/engine/krucible` + `cmd/vmm`.
