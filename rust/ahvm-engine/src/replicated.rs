//! Host-only replicated-volume service seam. No object-store credentials cross it.
//!
//! The service owns attachment/journal lifetime independently of the engine.
//! The service validates sandbox/VM bindings and owns writer fencing/recovery.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageMode {
    #[default]
    Local,
    Replicated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicationStatus {
    pub local_sequence: u64,
    pub remote_sequence: u64,
    pub pending_bytes: u64,
    pub local_failed: bool,
    pub replication_failed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxStorage {
    pub mode: StorageMode,
    pub volume_id: Option<String>,
    /// None means unavailable or local storage, never "fully replicated".
    pub replication: Option<ReplicationStatus>,
}

/// Explicit supervisor proofs: safe logical retirement and physical GC differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetirementStatus {
    pub logical_released: bool,
    pub reclamation_complete: bool,
}

/// Trusted host service, configured by the embedding engine application.
/// It must serve a private Unix socket and never accept guest connections.
#[derive(Debug, Clone)]
pub struct ReplicatedConfig {
    pub socket: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    ok: bool,
    volume_id: String,
    #[serde(default)]
    device: Option<PathBuf>,
    #[serde(default)]
    status: Option<ReplicationStatus>,
    #[serde(default)]
    reclamation_complete: Option<bool>,
    #[serde(default)]
    logical_released: Option<bool>,
    #[serde(default)]
    resources_enforced: Option<bool>,
    #[serde(default)]
    project_images_available: Option<bool>,
    #[serde(default)]
    project_image_capacity: Option<crate::ProjectImageCapacity>,
    #[serde(default)]
    project_image: Option<crate::ProjectImageInfo>,
    #[serde(default)]
    project_image_phase: Option<String>,
    #[serde(default)]
    project_image_quiescent: Option<bool>,
    #[serde(default)]
    project_image_error_code: Option<String>,
}

pub(crate) fn validate_volume(id: &str) -> Result<()> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::InvalidState(
            "invalid replicated volume identity".into(),
        ));
    }
    Ok(())
}

