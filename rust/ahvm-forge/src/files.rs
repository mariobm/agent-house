//! File operations jailed under the configured root. Writes are atomic
//! (temp file + rename). `..` escapes are rejected after resolving
//! symlinks against the root.

use crate::agent::{DirEntry, FileReq, FileResp};
use crate::config::Config;
use std::io::Read;
use std::path::{Component, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Sibling temp file with a SHORT name independent of the destination:
/// embedding the full stem overflows NAME_MAX on long filenames (and
/// retries a permanent error forever). Created exclusively: pre-planting
/// the path always fails, so a symlink at the temp location can neither
/// redirect the write nor be destroyed. Only true name collisions retry.
fn unique_sibling(dest: &std::path::Path) -> Option<(PathBuf, std::fs::File)> {
    let pid = std::process::id();
    for _ in 0..10 {
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        // ~30 chars regardless of destination length.
        let name = format!(".ahvm-tmp-{pid}-{nanos}-{n}");
        let cand = dest.with_file_name(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&cand)
        {
            Ok(f) => return Some((cand, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

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
        FileReq::Upload { .. } => Err("upload requires a streaming connection".into()),
        FileReq::Read {
            path,
            offset,
            limit,
        } => {
            let p = jail(&cfg.root, path)?;
            let mut f = std::fs::File::open(&p).map_err(|e| format!("open: {e}"))?;
            use std::io::Seek;
            f.seek(std::io::SeekFrom::Start(*offset))
                .map_err(|e| format!("seek: {e}"))?;
            // Clamp to what fits one response frame (see exec::STREAM_CAP);
            // callers page with offset/limit for more.
            let n = (*limit)
                .min(cfg.max_output_bytes as u64)
                .min(crate::exec::STREAM_CAP as u64) as usize;
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
            // Unique temp name created exclusively: a predictable name lets
            // a pre-planted symlink redirect the write outside the jail
            // (or destroy a sibling file), and concurrent writes collide.
            let (tmp, mut tmp_f) = unique_sibling(&p).ok_or("write: temp name exhausted")?;
            // Cleanup guard: a failed write or rename must not leave the
            // temp file behind (repeated failures would consume disk).
            // Disarmed by forgetting after a successful rename.
            struct RmGuard(Option<PathBuf>);
            impl Drop for RmGuard {
                fn drop(&mut self) {
                    if let Some(p) = self.0.take() {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }
            let mut guard = RmGuard(Some(tmp.clone()));
            use std::io::Write as _;
            tmp_f.write_all(&data).map_err(|e| format!("write: {e}"))?;
            drop(tmp_f);
            std::fs::rename(&tmp, &p).map_err(|e| format!("rename: {e}"))?;
            guard.0.take();
            Ok(FileResp::Write {
                bytes: data.len() as u64,
            })
        }
        FileReq::List {
            path,
            offset,
            limit,
        } => {
            // Pages, not dumps: entries stream unbounded while one response
            // frame caps at 1 MiB, so an unbounded listing used to kill the
            // connection with no response at all. Oversized pages are still
            // possible with pathological names; the single encode in the
            // dispatcher turns those into an explicit error frame.
            const MAX_PAGE: u64 = 1000;
            let p = jail(&cfg.root, path)?;
            let take = (*limit).clamp(1, MAX_PAGE) as usize;
            let skip = (*offset) as usize;
            // Correct cross-page ordering needs the full name set sorted
            // before slicing; cap the scan so adversarial directories fail
            // loudly instead of eating the host.
            const MAX_SCAN: usize = 50_000;
            let mut names: Vec<std::ffi::OsString> = Vec::new();
            for ent in std::fs::read_dir(&p).map_err(|e| format!("list: {e}"))? {
                let ent = ent.map_err(|e| format!("list: {e}"))?;
                if names.len() >= MAX_SCAN {
                    return Err(format!("directory too large to list (>{MAX_SCAN} entries)"));
                }
                names.push(ent.file_name());
            }
            names.sort();
            let mut entries = Vec::new();
            let end = skip.saturating_add(take).min(names.len());
            let has_more = end < names.len();
            for name in &names[skip.min(names.len())..end] {
                let meta =
                    std::fs::symlink_metadata(p.join(name)).map_err(|e| format!("stat: {e}"))?;
                entries.push(DirEntry {
                    name: name.to_string_lossy().into_owned(),
                    is_dir: meta.is_dir(),
                    size: meta.len(),
                });
            }
            let resp = FileResp::List {
                entries,
                next_offset: has_more.then_some(end as u64),
            };
            Ok(resp)
        }
    }
}

/// A connection-scoped transaction. Disconnect, invalid frames, timeout and
/// write failures drop the temporary file without changing the destination.
pub fn upload(
    path: &str,
    cfg: &Config,
    r: &mut std::io::BufReader<crate::agent::Conn>,
    w: &mut crate::agent::Conn,
) -> Result<u64, String> {
    use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
    use std::io::Write;
    r.get_ref()
        .timeout(std::time::Duration::from_secs(30))
        .map_err(|e| e.to_string())?;
    let dest = jail(&cfg.root, path)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let (tmp, mut file) = unique_sibling(&dest).ok_or("cannot create upload temporary file")?;
    struct Cleanup(Option<PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(path) = &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let mut cleanup = Cleanup(Some(tmp.clone()));
    write_frame(
        w,
        &Frame {
            msg_type: FrameType::FileResp,
            payload: br#"{"op":"upload_ready"}"#.to_vec(),
        },
    )
    .map_err(|e| e.to_string())?;
    let mut bytes = 0u64;
    loop {
        let frame = read_frame(r).map_err(|e| format!("upload interrupted: {e}"))?;
        match frame.msg_type {
            FrameType::FileData => {
                if frame.payload.is_empty() || frame.payload.len() > 65536 {
                    return Err("upload chunks must be 1..=65536 bytes".into());
                }
                file.write_all(&frame.payload)
                    .map_err(|e| format!("upload write: {e}"))?;
                bytes = bytes
                    .checked_add(frame.payload.len() as u64)
                    .ok_or("upload size overflow")?;
            }
            FrameType::FileCommit => {
                let claimed: u64 = serde_json::from_slice(&frame.payload)
                    .map_err(|e| format!("upload commit: {e}"))?;
                if claimed != bytes {
                    return Err("upload length mismatch".into());
                }
                file.sync_all().map_err(|e| format!("upload sync: {e}"))?;
                std::fs::rename(&tmp, &dest).map_err(|e| format!("upload publish: {e}"))?;
                cleanup.0.take();
                return Ok(bytes);
            }
            _ => return Err("expected upload data or commit".into()),
        }
    }
}
