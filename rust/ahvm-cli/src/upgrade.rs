use crate::{distribution, Result};
use std::{
    fs,
    io::IsTerminal,
    path::PathBuf,
    process::{Command, Stdio},
};
pub fn platform() -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        _ => return Err("unsupported client platform".into()),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return Err("unsupported client architecture".into()),
    };
    Ok(format!("{os}-{arch}"))
}
fn homebrew(path: &std::path::Path) -> bool {
    path.components().any(|c| c.as_os_str() == "Cellar")
}
pub fn run() -> Result<i32> {
    let executable = std::env::current_exe()?.canonicalize()?;
    if homebrew(&executable) {
        return Err("Homebrew manages this CLI. Run: brew upgrade ahvm".into());
    }
    if executable.starts_with("/opt/ahvm-rust") {
        return Err(
            "this CLI belongs to the server bundle; use ahvm host upgrade <name> from your client"
                .into(),
        );
    }
    let catalog = distribution::catalog(&distribution::catalog_url())?;
    let platform = platform()?;
    let bundled = catalog.client.get(&platform);
    let artifact = bundled
        .or_else(|| catalog.cli.get(&platform))
        .ok_or("no CLI release available for this platform")?;
    let repair_viewer = bundled.is_some()
        && platform.starts_with("darwin-")
        && !executable.parent().unwrap().join("ahvm-desktop").is_file();
    let release = semver::Version::parse(&artifact.version)?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
    if release < current || (release == current && !repair_viewer) {
        eprintln!("AHVM is up to date ({})", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if artifact.unpacked_size > 256 * 1024 * 1024 {
        return Err("CLI size exceeds limit".into());
    }
    let parent = executable.parent().ok_or("invalid executable path")?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".ahvm-upgrade.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut download = tempfile::NamedTempFile::new_in(parent)?;
    distribution::download(artifact, download.as_file_mut())?;
    let mut candidate = tempfile::NamedTempFile::new_in(parent)?;
    let stage = tempfile::tempdir_in(parent)?;
    if bundled.is_some() {
        unpack_client(download.path(), stage.path(), artifact.unpacked_size)?;
        std::io::copy(
            &mut fs::File::open(stage.path().join("ahvm"))?,
            candidate.as_file_mut(),
        )?;
        if platform.starts_with("darwin-") && !stage.path().join("ahvm-desktop").is_file() {
            return Err("macOS client bundle is missing its viewer".into());
        }
    } else {
        distribution::unpack_gzip(
            download.path(),
            candidate.as_file_mut(),
            artifact.unpacked_size,
        )?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        candidate
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    let output = Command::new(candidate.path()).arg("--version").output()?;
    if !output.status.success()
        || String::from_utf8(output.stdout)?.trim() != format!("ahvm {}", artifact.version)
    {
        return Err("downloaded CLI failed its version check".into());
    }
    // Keep one previous binary for a manual recovery, publish atomically.
    let mut previous = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut fs::File::open(&executable)?, previous.as_file_mut())?;
    previous
        .as_file()
        .set_permissions(fs::metadata(&executable)?.permissions())?;
    previous.as_file().sync_all()?;
    previous.persist(parent.join("ahvm.previous"))?;
    // Companions are validated before publication. Publish the CLI last so a
    // failed companion install leaves the previous working CLI in place.
    for entry in fs::read_dir(stage.path())? {
        let entry = entry?;
        if entry.file_name() != "ahvm" {
            fs::rename(entry.path(), parent.join(entry.file_name()))?;
        }
    }
    candidate.persist(&executable)?;
    eprintln!("Updated AHVM to {}", artifact.version);
    Ok(0)
}
fn cache_path() -> Option<PathBuf> {
    std::env::var_os("AHVM_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config/ahvm")))
        .map(|p| p.join("update.json"))
}
pub fn refresh() -> Result<i32> {
    let Some(path) = cache_path() else {
        return Ok(0);
    };
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join("update.lock"))?;
    if fs2::FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(0);
    }
    let recent = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v["checked"].as_u64())
        .is_some_and(|t| distribution::now().saturating_sub(t) < 86400);
    if recent {
        return Ok(0);
    }
    let version = distribution::catalog(&distribution::catalog_url())
        .ok()
        .and_then(|c| c.cli.get(&platform().ok()?).map(|a| a.version.clone()));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(
        &mut tmp,
        &serde_json::json!({"checked":distribution::now(),"version":version}),
    )?;
    tmp.persist(path)?;
    Ok(0)
}
pub fn notice() {
    if !std::io::stderr().is_terminal()
        || std::env::var_os("AHVM_NO_UPDATE_CHECK").is_some()
        || std::env::args_os().any(|a| a == "--json")
    {
        return;
    }
    let Some(path) = cache_path() else {
        return;
    };
    let cache = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
    if let Some(version) = cache.as_ref().and_then(|v| v["version"].as_str()) {
        if semver::Version::parse(version)
            .ok()
            .zip(semver::Version::parse(env!("CARGO_PKG_VERSION")).ok())
            .is_some_and(|(new, old)| new > old)
        {
            let brew = std::env::current_exe()
                .ok()
                .and_then(|p| p.canonicalize().ok())
                .is_some_and(|p| homebrew(&p));
            eprintln!(
                "AHVM {version} is available. Run: {}",
                if brew {
                    "brew upgrade ahvm"
                } else {
                    "ahvm upgrade"
                }
            );
        }
    }
    if cache
        .and_then(|v| v["checked"].as_u64())
        .is_none_or(|t| distribution::now().saturating_sub(t) >= 86400)
    {
        if let Ok(exe) = std::env::current_exe() {
            let _ = Command::new(exe)
                .arg("check-updates")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    }
}

/// Only flat, regular files from the client format; never extract archive paths.
fn unpack_client(source: &std::path::Path, target: &std::path::Path, expected: u64) -> Result<()> {
    if expected == 0 || expected > 256 * 1024 * 1024 {
        return Err("invalid client size".into());
    }
    let decoder = flate2::read::GzDecoder::new(fs::File::open(source)?);
    let mut archive = tar::Archive::new(std::io::Read::take(decoder, expected + 65536));
    let mut total = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.to_str().ok_or("invalid client filename")?;
        if !matches!(
            name,
            "ahvm"
                | "ahvm-desktop"
                | "ahvm-desktop-AHVM-LICENSE"
                | "ahvm-desktop-noVNC-LICENSE.txt"
                | "ahvm-desktop-noVNC-AUTHORS"
                | "ahvm-desktop-pako-LICENSE"
        ) || !entry.header().entry_type().is_file()
        {
            return Err("unexpected client archive entry".into());
        }
        total = total
            .checked_add(entry.size())
            .ok_or("client size overflow")?;
        if total > expected {
            return Err("oversized client bundle".into());
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target.join(name))?;
        std::io::copy(&mut entry, &mut file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(
                if matches!(name, "ahvm" | "ahvm-desktop") {
                    0o755
                } else {
                    0o644
                },
            ))?;
        }
        file.sync_all()?;
    }
    if total != expected || !target.join("ahvm").is_file() {
        return Err("incomplete client bundle".into());
    }
    Ok(())
}