pub(crate) fn new_volume() -> Result<String> {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

impl ReplicatedConfig {
    fn project_request(
        &self,
        operation: &str,
        volume: &str,
        sandbox: &Path,
        image: Option<crate::ProjectImageRequest>,
    ) -> Result<Reply> {
        validate_volume(volume)?;
        let mut payload = serde_json::json!({"version":1,"operation":operation,"volume_id":volume,"sandbox_dir":sandbox,"image":null,"logical_bytes":null});
        if let Some(image) = image {
            if !crate::project_uuid(&image.image_id) {
                return Err(Error::InvalidState("invalid private image identity".into()));
            }
            if let Some(metadata) = &image.metadata {
                metadata.validate()?;
            }
            payload["project_image"] = serde_json::to_value(image)?;
        }
        let bytes = serde_json::to_vec(&payload)?;
        if bytes.len() > 4095 {
            return Err(Error::InvalidState(
                "private image request too large".into(),
            ));
        }
        let mut conn = UnixStream::connect(&self.socket)?;
        conn.set_read_timeout(Some(Duration::from_secs(
            if operation == "project-image-capture" {
                3600
            } else if operation == "project-image-retire" {
                300
            } else {
                3
            },
        )))?;
        conn.set_write_timeout(Some(Duration::from_secs(3)))?;
        conn.write_all(&bytes)?;
        conn.write_all(b"\n")?;
        let mut line = String::new();
        BufReader::new(conn.take(4097)).read_line(&mut line)?;
        if line.len() > 4096 || !line.ends_with('\n') {
            return Err(Error::Control("invalid project image reply".into()));
        }
        let reply: Reply = serde_json::from_str(&line)?;
        if operation == "project-image-capabilities" && !reply.ok {
            return Ok(reply);
        }
        if !reply.ok || reply.volume_id != volume {
            return Err(Error::Control(
                "project image service rejected operation".into(),
            ));
        }
        Ok(reply)
    }
    pub(crate) fn project_capacity(
        &self,
        sandbox: &Path,
    ) -> Result<Option<crate::ProjectImageCapacity>> {
        let reply =
            self.project_request("project-image-capabilities", &"0".repeat(64), sandbox, None)?;
        if reply.project_images_available != Some(true) {
            return Ok(None);
        }
        reply
            .project_image_capacity
            .map(Some)
            .ok_or_else(|| Error::Control("project image capacity proof unavailable".into()))
    }
    pub(crate) fn capture_project_image(
        &self,
        volume: &str,
        sandbox: &Path,
        metadata: &crate::ProjectImageMetadata,
        source_bytes: u64,
    ) -> Result<crate::ProjectImageInfo> {
        metadata.validate()?;
        if source_bytes == 0
            || !source_bytes.is_multiple_of(65536)
            || source_bytes
                .checked_add(crate::PROJECT_IMAGE_TRAILER_BYTES)
                .is_none_or(|n| n > crate::PROJECT_IMAGE_MAX_BYTES)
        {
            return Err(Error::InvalidState("invalid project image geometry".into()));
        }
        let request = crate::ProjectImageRequest {
            image_id: metadata.build_id.clone(),
            metadata: Some(metadata.clone()),
            source_logical_bytes: Some(source_bytes),
            expected_digest: None,
        };
        let info = self
            .project_request("project-image-capture", volume, sandbox, Some(request))?
            .project_image
            .ok_or_else(|| Error::Control("project image completion proof unavailable".into()))?;
        validate_project_info(&info, volume, &metadata.build_id)?;
        if &info.metadata != metadata
            || info.logical_bytes != source_bytes + crate::PROJECT_IMAGE_TRAILER_BYTES
        {
            return Err(Error::Control(
                "project image capture provenance changed".into(),
            ));
        }
        Ok(info)
    }
    pub(crate) fn project_image(
        &self,
        volume: &str,
        sandbox: &Path,
        image: &str,
    ) -> Result<Option<crate::ProjectImageInfo>> {
        let status = self.project_image_status(volume, sandbox, image)?;
        Ok((status.phase == "ready" && status.quiescent)
            .then_some(status.image)
            .flatten())
    }
    pub(crate) fn project_image_status(
        &self,
        volume: &str,
        sandbox: &Path,
        image: &str,
    ) -> Result<crate::ProjectImageStatus> {
        let request = crate::ProjectImageRequest {
            image_id: image.into(),
            metadata: None,
            source_logical_bytes: None,
            expected_digest: None,
        };
        let reply = self.project_request("project-image-status", volume, sandbox, Some(request))?;
        let info = reply.project_image;
        if let Some(info) = &info {
            validate_project_info(info, volume, image)?;
        }
        let phase = reply
            .project_image_phase
            .ok_or_else(|| Error::Control("private image status proof unavailable".into()))?;
        let quiescent = reply
            .project_image_quiescent
            .ok_or_else(|| Error::Control("private image quiescence proof unavailable".into()))?;
        if !["absent", "capturing", "ready", "failed", "retired"].contains(&phase.as_str())
            || (phase == "ready" && (!quiescent || info.is_none()))
            || (phase == "failed" && !quiescent)
            || (phase == "absent" && (quiescent || info.is_some()))
            || (phase != "ready" && info.is_some())
            || reply.project_image_error_code.as_ref().is_some_and(|s| {
                s.is_empty()
                    || s.len() > 80
                    || !s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            })
        {
            return Err(Error::Control("invalid private image state proof".into()));
        }
        Ok(crate::ProjectImageStatus {
            phase,
            quiescent,
            error_code: reply.project_image_error_code,
            image: info,
        })
    }
    pub(crate) fn retire_project_image(
        &self,
        volume: &str,
        sandbox: &Path,
        image: &str,
        digest: Option<&str>,
    ) -> Result<crate::ProjectImageRetirement> {
        if digest.is_some_and(|d| !crate::project_digest(d)) {
            return Err(Error::InvalidState("invalid private image digest".into()));
        }
        let request = crate::ProjectImageRequest {
            image_id: image.into(),
            metadata: None,
            source_logical_bytes: None,
            expected_digest: digest.map(str::to_string),
        };
        let reply = self.project_request("project-image-retire", volume, sandbox, Some(request))?;
        let logical = reply
            .logical_released
            .ok_or_else(|| Error::Control("private image retirement proof unavailable".into()))?;
        let reclaimed = reply
            .reclamation_complete
            .ok_or_else(|| Error::Control("private image reclamation proof unavailable".into()))?;
        if reclaimed && !logical {
            return Err(Error::Control(
                "private image reclamation contradicted retirement".into(),
            ));
        }
        Ok(crate::ProjectImageRetirement {
            logical_released: logical,
            reclamation_complete: reclaimed,
        })
    }
    fn request(
        &self,
        operation: &str,
        id: &str,
        image: Option<&Path>,
        sandbox: &Path,
    ) -> Result<Reply> {
        self.request_sized(operation, id, image, sandbox, None)
    }
    fn request_sized(
        &self,
        operation: &str,
        id: &str,
        image: Option<&Path>,
        sandbox: &Path,
        logical_bytes: Option<u64>,
    ) -> Result<Reply> {
        validate_volume(id)?;
        let mut conn = UnixStream::connect(&self.socket)?;
        let seconds = match operation {
            // First import uploads a full developer image, not just dirty writes.
            // Lifecycle receipts remain pollable while this bounded request runs.
            "prepare" => 3600,
            "status" | "resources" => 3,
            "retire" => 30,
            _ => 300,
        };
        conn.set_read_timeout(Some(Duration::from_secs(seconds)))?;
        conn.set_write_timeout(Some(Duration::from_secs(3)))?;
        serde_json::to_writer(
            &mut conn,
            &serde_json::json!({
                "version": 1, "operation": operation, "volume_id": id, "image": image, "sandbox_dir": sandbox, "logical_bytes":logical_bytes,
            }),
        )?;
        conn.write_all(b"\n")?;
        let mut line = String::new();
        BufReader::new(conn.take(4097)).read_line(&mut line)?;
        if line.len() > 4096 || !line.ends_with('\n') {
            return Err(Error::Control("invalid volume service reply".into()));
        }
        let reply: Reply = serde_json::from_str(&line)?;
        if !reply.ok || reply.volume_id != id {
            return Err(Error::Control(
                "volume service rejected operation or identity".into(),
            ));
        }
        if let Some(s) = &reply.status {
            if s.remote_sequence > s.local_sequence {
                return Err(Error::Control("invalid replication watermark".into()));
            }
        }
        Ok(reply)
    }
    pub(crate) fn verify_resources(&self, id: &str, sandbox: &Path) -> Result<()> {
        if self
            .request("resources", id, None, sandbox)?
            .resources_enforced
            != Some(true)
        {
            return Err(Error::InvalidState(
                "volume service requires bounded supervisor and worker cgroups".into(),
            ));
        }
        Ok(())
    }
    /// Missing or contradictory proofs must keep tenant capacity reserved.
    pub fn retire(&self, id: &str, sandbox: &Path, logical_bytes: u64) -> Result<RetirementStatus> {
        let reply = self.request_sized("retire", id, None, sandbox, Some(logical_bytes))?;
        let logical_released = reply
            .logical_released
            .ok_or_else(|| Error::Control("missing logical retirement confirmation".into()))?;
        let reclamation_complete = reply
            .reclamation_complete
            .ok_or_else(|| Error::Control("missing reclamation confirmation".into()))?;
        if reclamation_complete && !logical_released {
            return Err(Error::Control(
                "reclamation without logical retirement".into(),
            ));
        }
        Ok(RetirementStatus {
            logical_released,
            reclamation_complete,
        })
    }
    pub(crate) fn prepare(
        &self,
        id: &str,
        image: &Path,
        sandbox: &Path,
        logical_bytes: Option<u64>,
    ) -> Result<()> {
        self.request_sized("prepare", id, Some(image), sandbox, logical_bytes)
            .map(|_| ())
    }
    pub(crate) fn attach(&self, id: &str, sandbox: &Path) -> Result<PathBuf> {
        self.device("attach", id, sandbox)
    }
    pub(crate) fn inspect(&self, id: &str, sandbox: &Path) -> Result<PathBuf> {
        self.device("inspect", id, sandbox)
    }
    fn device(&self, operation: &str, id: &str, sandbox: &Path) -> Result<PathBuf> {
        let reply = self.request(operation, id, None, sandbox)?;
        if reply.status.as_ref().is_none_or(|s| s.local_failed) {
            return Err(Error::Control("replicated device unavailable".into()));
        }
        let device = reply
            .device
            .ok_or_else(|| Error::Control("missing volume device".into()))?;
        let name = device.to_str().unwrap_or("");
        let suffix = name.strip_prefix("/dev/nbd").unwrap_or("");
        if suffix.is_empty()
            || !suffix.bytes().all(|b| b.is_ascii_digit())
            || !std::fs::symlink_metadata(&device)?
                .file_type()
                .is_block_device()
        {
            return Err(Error::Control("invalid replicated block device".into()));
        }
        Ok(device)
    }
    pub(crate) fn bind(&self, id: &str, sandbox: &Path) -> Result<()> {
        self.request("bind", id, None, sandbox).map(|_| ())
    }
    pub(crate) fn status(&self, id: &str, sandbox: &Path) -> Result<ReplicationStatus> {
        self.request("status", id, None, sandbox)?
            .status
            .ok_or_else(|| Error::Control("missing volume status".into()))
    }
    pub(crate) fn sync(&self, id: &str, sandbox: &Path) -> Result<ReplicationStatus> {
        let status = self
            .request("sync", id, None, sandbox)?
            .status
            .ok_or_else(|| Error::Control("missing volume status".into()))?;
        if status.local_failed
            || status.replication_failed
            || status.pending_bytes != 0
            || status.local_sequence != status.remote_sequence
        {
            return Err(Error::Control(
                "volume did not drain to remote storage".into(),
            ));
        }
        Ok(status)
    }
    pub(crate) fn detach(&self, id: &str, sandbox: &Path) -> Result<()> {
        self.request("detach", id, None, sandbox).map(|_| ())
    }
    pub(crate) fn delete(&self, id: &str, sandbox: &Path) -> Result<()> {
        if self.request("delete", id, None, sandbox)?.logical_released != Some(true) {
            return Err(Error::Control("missing safe deletion confirmation".into()));
        }
        Ok(())
    }
}

fn validate_project_info(info: &crate::ProjectImageInfo, volume: &str, image: &str) -> Result<()> {
    info.metadata.validate()?;
    if info.image_id != image
        || info.metadata.build_id != image
        || info.source_volume_id != volume
        || !crate::project_digest(&info.digest)
        || !info.image_path.is_absolute()
        || info.image_path.file_name().and_then(|s| s.to_str())
            != Some(format!("{}.ext4", info.digest).as_str())
        || info.logical_bytes <= crate::PROJECT_IMAGE_TRAILER_BYTES
        || info.logical_bytes > crate::PROJECT_IMAGE_MAX_BYTES
        || !info.logical_bytes.is_multiple_of(65536)
    {
        return Err(Error::Control(
            "invalid private image completion proof".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    fn reply(tag: &str, value: serde_json::Value, call: impl FnOnce(&ReplicatedConfig)) {
        reply_checked(tag, value, |_| {}, call)
    }
    fn reply_checked(
        tag: &str,
        value: serde_json::Value,
        inspect: impl FnOnce(serde_json::Value) + Send + 'static,
        call: impl FnOnce(&ReplicatedConfig),
    ) {
        let path = PathBuf::from(format!(
            "/tmp/ahvm-volume-reply-{}-{tag}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            inspect(serde_json::from_str(&line).unwrap());
            writeln!(stream, "{value}").unwrap();
        });
        call(&ReplicatedConfig {
            socket: path.clone(),
        });
        task.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }
    fn project_metadata() -> crate::ProjectImageMetadata {
        crate::ProjectImageMetadata {
            schema: 1,
            tenant_id: "33333333-3333-4333-8333-333333333333".into(),
            project_id: "11111111-1111-4111-8111-111111111111".into(),
            build_id: "22222222-2222-4222-8222-222222222222".into(),
            source_commit: "c".repeat(40),
            base_image_digest: "a".repeat(64),
            base_content_digest: "e".repeat(64),
            revision_id: "b".repeat(64),
            install_hook_digest: None,
            start_hook_digest: None,
            canonical_workdir: "/workspace/project".into(),
        }
    }
    #[test]
    fn project_capability_uses_exact_engine_root_and_requires_capacity_proof() {
        reply_checked(
            "project-caps",
            serde_json::json!({"ok":true,"volume_id":"0".repeat(64),"project_images_available":true,"project_image_capacity":{"physical_available_bytes":1024,"physical_reserved_bytes":64}}),
            |q| {
                assert_eq!(q["sandbox_dir"], "/tmp/engine-root");
                assert_eq!(q["operation"], "project-image-capabilities");
                assert!(q.get("project_image").is_none());
            },
            |c| {
                assert_eq!(
                    c.project_capacity(Path::new("/tmp/engine-root")).unwrap(),
                    Some(crate::ProjectImageCapacity {
                        physical_available_bytes: 1024,
                        physical_reserved_bytes: 64
                    })
                )
            },
        );
        reply(
            "project-old",
            serde_json::json!({"ok":false,"volume_id":"0".repeat(64)}),
            |c| {
                assert!(c
                    .project_capacity(Path::new("/tmp/engine-root"))
                    .unwrap()
                    .is_none())
            },
        );
        reply(
            "project-no-capacity",
            serde_json::json!({"ok":true,"volume_id":"0".repeat(64),"project_images_available":true}),
            |c| assert!(c.project_capacity(Path::new("/tmp/engine-root")).is_err()),
        );
    }
    #[test]
    fn private_image_proofs_bind_source_generation_and_reject_retirement_contradiction() {
        let metadata = project_metadata();
        let volume = "e".repeat(64);
        let image = metadata.build_id.clone();
        let info = crate::ProjectImageInfo {
            image_id: image.clone(),
            digest: "d".repeat(64),
            image_path: PathBuf::from(format!("/private/{}.ext4", "d".repeat(64))),
            logical_bytes: 131072,
            source_volume_id: volume.clone(),
            metadata: metadata.clone(),
        };
        reply(
            "project-capture",
            serde_json::json!({"ok":true,"volume_id":volume,"project_image":info}),
            |c| {
                assert_eq!(
                    c.capture_project_image(&volume, Path::new("/tmp/source"), &metadata, 65536)
                        .unwrap(),
                    info
                )
            },
        );
        let mut wrong = info.clone();
        wrong.metadata.project_id = "44444444-4444-4444-8444-444444444444".into();
        reply(
            "project-changed-meta",
            serde_json::json!({"ok":true,"volume_id":volume,"project_image":wrong}),
            |c| {
                assert!(c
                    .capture_project_image(&volume, Path::new("/tmp/source"), &metadata, 65536)
                    .is_err())
            },
        );
        let mut wrong = info.clone();
        wrong.source_volume_id = "f".repeat(64);
        reply(
            "project-changed-source",
            serde_json::json!({"ok":true,"volume_id":volume,"project_image":wrong}),
            |c| {
                assert!(c
                    .project_image(&volume, Path::new("/tmp/deleted-source"), &image)
                    .is_err())
            },
        );
        reply(
            "project-retired",
            serde_json::json!({"ok":true,"volume_id":volume,"logical_released":false,"reclamation_complete":true}),
            |c| {
                assert!(c
                    .retire_project_image(
                        &volume,
                        Path::new("/tmp/deleted-source"),
                        &image,
                        Some(&"d".repeat(64))
                    )
                    .is_err())
            },
        );
    }
    #[test]
    fn project_status_requires_ready_quiescence_and_distinguishes_absence() {
        let volume = "e".repeat(64);
        let image = project_metadata().build_id;
        for (phase, quiet, code) in [
            ("absent", false, None),
            ("failed", true, Some("project_image_capture_failed")),
            ("retired", true, None),
        ] {
            reply(
                &format!("proof-{phase}"),
                serde_json::json!({"ok":true,"volume_id":volume,"project_image_phase":phase,"project_image_quiescent":quiet,"project_image_error_code":code}),
                |c| {
                    let status = c
                        .project_image_status(&volume, Path::new("/tmp/deleted-source"), &image)
                        .unwrap();
                    assert_eq!(status.phase, phase);
                    assert_eq!(status.quiescent, quiet);
                    assert!(status.image.is_none());
                },
            );
        }
        reply(
            "absence-not-quiescence",
            serde_json::json!({"ok":true,"volume_id":volume,"project_image_phase":"absent","project_image_quiescent":true}),
            |c| {
                assert!(c
                    .project_image_status(&volume, Path::new("/tmp/source"), &image)
                    .is_err());
            },
        );
    }
    #[test]
    fn resource_enforcement_requires_explicit_confirmation() {
        let id = "a".repeat(64);
        for (tag, proof, accepted) in [
            ("missing", serde_json::Value::Null, false),
            ("disabled", serde_json::json!(false), false),
            ("enabled", serde_json::json!(true), true),
        ] {
            reply(
                tag,
                serde_json::json!({"ok":true,"volume_id":id,"resources_enforced":proof}),
                |cfg| {
                    assert_eq!(
                        cfg.verify_resources(&id, Path::new("/sandbox")).is_ok(),
                        accepted
                    );
                },
            );
        }
    }
    #[test]
    fn retirement_acknowledgement_is_not_reclamation() {
        let id = "a".repeat(64);
        for (tag, value, expected) in [
            (
                "pending",
                serde_json::json!({"ok":true,"volume_id":id,"logical_released":true,"reclamation_complete":false}),
                Some(RetirementStatus {
                    logical_released: true,
                    reclamation_complete: false,
                }),
            ),
            (
                "complete",
                serde_json::json!({"ok":true,"volume_id":id,"logical_released":true,"reclamation_complete":true}),
                Some(RetirementStatus {
                    logical_released: true,
                    reclamation_complete: true,
                }),
            ),
            (
                "not-detached",
                serde_json::json!({"ok":true,"volume_id":id,"logical_released":false,"reclamation_complete":false}),
                Some(RetirementStatus {
                    logical_released: false,
                    reclamation_complete: false,
                }),
            ),
            (
                "missing-logical-proof",
                serde_json::json!({"ok":true,"volume_id":id,"reclamation_complete":true}),
                None,
            ),
            (
                "contradictory-proof",
                serde_json::json!({"ok":true,"volume_id":id,"logical_released":false,"reclamation_complete":true}),
                None,
            ),
            (
                "missing-proof",
                serde_json::json!({"ok":true,"volume_id":id}),
                None,
            ),
            (
                "wrong-proof-id",
                serde_json::json!({"ok":true,"volume_id":"b".repeat(64),"logical_released":true,"reclamation_complete":true}),
                None,
            ),
        ] {
            reply(tag, value, |c| {
                assert_eq!(c.retire(&id, Path::new("/tmp/test"), 65536).ok(), expected)
            });
        }
    }
    #[test]
    fn rejects_mismatched_identity_and_impossible_watermark() {
        let id = "a".repeat(64);
        reply(
            "id",
            serde_json::json!({"ok":true,"volume_id":"b".repeat(64)}),
            |c| assert!(c.delete(&id, Path::new("/tmp/test")).is_err()),
        );
        reply(
            "watermark",
            serde_json::json!({"ok":true,"volume_id":id,"status":{"local_sequence":1,"remote_sequence":2,"pending_bytes":0,"local_failed":false,"replication_failed":false}}),
            |c| assert!(c.status(&id, Path::new("/tmp/test")).is_err()),
        );
    }
    #[test]
    fn sync_rejects_backlog_and_attach_rejects_a_regular_file() {
        let id = "a".repeat(64);
        let status = serde_json::json!({"local_sequence":2,"remote_sequence":1,"pending_bytes":65536,"local_failed":false,"replication_failed":false});
        reply(
            "backlog",
            serde_json::json!({"ok":true,"volume_id":id,"status":status}),
            |c| assert!(c.sync(&id, Path::new("/tmp/test")).is_err()),
        );
        reply(
            "file",
            serde_json::json!({"ok":true,"volume_id":id,"device":"/etc/passwd","status":status}),
            |c| assert!(c.attach(&id, Path::new("/tmp/test")).is_err()),
        );
    }
}
