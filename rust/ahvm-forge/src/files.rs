//! File operations jailed under the configured root. Writes are atomic
//! (temp file + rename). `..` escapes are rejected after resolving
//! symlinks against the root.

use crate::agent::{DirEntry, FileReq, FileResp};
use crate::config::Config;
use std::io::Read;
use std::path::{Component, PathBuf};

fn jail(root: &std::path::Path, req: &str) -> Result<PathBuf, String> {
    let mut out = PathBuf::from(root);
    for comp in std::path::Path::new(req).components() {
        match comp {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::RootDir => {}
            _ => return Err(format!("path escapes jail: {req:?}")),
        }
    }
    // Resolve symlinks and re-check containment. For not-yet-existing
    // paths (writes), resolve the longest existing ancestor instead, so a
    // symlinked parent directory can't redirect the write outside the jail.
    // (TOCTOU-advise: single-user guest agent; full hardening lands with
    // the production review.)
    let anchor = {
        let mut a = out.as_path();
        while !a.exists() {
            match a.parent() {
                Some(p) if !p.as_os_str().is_empty() => a = p,
                _ => break,
            }
        }
        a
    };
    if let (Ok(real), Ok(real_root)) = (anchor.canonicalize(), root.canonicalize()) {
        if !real.starts_with(&real_root) {
            return Err(format!("path escapes jail: {req:?}"));
        }
    }
    Ok(out)
}

pub fn serve(req: &FileReq, cfg: &Config) -> Result<FileResp, String> {
    match req {
        FileReq::Read { path, offset, limit } => {
            let p = jail(&cfg.root, path)?;
            let mut f = std::fs::File::open(&p).map_err(|e| format!("open: {e}"))?;
            use std::io::Seek;
            f.seek(std::io::SeekFrom::Start(*offset)).map_err(|e| format!("seek: {e}"))?;
            // Clamp to what fits one response frame (see exec::STREAM_CAP);
            // callers page with offset/limit for more.
            let n = (*limit).min(cfg.max_output_bytes as u64).min(crate::exec::STREAM_CAP as u64) as usize;
            let mut buf = vec![0u8; n];
            let mut got = 0;
            while got < n {
                match f.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(m) => got += m,
                    Err(e) => return Err(format!("read: {e}")),
                }
            }
            buf.truncate(got);
            let eof = got < n;
            Ok(FileResp::Read {
                data_b64: crate::agent::b64(&buf),
                eof,
            })
        }
        FileReq::Write { path, data_b64 } => {
            use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
            let data = B64.decode(data_b64).map_err(|e| format!("base64: {e}"))?;
            if data.len() > cfg.max_output_bytes {
                return Err("write too large".into());
            }
            let p = jail(&cfg.root, path)?;
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
            }
            let tmp = p.with_extension("ahvm-tmp");
            std::fs::write(&tmp, &data).map_err(|e| format!("write: {e}"))?;
            std::fs::rename(&tmp, &p).map_err(|e| format!("rename: {e}"))?;
            Ok(FileResp::Write {
                bytes: data.len() as u64,
            })
        }
        FileReq::List { path } => {
            let p = jail(&cfg.root, path)?;
            let mut entries = Vec::new();
            for ent in std::fs::read_dir(&p).map_err(|e| format!("list: {e}"))? {
                let ent = ent.map_err(|e| format!("list: {e}"))?;
                let meta = ent.metadata().map_err(|e| format!("stat: {e}"))?;
                entries.push(DirEntry {
                    name: ent.file_name().to_string_lossy().into_owned(),
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                });
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(FileResp::List { entries })
        }
    }
}
