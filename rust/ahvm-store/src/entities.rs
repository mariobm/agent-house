//! Entity types. Plain data; validation lives in the daemon.

use rusqlite::{Result, Row};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    pub api_key_hash: String,
    pub max_sandboxes: i64,
    pub max_cpus: i64,
    pub max_memory_mb: i64,
    pub max_volumes_mb: i64,
    pub max_snapshots: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl User {
    pub(crate) fn from_row(r: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            name: r.get(1)?,
            api_key_hash: r.get(2)?,
            max_sandboxes: r.get(3)?,
            max_cpus: r.get(4)?,
            max_memory_mb: r.get(5)?,
            max_volumes_mb: r.get(6)?,
            max_snapshots: r.get(7)?,
            created_at: r.get(8)?,
            updated_at: r.get(9)?,
        })
    }
}

/// Which VMM backend owns a sandbox. krucible is the default and only
/// initially-supported backend; Firecracker rows are planned (see plan §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Krucible,
    Firecracker,
}

impl Backend {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sandbox {
    pub id: String,
    pub owner_user_id: String,
    pub name: String,
    pub backend: Backend,
    pub state: String,
    pub thermal: String,
    pub cpus: i64,
    pub memory_mb: i64,
    pub ip: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Sandbox {
    pub(crate) fn from_row(r: &Row<'_>) -> Result<Self> {
        let backend: String = r.get(3)?;
        Ok(Self {
            id: r.get(0)?,
            owner_user_id: r.get(1)?,
            name: r.get(2)?,
            backend: Backend::parse(&backend).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    format!("unknown backend {backend:?}").into(),
                )
            })?,
            state: r.get(4)?,
            thermal: r.get(5)?,
            cpus: r.get(6)?,
            memory_mb: r.get(7)?,
            ip: r.get(8)?,
            created_at: r.get(9)?,
            updated_at: r.get(10)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub owner_user_id: String,
    pub sandbox_id: Option<String>,
    pub name: String,
    pub kind: String,
    pub state: String,
    pub local_bytes: i64,
    pub remote_state: String,
    pub remote_manifest_key: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

impl Snapshot {
    pub(crate) fn from_row(r: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            owner_user_id: r.get(1)?,
            sandbox_id: r.get(2)?,
            name: r.get(3)?,
            kind: r.get(4)?,
            state: r.get(5)?,
            local_bytes: r.get(6)?,
            remote_state: r.get(7)?,
            remote_manifest_key: r.get(8)?,
            created_at: r.get(9)?,
            expires_at: r.get(10)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Volume {
    pub id: String,
    pub owner_user_id: String,
    pub name: String,
    pub size_mb: i64,
    pub attached_to: Option<String>,
    pub created_at: i64,
}

impl Volume {
    pub(crate) fn from_row(r: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            owner_user_id: r.get(1)?,
            name: r.get(2)?,
            size_mb: r.get(3)?,
            attached_to: r.get(4)?,
            created_at: r.get(5)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub id: String,
    pub name: String,
    pub source: String,
    pub size_mb: i64,
    pub created_at: i64,
}

impl Image {
    pub(crate) fn from_row(r: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            name: r.get(1)?,
            source: r.get(2)?,
            size_mb: r.get(3)?,
            created_at: r.get(4)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub state: String,
    pub progress: f64,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Task {
    pub(crate) fn from_row(r: &Row<'_>) -> Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            kind: r.get(1)?,
            state: r.get(2)?,
            progress: r.get(3)?,
            created_at: r.get(4)?,
            updated_at: r.get(5)?,
        })
    }
}
