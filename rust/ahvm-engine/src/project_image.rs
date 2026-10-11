//! Shared host-only project image RPC types. The disk/catalog format stays the
//! ordinary content-addressed base format; provenance occupies one aligned
//! block after the exported ext4 filesystem's declared extent.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const PROJECT_IMAGE_TRAILER_BYTES: u64 = 65536;
pub const PROJECT_IMAGE_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub const PROJECT_IMAGE_HEADROOM_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectImageMetadata {
    pub schema: u32,
    pub tenant_id: String,
    pub project_id: String,
    pub build_id: String,
    pub source_commit: String,
    pub base_image_digest: String,
    pub revision_id: String,
    pub install_hook_digest: Option<String>,
    pub start_hook_digest: Option<String>,
    pub canonical_workdir: String,
}

pub fn project_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

pub fn project_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl ProjectImageMetadata {
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1
            || !project_uuid(&self.tenant_id)
            || !project_uuid(&self.project_id)
            || !project_uuid(&self.build_id)
            || ![40, 64].contains(&self.source_commit.len())
            || !self
                .source_commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || !project_digest(&self.base_image_digest)
            || !project_digest(&self.revision_id)
            || self
                .install_hook_digest
                .as_deref()
                .is_some_and(|s| !project_digest(s))
            || self
                .start_hook_digest
                .as_deref()
                .is_some_and(|s| !project_digest(s))
            || self.canonical_workdir != "/workspace/project"
        {
            return Err(Error::InvalidState(
                "invalid project image provenance".into(),
            ));
        }
        Ok(())
    }

    /// Struct field order and mandatory nulls are the frozen trailer encoding.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        if bytes.len() > 4096 {
            return Err(Error::InvalidState(
                "project image provenance too large".into(),
            ));
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectImageRequest {
    pub image_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<ProjectImageMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_logical_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectImageInfo {
    pub image_id: String,
    pub digest: String,
    pub image_path: PathBuf,
    pub logical_bytes: u64,
    pub source_volume_id: String,
    pub metadata: ProjectImageMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectImageRetirement {
    pub logical_released: bool,
    pub reclamation_complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectImageCapacity {
    pub physical_available_bytes: u64,
    pub physical_reserved_bytes: u64,
}

/// A create request pins the exact generation and its target project, even
/// when a node's daemon owner represents several Cloud tenants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectImageSelection {
    pub image_id: String,
    pub digest: String,
    pub tenant_id: String,
    pub project_id: String,
}
