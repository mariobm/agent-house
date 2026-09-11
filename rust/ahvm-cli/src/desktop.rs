//! Launch the separately installed native viewer with credentials on stdin.
use crate::{client::Api, Result};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

pub fn launch(api: &Api, id: &str, viewer: Option<PathBuf>) -> Result<i32> {
    let path = viewer.unwrap_or_else(|| {
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("ahvm-desktop")));
        sibling
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("ahvm-desktop"))
    });
    let request = api.desktop_request(id)?;
    let settings = serde_json::json!({
        "url": request.uri().to_string(),
        "authorization": request.headers()["Authorization"].to_str()?,
        "title": id,
    });
    let mut child = Command::new(path).env_remove("AHVM_TOKEN").env_remove("AHVM_TOKEN_FILE").stdin(Stdio::piped()).spawn().map_err(|e| {
        format!("cannot launch optional ahvm-desktop viewer: {e}; install the viewer or use --viewer /path/to/ahvm-desktop")
    })?;
    let write = (|| -> Result<()> {
        let mut input = child.stdin.take().ok_or("viewer stdin unavailable")?;
        input.write_all(&serde_json::to_vec(&settings)?)?;
        Ok(())
    })();
    if let Err(e) = write {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e);
    }
    eprintln!(
        "Desktop {id}; closing the viewer leaves the VM running. Remove it with: ahvm delete {id}"
    );
    Ok(child.wait()?.code().unwrap_or(1))
}
