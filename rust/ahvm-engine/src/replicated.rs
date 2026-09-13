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
    resources_enforced: Option<bool>,
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
            "prepare" => 600,
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
    /// Host-only retirement handshake. False acknowledges intent, not cleanup.
    /// A missing proof or protocol failure must keep the tenant's reservation.
    pub fn retire(&self, id: &str, sandbox: &Path, logical_bytes: u64) -> Result<bool> {
        self.request_sized("retire", id, None, sandbox, Some(logical_bytes))?
            .reclamation_complete
            .ok_or_else(|| Error::Control("missing reclamation confirmation".into()))
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
        self.request("delete", id, None, sandbox).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    fn reply(tag: &str, value: serde_json::Value, call: impl FnOnce(&ReplicatedConfig)) {
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
            writeln!(stream, "{value}").unwrap();
        });
        call(&ReplicatedConfig {
            socket: path.clone(),
        });
        task.join().unwrap();
        std::fs::remove_file(path).unwrap();
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
                serde_json::json!({"ok":true,"volume_id":id,"reclamation_complete":false}),
                Some(false),
            ),
            (
                "complete",
                serde_json::json!({"ok":true,"volume_id":id,"reclamation_complete":true}),
                Some(true),
            ),
            (
                "missing-proof",
                serde_json::json!({"ok":true,"volume_id":id}),
                None,
            ),
            (
                "wrong-proof-id",
                serde_json::json!({"ok":true,"volume_id":"b".repeat(64),"reclamation_complete":true}),
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
