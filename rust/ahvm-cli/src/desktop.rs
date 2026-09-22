//! Launch the separately installed native viewer with credentials on stdin.
use crate::{client::Api, Result};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Resolution {
    #[value(name = "720p")]
    Hd,
    #[value(name = "1080p")]
    FullHd,
}

pub fn launch(api: &Api, id: &str, viewer: Option<PathBuf>, resolution: Resolution) -> Result<i32> {
    let resolution = match resolution {
        Resolution::Hd => "720p",
        Resolution::FullHd => "1080p",
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let result = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("desktop wake timed out; retry when the VM is ready".into());
        }
        let result = api.call_with_timeout(reqwest::Method::POST, &["sandboxes", id, "exec"], &[],
            Some(serde_json::json!({"argv": ["python3", "-c", include_str!("desktop_resolution.py"), resolution]})),
            Some(remaining.min(std::time::Duration::from_secs(30))));
        match result {
            Ok(value) => break value,
            // These Cloud responses explicitly mean the guest request has not run.
            Err(error) if crate::client::is_waking(error.as_ref()) => {
                std::thread::sleep(std::time::Duration::from_secs(2))
            }
            Err(error) => return Err(error),
        }
    };
    if result["exit_code"].as_i64() != Some(0) {
        return Err(format!(
            "desktop resolution failed: {}",
            result["stderr"].as_str().unwrap_or("guest command failed")
        )
        .into());
    }

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
