//! Neutral sandbox types shared by every backend.
//!
//! These port the *shape* of Go's `engine.SandboxSpec` / `SandboxInfo` /
//! `ExecResult` without the Go-only surface (volumes, net policy, TTY
//! handles): just what the lifecycle core needs to create, inspect, and
//! exec into a sandbox regardless of which VMM runs it.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::Capabilities;

/// Which VMM backend owns a sandbox. krucible is the default and only
/// initially-supported backend; Firecracker is a typed placeholder for the
/// dual-backend future (plan §7) — it can never do macOS/HVF, so krucible
/// stays the dev-machine path regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Krucible,
    Firecracker,
}

impl BackendKind {
    /// Static capability set for each backend kind. The real krucible
    /// backend reports the krucible row; the firecracker row is the
    /// currently-planned shape (no fork, untyped snapshots), not a promise.
    pub fn capabilities(self) -> Capabilities {
        match self {
            Self::Krucible => Capabilities {
                fork: true,
                typed_snapshots: true,
                live_migration: false,
                max_vcpus: 8,
            },
            Self::Firecracker => Capabilities {
                fork: false,
                typed_snapshots: false,
                live_migration: false,
                max_vcpus: 32,
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Krucible => "krucible",
            Self::Firecracker => "firecracker",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "krucible" => Some(Self::Krucible),
            "firecracker" => Some(Self::Firecracker),
            _ => None,
        }
    }
}

/// What to create. Backend-agnostic subset of Go's `engine.SandboxSpec`:
/// identity + sizing + images + env. Networking, volumes, and policy are
/// resolved by the daemon and travel out-of-band, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub name: String,
    pub cpus: u8,
    pub memory_mb: u32,
    pub backend: BackendKind,
    #[serde(default)]
    pub root_image: Option<String>,
    #[serde(default)]
    pub kernel_image: Option<String>,
    #[serde(default)]
    pub extra_env: HashMap<String, String>,
}

/// Runtime state of a sandbox. `Creating` covers the window between
/// `create` returning and the worker reporting boot; `Failed` is a
/// terminal backend-reported state (boot error, worker died).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Creating,
    Running,
    Stopped,
    Failed,
}

/// Thermal tier: `Hot` is a live worker, `Warm` was restored from a
/// snapshot bundle, `Cold` is stopped-to-disk (relaunch restores it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Thermal {
    Hot,
    Warm,
    Cold,
}

/// Runtime facts about one sandbox (cf. Go's `engine.SandboxInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub id: String,
    pub name: String,
    pub state: State,
    pub thermal: Thermal,
    pub ip: String,
}

/// Output of one exec. `truncated` is set when the backend capped the
/// captured output (see [`crate::MAX_EXEC_OUTPUT`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

/// One file-read chunk (cf. forge `FileResp::Read`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChunk {
    pub data: Vec<u8>,
    pub eof: bool,
}

/// One directory entry (cf. forge `DirEntry`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// A directory page (cf. forge `FileResp::List`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirListing {
    pub entries: Vec<DirEntry>,
    pub next_offset: Option<u64>,
}

/// Session facts (cf. forge `SessionInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub argv: Vec<String>,
    pub running: bool,
    pub started_at: i64,
}

/// Drained session output (cf. forge `SessionData`). `exit_code` is set
/// once the session process has exited. `next_seq` is the authoritative
/// resume cursor (frame `seq` + bytes, never client-side byte counting:
/// scrollback eviction makes byte counting wrong). `truncated` means output
/// between the requested `from_seq` and the returned data was evicted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionChunk {
    pub data: Vec<u8>,
    pub eof: bool,
    pub exit_code: Option<i32>,
    pub next_seq: u64,
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_json_roundtrip() {
        let spec = SandboxSpec {
            name: "web".to_string(),
            cpus: 2,
            memory_mb: 512,
            backend: BackendKind::Krucible,
            root_image: Some("img-root".to_string()),
            kernel_image: None,
            extra_env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
        };
        let raw = serde_json::to_string(&spec).unwrap();
        let back: SandboxSpec = serde_json::from_str(&raw).unwrap();
        assert_eq!(back, spec);

        // Minimal JSON still parses: images/env default out.
        let minimal: SandboxSpec = serde_json::from_str(
            r#"{"name":"m","cpus":1,"memory_mb":256,"backend":"firecracker"}"#,
        )
        .unwrap();
        assert_eq!(minimal.root_image, None);
        assert!(minimal.extra_env.is_empty());
    }

    #[test]
    fn info_state_thermal_roundtrip() {
        let info = SandboxInfo {
            id: "sb-1".to_string(),
            name: "web".to_string(),
            state: State::Running,
            thermal: Thermal::Hot,
            ip: "10.42.0.2".to_string(),
        };
        let raw = serde_json::to_string(&info).unwrap();
        assert_eq!(serde_json::from_str::<SandboxInfo>(&raw).unwrap(), info);
        assert_eq!(BackendKind::parse("krucible"), Some(BackendKind::Krucible));
        assert_eq!(BackendKind::parse("nope"), None);
    }

    #[test]
    fn capabilities_differ_per_backend() {
        let k = BackendKind::Krucible.capabilities();
        assert!(k.fork && k.typed_snapshots && !k.live_migration);
        assert_eq!(k.max_vcpus, 8);
        let f = BackendKind::Firecracker.capabilities();
        assert!(!f.fork);
    }
}
