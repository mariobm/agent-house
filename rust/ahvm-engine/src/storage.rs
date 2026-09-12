//! Optional host quota broker. The daemon never receives quota-management privileges.
use crate::{Error, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub socket: PathBuf,
}
impl StorageConfig {
    pub(crate) fn request(&self, action: &str, dir: &Path) -> Result<()> {
        let id = dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| Error::InvalidState("invalid quota directory".into()))?;
        let mut socket = UnixStream::connect(&self.socket)?;
        socket.set_read_timeout(Some(Duration::from_secs(10)))?;
        socket.set_write_timeout(Some(Duration::from_secs(10)))?;
        serde_json::to_writer(&mut socket, &serde_json::json!({"action":action,"id":id}))?;
        socket.write_all(b"\n")?;
        let mut reply = String::new();
        BufReader::new(socket.take(4097)).read_line(&mut reply)?;
        if reply.len() > 4096 || !reply.ends_with('\n') {
            return Err(Error::InvalidState("invalid quota broker reply".into()));
        }
        let reply: serde_json::Value = serde_json::from_str(&reply)?;
        if reply["ok"] != true {
            return Err(Error::InvalidState(format!(
                "storage quota: {}",
                reply["error"]
            )));
        }
        // Refuse a broker configured for a different backend tree.
        let expected = dir
            .parent()
            .ok_or_else(|| Error::InvalidState("quota parent missing".into()))?
            .canonicalize()?
            .join(id);
        if reply["path"].as_str() != expected.to_str() {
            return Err(Error::InvalidState(
                "quota broker directory mismatch".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn remove(&self, dir: &Path) -> Result<()> {
        // The privileged broker removes only an empty project root. All recursive
        // cleanup runs with the ordinary daemon uid, never with root authority.
        if !dir.exists() {
            return self.request("release", dir);
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
            } else {
                std::fs::remove_file(entry.path())?;
            }
        }
        self.request("release", dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    #[test]
    fn rejects_wrong_tree_and_missing_broker() {
        let root = std::env::temp_dir().join(format!("ahvm-quota-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let cfg = StorageConfig {
            socket: root.join("q.sock"),
        };
        assert!(cfg.request("verify", &root.join("vm")).is_err());
        let listener = UnixListener::bind(&cfg.socket).unwrap();
        let task = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(conn.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            conn.write_all(b"{\"ok\":true,\"path\":\"/wrong/vm\"}\n")
                .unwrap();
        });
        assert!(cfg.request("prepare", &root.join("vm")).is_err());
        task.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