#[cfg(test)]
mod bundle_tests {
    use super::*;
    fn archive(entries: &[(&str, &[u8], bool)]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let gzip = flate2::write::GzEncoder::new(
                file.reopen().unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(gzip);
            for (name, body, link) in entries {
                let mut header = tar::Header::new_ustar();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                if *link {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_link_name("/tmp/escape").unwrap();
                }
                header.set_cksum();
                tar.append_data(&mut header, name, *body).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        file
    }
    #[test]
    fn client_archive_preserves_companions_and_rejects_unsafe_entries() {
        let good = archive(&[("ahvm", b"cli", false), ("ahvm-desktop", b"viewer", false)]);
        let stage = tempfile::tempdir().unwrap();
        unpack_client(good.path(), stage.path(), 9).unwrap();
        assert_eq!(
            fs::read(stage.path().join("ahvm-desktop")).unwrap(),
            b"viewer"
        );
        for bad in [
            archive(&[("ahvm", b"", true)]),
            archive(&[("ahvm", b"cli", false), ("ahvm", b"cli", false)]),
            archive(&[("unexpected", b"cli", false)]),
        ] {
            assert!(unpack_client(bad.path(), tempfile::tempdir().unwrap().path(), 6).is_err());
        }
        assert!(unpack_client(good.path(), tempfile::tempdir().unwrap().path(), 8).is_err());
        assert!(unpack_client(good.path(), tempfile::tempdir().unwrap().path(), 10).is_err());
    }
}
