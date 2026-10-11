//! Privileged, opt-in capture of immutable project images. Guest filesystems are
//! never mounted on the host. Every export/upload is preceded by a durable charge.
use super::*;
use ahvm_engine::{
    project_digest, project_uuid, ProjectImageInfo, ProjectImageMetadata, ProjectImageRequest,
    ProjectImageRetirement, PROJECT_IMAGE_HEADROOM_BYTES,
};
use std::{
    io::{Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};

const MAX_CAPTURE_TIME: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Exporting,
    Importing,
    Ready,
    Failed,
    Retiring,
    Retired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Candidate {
    schema: u32,
    image_id: String,
    source_volume_id: String,
    sandbox_dir: PathBuf,
    output_root: PathBuf,
    source_logical_bytes: u64,
    logical_bytes: u64,
    metadata: ProjectImageMetadata,
    source_revision: String,
    source_manifest_sha256: String,
    export_sha256: Option<String>,
    digest: Option<String>,
    base: Option<crate::BaseRef>,
    file_identity: Option<ImageIdentity>,
    local_published: bool,
    phase: Phase,
    error_code: Option<String>,
    remote_retired: bool,
    retirement_verified: bool,
    /// A shrinking remote namespace is always resumed from its first page.
    /// This durable phase/count is the cleanup checkpoint, never a list offset.
    cleanup_passes: u64,
    remote_clean: bool,
    logical_released: bool,
    reclamation_complete: bool,
}
impl Candidate {
    fn path(&self) -> Result<PathBuf> {
        Ok(self.output_root.join(format!(
            "{}.ext4",
            self.digest.as_ref().ok_or("image digest unavailable")?
        )))
    }
    fn temporary(&self) -> PathBuf {
        self.output_root.join(format!(".{}.partial", self.image_id))
    }
    fn validate(&self, config: &Config) -> Result<()> {
        self.metadata.validate()?;
        if self.error_code.as_deref().is_some_and(|code| {
            ![
                "project_image_capture_failed",
                "project_image_capture_interrupted",
            ]
            .contains(&code)
        }) || (self.phase == Phase::Failed && self.error_code.is_none())
        {
            return Err("invalid project image failure proof".into());
        }
        if self.schema != 1
            || !project_uuid(&self.image_id)
            || self.image_id != self.metadata.build_id
            || !valid_id(&self.source_volume_id)
            || self.sandbox_dir.parent() != Some(&config.engine_root)
            || crate::project_image::logical_bytes(self.source_logical_bytes)? != self.logical_bytes
            || self.source_revision.is_empty()
            || self.source_revision.len() > 256
            || !project_digest(&self.source_manifest_sha256)
            || self
                .export_sha256
                .as_deref()
                .is_some_and(|s| !project_digest(s))
            || self.digest.as_deref().is_some_and(|s| !project_digest(s))
            || self.base.as_ref().is_some_and(|b| {
                Some(&b.image) != self.digest.as_ref() || !project_digest(&b.catalog)
            })
            || (self.phase != Phase::Retired
                && config.project_image_root.as_ref() != Some(&self.output_root))
            || (self.phase == Phase::Ready
                && (self.digest.is_none()
                    || self.base.is_none()
                    || !self.local_published
                    || self.file_identity.is_none()))
            || (self.phase == Phase::Retired
                && (!self.logical_released || !self.reclamation_complete))
            || (self.phase != Phase::Retired
                && (self.logical_released || self.reclamation_complete))
            || (self.remote_clean && !self.remote_retired)
        {
            return Err("invalid project image record".into());
        }
        Ok(())
    }
    fn matching(&self, q: &Request, request: &ProjectImageRequest) -> Result<()> {
        if self.source_volume_id != q.volume_id
            || self.sandbox_dir != q.sandbox_dir
            || request
                .metadata
                .as_ref()
                .is_some_and(|m| m != &self.metadata)
            || request
                .source_logical_bytes
                .is_some_and(|n| n != self.source_logical_bytes)
            || request
                .expected_digest
                .as_ref()
                .is_some_and(|d| Some(d) != self.digest.as_ref())
        {
            return Err("project image intent changed".into());
        }
        Ok(())
    }
    fn retirement(&self) -> ProjectImageRetirement {
        ProjectImageRetirement {
            logical_released: self.logical_released,
            reclamation_complete: self.reclamation_complete,
        }
    }
}

fn registry(config: &Config) -> PathBuf {
    config.root.join("project-images")
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedIntent {
    schema: u32,
    image_id: String,
    volume_scope: String,
    sandbox_scope: String,
    request_sha256: String,
    error_code: String,
    retired: bool,
}
fn intent_root(config: &Config) -> PathBuf {
    config.root.join("project-image-intents")
}
fn intent_path(config: &Config, id: &str) -> PathBuf {
    intent_root(config).join(format!("{id}.json"))
}
fn intent_lock(config: &Config, id: &str) -> Result<Lock> {
    private_dir(&intent_root(config))?;
    // A quiescent failed reply must survive losing the entire new directory
    // entry, not merely its record rename within that directory.
    File::open(&config.root)?.sync_all()?;
    Lock::take(&intent_root(config).join(format!("{id}.lock")))
}
fn failed_intent(config: &Config, q: &Request, id: &str) -> Result<Option<FailedIntent>> {
    let path = intent_path(config, id);
    if !path.exists() {
        return Ok(None);
    }
    let failure: FailedIntent = read(&path)?;
    if failure.schema != 1
        || failure.image_id != id
        || !project_digest(&failure.request_sha256)
        || failure.volume_scope != crate::digest(q.volume_id.as_bytes())
        || failure.sandbox_scope != crate::digest(q.sandbox_dir.as_os_str().as_encoded_bytes())
        || ![
            "project_image_capture_failed",
            "project_image_capture_interrupted",
        ]
        .contains(&failure.error_code.as_str())
    {
        return Err("sealed project image intent mismatch".into());
    }
    Ok(Some(failure))
}
fn seal_failed_intent(
    config: &Config,
    q: &Request,
    request: &ProjectImageRequest,
    code: &str,
) -> Result<()> {
    if let Some(existing) = failed_intent(config, q, &request.image_id)? {
        if existing.request_sha256 != crate::digest(&serde_json::to_vec(request)?) {
            return Err("sealed project image intent changed".into());
        }
        return Ok(());
    }
    private_dir(&intent_root(config))?;
    save(
        &intent_path(config, &request.image_id),
        &FailedIntent {
            schema: 1,
            image_id: request.image_id.clone(),
            volume_scope: crate::digest(q.volume_id.as_bytes()),
            sandbox_scope: crate::digest(q.sandbox_dir.as_os_str().as_encoded_bytes()),
            request_sha256: crate::digest(&serde_json::to_vec(request)?),
            error_code: code.into(),
            retired: false,
        },
    )
}
fn proof(
    q: &Request,
    phase: &str,
    quiescent: bool,
    error: Option<&str>,
    image: Option<ProjectImageInfo>,
    ok: bool,
) -> serde_json::Value {
    serde_json::json!({"ok":ok,"volume_id":q.volume_id,"project_image":image,"project_image_phase":phase,"project_image_quiescent":quiescent,"project_image_error_code":error})
}
fn candidate_path(config: &Config, id: &str) -> PathBuf {
    registry(config).join(format!("{id}.json"))
}
fn records(config: &Config) -> Result<Vec<Candidate>> {
    let root = registry(config);
    if !root.exists() {
        return Ok(vec![]);
    }
    private_dir(&root)?;
    let mut out = vec![];
    for item in fs::read_dir(root)? {
        let item = item?;
        let name = item.file_name().to_string_lossy().to_string();
        let metadata = fs::symlink_metadata(item.path())?;
        if !metadata.is_file()
            || metadata.uid() != 0
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err("unexpected project registry file".into());
        }
        if name.strip_suffix(".tmp").is_some_and(project_uuid) {
            continue;
        }
        let id = name
            .strip_suffix(".json")
            .filter(|id| project_uuid(id))
            .ok_or("invalid project registry key")?;
        let record: Candidate = read(&item.path())?;
        record.validate(config)?;
        if record.image_id != id {
            return Err("project registry key mismatch".into());
        }
        out.push(record);
    }
    Ok(out)
}
fn persist(config: &Config, candidate: &Candidate) -> Result<()> {
    candidate.validate(config)?;
    save(&candidate_path(config, &candidate.image_id), candidate)
}
fn load(config: &Config, id: &str) -> Result<Candidate> {
    if !project_uuid(id) {
        return Err("invalid project image ID".into());
    }
    records(config)?
        .into_iter()
        .find(|r| r.image_id == id)
        .ok_or_else(|| "unknown project image".into())
}
pub(super) fn logical_charge(config: &Config) -> Result<u64> {
    records(config)?
        .iter()
        .filter(|r| !r.logical_released)
        .try_fold(0u64, |sum, r| {
            sum.checked_add(r.logical_bytes)
                .ok_or_else(|| "project image accounting overflow".into())
        })
}
fn pending_bytes(config: &Config, except: Option<&str>) -> Result<u64> {
    records(config)?
        .iter()
        .filter(|r| {
            !r.logical_released && !r.local_published && Some(r.image_id.as_str()) != except
        })
        .try_fold(0u64, |sum, r| {
            sum.checked_add(r.logical_bytes)
                .ok_or_else(|| "project image accounting overflow".into())
        })
}

/// Existing broker configuration is the proof of the trusted daemon group and
/// disjoint worker UID allocation. No worker role gains control-plane authority.
fn daemon_group(config: &Config) -> Result<u32> {
    let Some(path) = &config.worker_broker_config else {
        if config.client_uid == 0 && config.worker_broker_socket.is_none() {
            return Ok(0);
        }
        return Err("project images require the root-owned worker broker configuration".into());
    };
    trusted_image_path(path)?;
    let value: serde_json::Value = read(path)?;
    let number = |name: &str| -> Result<u32> {
        Ok(u32::try_from(
            value[name]
                .as_u64()
                .ok_or("invalid worker identity configuration")?,
        )?)
    };
    let uid = number("daemon_uid")?;
    let gid = number("daemon_gid")?;
    let base = number("uid_base")?;
    let gid_base = number("gid_base")?;
    let count = number("identity_count")?;
    let end = base
        .checked_add(count)
        .ok_or("worker identity range overflow")?;
    let gid_end = gid_base
        .checked_add(count)
        .ok_or("worker group range overflow")?;
    if uid == 0
        || gid == 0
        || base < 65536
        || gid_base < 65536
        || count == 0
        || count > 16_777_216
        || end >= i32::MAX as u32
        || gid_end >= i32::MAX as u32
        || (config.client_uid != 0 && uid != config.client_uid)
        || (base..end).contains(&config.client_uid)
        || (base..end).contains(&uid)
        || (gid_base..gid_end).contains(&gid)
        || config
            .worker_broker_socket
            .as_ref()
            .and_then(|s| s.to_str())
            != value["socket"].as_str()
    {
        return Err("project image daemon/worker identity proof failed".into());
    }
    Ok(gid)
}
fn configured_root(config: &Config) -> Result<&Path> {
    let root = config
        .project_image_root
        .as_deref()
        .ok_or("project images unavailable")?;
    if !root.is_absolute()
        || root.canonicalize()? != root
        || !root.is_dir()
        || !config
            .image_roots
            .iter()
            .any(|p| root.starts_with(p) && root != p)
    {
        return Err("project image root must be a canonical directory beneath image_roots".into());
    }
    trusted_image_path(root)?;
    let group = daemon_group(config)?;
    let metadata = fs::symlink_metadata(root)?;
    if metadata.uid() != 0
        || metadata.gid() != group
        || (config.client_uid != 0 && metadata.mode() & 0o777 != 0o750)
        || (config.client_uid == 0 && ![0o700, 0o750].contains(&(metadata.mode() & 0o777)))
    {
        return Err(
            "project image root requires root ownership and private daemon-group traversal".into(),
        );
    }
    Ok(root)
}
pub(super) fn initialize(config: &Config) -> Result<()> {
    if config.project_image_root.is_some() {
        configured_root(config)?;
        private_dir(&registry(config))?;
        File::open(&config.root)?.sync_all()?;
    }
    // Disabling/changing a configured root cannot silently erase old charges.
    let _ = records(config)?;
    Ok(())
}
fn available(config: &Config) -> Result<u64> {
    let root = configured_root(config)?;
    let stat = rustix::fs::statvfs(root)?;
    stat.f_bavail
        .checked_mul(stat.f_frsize)
        .ok_or_else(|| "project image geometry overflow".into())
}
fn check_headroom(config: &Config, bytes: u64, image_id: &str) -> Result<()> {
    let required = bytes
        .checked_add(pending_bytes(config, Some(image_id))?)
        .and_then(|n| n.checked_add(PROJECT_IMAGE_HEADROOM_BYTES))
        .ok_or("project image headroom overflow")?;
    if available(config)? < required {
        return Err("project image filesystem headroom exhausted".into());
    }
    Ok(())
}
fn identity(file: &File) -> Result<ImageIdentity> {
    let m = file.metadata()?;
    if !m.is_file() || m.uid() != 0 || m.nlink() != 1 || m.mode() & 0o022 != 0 {
        return Err("untrusted project image file".into());
    }
    Ok((
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    ))
}
struct LockedFile {
    file: File,
    locked: bool,
}
impl std::ops::Deref for LockedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.file
    }
}
impl std::ops::DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.file
    }
}
impl LockedFile {
    fn release(mut self) -> Result<()> {
        self.file.unlock()?;
        self.locked = false;
        Ok(())
    }
}
impl Drop for LockedFile {
    fn drop(&mut self) {
        if self.locked {
            // Forked children can briefly retain the open file description even
            // with CLOEXEC. Closing our FD alone would not release that flock.
            let _ = self.file.unlock();
        }
    }
}
fn open_file(path: &Path, writable: bool, create: bool) -> Result<LockedFile> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(writable)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?;
    identity(&file)?;
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "project image file busy")
        }
        fs::TryLockError::Error(error) => error,
    })?;
    Ok(LockedFile { file, locked: true })
}
fn info(candidate: &Candidate) -> Result<ProjectImageInfo> {
    if candidate.phase != Phase::Ready {
        return Err("project image is not ready".into());
    }
    let path = candidate.path()?;
    let mut file = open_file(&path, false, false)?;
    if Some(identity(&file)?) != candidate.file_identity
        || file.metadata()?.len() != candidate.logical_bytes
        || crate::project_image::read_metadata(&mut file)? != candidate.metadata
    {
        return Err("project image changed after publication".into());
    }
    Ok(ProjectImageInfo {
        image_id: candidate.image_id.clone(),
        digest: candidate.digest.clone().ok_or("missing digest")?,
        image_path: path,
        logical_bytes: candidate.logical_bytes,
        source_volume_id: candidate.source_volume_id.clone(),
        metadata: candidate.metadata.clone(),
    })
}

pub(super) fn check_image_admission(service: &Service, image: &Path) -> Result<()> {
    for candidate in records(&service.config)? {
        if candidate.digest.is_some()
            && candidate.path()? == image
            && candidate.phase != Phase::Ready
        {
            return Err("project image is not available for volume admission".into());
        }
    }
    if service
        .config
        .project_image_root
        .as_ref()
        .is_some_and(|p| image.starts_with(p))
    {
        let candidate = records(&service.config)?
            .into_iter()
            .find(|r| r.digest.is_some() && r.path().is_ok_and(|p| p == image))
            .ok_or("unregistered private project image")?;
        info(&candidate)?;
    }
    Ok(())
}
pub(super) fn check_digest_admission(service: &Service, digest: &str) -> Result<()> {
    if records(&service.config)?
        .iter()
        .any(|r| r.digest.as_deref() == Some(digest) && r.phase != Phase::Ready)
    {
        return Err("private project image is not available for import".into());
    }
    Ok(())
}

struct SparseExport<'a> {
    file: &'a mut File,
    deadline: Instant,
    written: u64,
    limit: u64,
}
impl Write for SparseExport<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "project export deadline",
            ));
        }
        if self
            .written
            .checked_add(bytes.len() as u64)
            .is_none_or(|n| n > self.limit)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "project export exceeds reservation",
            ));
        }
        let count = if bytes.iter().all(|b| *b == 0) {
            self.file.seek(SeekFrom::Current(bytes.len() as i64))?;
            bytes.len()
        } else {
            self.file.write(bytes)?
        };
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn publish_file(config: &Config, candidate: &mut Candidate, file: &File) -> Result<()> {
    let path = candidate.path()?;
    fs::set_permissions(candidate.temporary(), fs::Permissions::from_mode(0o444))?;
    file.sync_all()?;
    // Atomic create-only publication; an operator image is never overwritten.
    fs::hard_link(candidate.temporary(), &path)?;
    File::open(&candidate.output_root)?.sync_all()?;
    fs::remove_file(candidate.temporary())?;
    File::open(&candidate.output_root)?.sync_all()?;
    candidate.local_published = true;
    candidate.file_identity = Some(identity(file)?);
    persist(config, candidate)
}

/// The existing immutable-base import format and full nonzero upload path.
/// This seam lets qualification exercise the actual importer without credentials.
pub(super) fn import_base_file(
    bases: Arc<dyn ObjectStore>,
    file: &mut File,
    image: &str,
    size: u64,
    bytes: &mut [u8],
    deadline: Option<Instant>,
) -> Result<crate::BaseRef> {
    use sha2::{Digest, Sha256};
    let mut disk = match IndexedVolume::open(bases.clone(), image) {
        Ok(disk) => {
            if disk.size() != size {
                return Err("base image size mismatch".into());
            }
            return Ok(disk.export_base()?);
        }
        Err(crate::Error::NotFound) => IndexedVolume::create_import(bases.clone(), image, size)?,
        Err(crate::Error::NotReady) => IndexedVolume::resume_import(bases.clone(), image, size)?,
        Err(error) => return Err(error.into()),
    };
    file.seek(SeekFrom::Start(0))?;
    let mut check = Sha256::new();
    let mut at = 0;
    while at < size {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err("base import deadline".into());
        }
        let n = bytes.len().min((size - at) as usize);
        file.read_exact(&mut bytes[..n])?;
        check.update(&bytes[..n]);
        disk.write(at, &bytes[..n])?;
        disk.commit_import()?;
        at += n as u64;
    }
    if format!("{:x}", check.finalize()) != image {
        return Err("base image changed during import".into());
    }
    disk.finish_import()?;
    Ok(disk.export_base()?)
}

fn capture_bytes(
    config: &Config,
    candidate: &mut Candidate,
    raw: Arc<dyn ObjectStore>,
    mut import: impl FnMut(&mut File, &str, u64, &mut [u8], Instant) -> Result<crate::BaseRef>,
) -> Result<ProjectImageInfo> {
    let deadline = Instant::now() + MAX_CAPTURE_TIME;
    if candidate.phase == Phase::Ready {
        return info(candidate);
    }
    if matches!(
        candidate.phase,
        Phase::Failed | Phase::Retiring | Phase::Retired
    ) {
        return Err("project image was retired".into());
    }
    let head = raw
        .head(&candidate.source_volume_id)?
        .ok_or("source volume unavailable")?;
    if head.revision != candidate.source_revision
        || crate::digest(&head.manifest) != candidate.source_manifest_sha256
    {
        return Err("project image source generation changed".into());
    }
    let mut file = if candidate.digest.is_some() && candidate.path()?.exists() {
        let path = candidate.path()?;
        // Resume a crash between create-only hardlink publication and temp unlink.
        if candidate.temporary().exists() {
            let a = fs::symlink_metadata(&path)?;
            let b = fs::symlink_metadata(candidate.temporary())?;
            if !a.is_file() || !b.is_file() || a.dev() != b.dev() || a.ino() != b.ino() {
                return Err("project image publication collision".into());
            }
            fs::remove_file(candidate.temporary())?;
            File::open(&candidate.output_root)?.sync_all()?;
        }
        open_file(&path, false, false)?
    } else {
        check_headroom(config, candidate.logical_bytes, &candidate.image_id)?;
        let mut file = open_file(&candidate.temporary(), true, true)?;
        if candidate.digest.is_none() {
            // An interrupted append/export is rebuilt only from the SAME frozen
            // remote generation. Never hash a partial file as a ready artifact.
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            let report = crate::owned::export_remote(
                raw.clone(),
                &candidate.source_volume_id,
                &mut SparseExport {
                    file: &mut file,
                    deadline,
                    written: 0,
                    limit: candidate.source_logical_bytes,
                },
            )?;
            if report.logical_bytes != candidate.source_logical_bytes
                || candidate
                    .export_sha256
                    .as_ref()
                    .is_some_and(|s| s != &report.sha256)
            {
                return Err("project image export changed".into());
            }
            file.set_len(report.logical_bytes)?;
            file.sync_all()?;
            let head = raw
                .head(&candidate.source_volume_id)?
                .ok_or("source volume unavailable")?;
            if head.revision != candidate.source_revision
                || crate::digest(&head.manifest) != candidate.source_manifest_sha256
            {
                return Err("source generation changed after export".into());
            }
            candidate.export_sha256 = Some(report.sha256.clone());
            persist(config, candidate)?;
            let verified = crate::project_image::finalize_export(
                &mut file,
                report.logical_bytes,
                &report.sha256,
                &candidate.metadata,
            )?;
            if verified.logical_bytes != candidate.logical_bytes {
                return Err("project image sizing changed".into());
            }
            candidate.digest = Some(verified.digest);
            candidate.phase = Phase::Importing;
            // Ownership of the immutable SHA is durable before publication/upload.
            persist(config, candidate)?;
        } else if let Some(digest) = candidate.digest.as_ref() {
            if file.metadata()?.len() != candidate.logical_bytes
                || crate::project_image::hash_file(&mut file)? != *digest
                || crate::project_image::read_metadata(&mut file)? != candidate.metadata
            {
                return Err("interrupted project image changed".into());
            }
        }
        publish_file(config, candidate, &file)?;
        file
    };
    if file.metadata()?.len() != candidate.logical_bytes
        || crate::project_image::read_metadata(&mut file)? != candidate.metadata
        || crate::project_image::hash_file(&mut file)?
            != *candidate.digest.as_ref().ok_or("missing digest")?
    {
        return Err("published project image changed".into());
    }
    if !candidate.local_published {
        candidate.local_published = true;
        candidate.file_identity = Some(identity(&file)?);
        persist(config, candidate)?;
    }
    if Instant::now() >= deadline {
        return Err("project image capture deadline".into());
    }
    let digest = candidate.digest.clone().ok_or("missing image digest")?;
    if records(config)?
        .iter()
        .any(|r| r.image_id != candidate.image_id && r.digest.as_deref() == Some(&digest))
    {
        return Err("private image digest ownership collision".into());
    }
    let base = import(
        &mut file,
        &digest,
        candidate.logical_bytes,
        &mut vec![0; crate::indexed::WRITE_LIMIT],
        deadline,
    )?;
    if base.image != digest {
        return Err("project image import identity mismatch".into());
    }
    candidate.base = Some(base);
    candidate.file_identity = Some(identity(&file)?);
    // Unlock explicitly before Ready even if an unrelated launch inherited the
    // descriptor between fork and exec. Error exits also unlock through Drop.
    file.release()?;
    candidate.phase = Phase::Ready;
    persist(config, candidate)?;
    info(candidate)
}

fn validate_builder_record(
    r: &Record,
    metadata: &ProjectImageMetadata,
    source_bytes: u64,
) -> Result<()> {
    if r.deleted
        || r.reclaimed
        || !r.prepared
        || r.desired
        || r.worker.is_some()
        || r.client.is_some()
        || r.vm.is_some()
        || r.logical_bytes != source_bytes
        || r.image_hash != metadata.base_content_digest
    {
        return Err("project image capture requires its stopped builder volume".into());
    }
    Ok(())
}

impl Service {
    fn capture_store(&self, record: &Record) -> Result<Arc<dyn ObjectStore>> {
        self.capture_store_with(
            record,
            Arc::new(S3Store::with_timeout(
                S3Config::from_file(&self.config.credentials)?,
                Duration::from_secs(3),
            )?),
        )
    }

    fn capture_store_with(
        &self,
        record: &Record,
        raw: Arc<dyn ObjectStore>,
    ) -> Result<Arc<dyn ObjectStore>> {
        let remote = || {
            CachedStore::new(raw.clone(), CACHE_BYTES as usize)?
                .with_metadata_cache(&self.config.root.join("base-metadata"))
        };
        let cache = remote()?;
        if self.config.local_base_reads {
            // Only the stopped volume's persisted base is a cache candidate.
            // NOFOLLOW/NONBLOCK prevents a replaced leaf redirecting or hanging
            // export. CachedStore verifies every inherited block against the
            // immutable remote map; private changes still come from the store.
            if let Ok(file) = fs::OpenOptions::new()
                .read(true)
                .custom_flags(
                    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
                )
                .open(&record.image)
            {
                if let Ok(cache) = cache.with_local_base(record.image_hash.clone(), file) {
                    return Ok(Arc::new(cache));
                }
                // An absent/untrusted cache or metadata race cannot change the
                // source. Rebuild the unchanged remote cache after attachment
                // consumes its candidate.
                return Ok(Arc::new(remote()?));
            }
        }
        Ok(Arc::new(cache))
    }

    fn capture_project_image_with(
        &self,
        candidate: &mut Candidate,
        raw: Arc<dyn ObjectStore>,
        import: impl FnMut(&mut File, &str, u64, &mut [u8], Instant) -> Result<crate::BaseRef>,
    ) -> Result<ProjectImageInfo> {
        // One private operation is already bounded by project_images and its
        // per-intent lock. Importing path/digest barriers prevent ordinary
        // importers from touching this build-unique SHA before it becomes Ready.
        capture_bytes(&self.config, candidate, raw, import)
    }

    pub(super) fn project_image_request(&self, q: Request) -> Result<serde_json::Value> {
        if q.operation == "project-image-capabilities" {
            return self.project_image_request_inner(q);
        }
        let request = q
            .project_image
            .as_ref()
            .ok_or("missing project image request")?;
        if !project_uuid(&request.image_id) {
            return Err("invalid project image ID".into());
        }
        if q.operation == "project-image-status" {
            return self.project_image_status(&q, request);
        }
        let _intent = match intent_lock(&self.config, &request.image_id) {
            Ok(lock) => lock,
            Err(_) => return Ok(proof(&q, "capturing", false, None, None, false)),
        };
        if let Some(mut failure) = failed_intent(&self.config, &q, &request.image_id)? {
            if q.operation == "project-image-retire"
                && !candidate_path(&self.config, &request.image_id).exists()
            {
                failure.retired = true;
                save(&intent_path(&self.config, &request.image_id), &failure)?;
                let mut reply = proof(&q, "retired", true, None, None, true);
                reply["logical_released"] = true.into();
                reply["reclamation_complete"] = true.into();
                return Ok(reply);
            }
            if q.operation == "project-image-capture" {
                if !failure.retired
                    && failure.request_sha256 != crate::digest(&serde_json::to_vec(request)?)
                {
                    return Err("sealed project image intent changed".into());
                }
                return Ok(proof(
                    &q,
                    if failure.retired { "retired" } else { "failed" },
                    true,
                    Some(&failure.error_code),
                    None,
                    false,
                ));
            }
        }
        if q.operation == "project-image-retire"
            && !candidate_path(&self.config, &request.image_id).exists()
        {
            if q.version != 1 || q.image.is_some() || q.logical_bytes.is_some() {
                return Err("invalid project image retirement".into());
            }
            // Preflight may fail before any volume capture, or before a builder
            // volume exists. The intent lock plus durable retired seal prevents
            // any delayed capture from starting after this cleanup proof.
            private_dir(&intent_root(&self.config))?;
            save(
                &intent_path(&self.config, &request.image_id),
                &FailedIntent {
                    schema: 1,
                    image_id: request.image_id.clone(),
                    volume_scope: crate::digest(q.volume_id.as_bytes()),
                    sandbox_scope: crate::digest(q.sandbox_dir.as_os_str().as_encoded_bytes()),
                    request_sha256: crate::digest(&serde_json::to_vec(request)?),
                    error_code: "project_image_capture_failed".into(),
                    retired: true,
                },
            )?;
            let mut reply = proof(&q, "retired", true, None, None, true);
            reply["logical_released"] = true.into();
            reply["reclamation_complete"] = true.into();
            return Ok(reply);
        }
        if q.operation == "project-image-capture"
            && candidate_path(&self.config, &request.image_id).exists()
        {
            let candidate = load(&self.config, &request.image_id)?;
            candidate.matching(&q, request)?;
            if matches!(
                candidate.phase,
                Phase::Failed | Phase::Retiring | Phase::Retired
            ) {
                return Ok(proof(
                    &q,
                    if candidate.phase == Phase::Retired {
                        "retired"
                    } else {
                        "failed"
                    },
                    true,
                    candidate.error_code.as_deref(),
                    None,
                    false,
                ));
            }
        }
        let _images = match self.project_images.try_lock() {
            Ok(lock) => lock,
            Err(std::sync::TryLockError::WouldBlock) => {
                return Ok(proof(&q, "capturing", false, None, None, false))
            }
            Err(error) => {
                if q.operation == "project-image-capture" {
                    seal_failed_intent(&self.config, &q, request, "project_image_capture_failed")?;
                    return Ok(proof(
                        &q,
                        "failed",
                        true,
                        Some("project_image_capture_failed"),
                        None,
                        false,
                    ));
                }
                return Err(format!("project image operation busy: {error}").into());
            }
        };
        let result = self.project_image_request_inner(q.clone());
        match result {
            Ok(mut reply) => {
                reply["project_image_phase"] = if q.operation == "project-image-capture" {
                    "ready"
                } else if reply["reclamation_complete"] == true {
                    "retired"
                } else {
                    "capturing"
                }
                .into();
                reply["project_image_quiescent"] = true.into();
                reply["project_image_error_code"] = serde_json::Value::Null;
                if q.operation == "project-image-retire" && reply["reclamation_complete"] == true {
                    if let Some(mut failure) = failed_intent(&self.config, &q, &request.image_id)? {
                        failure.retired = true;
                        save(&intent_path(&self.config, &request.image_id), &failure)?;
                    }
                }
                Ok(reply)
            }
            Err(error) if q.operation == "project-image-capture" => {
                eprintln!("project image {} capture: {error}", request.image_id);
                // All source/file/import/owner scopes in the inner worker ended
                // before sealing. The intent lock prevents a late duplicate.
                if candidate_path(&self.config, &request.image_id).exists() {
                    let mut candidate = load(&self.config, &request.image_id)?;
                    candidate.matching(&q, request)?;
                    candidate.phase = Phase::Failed;
                    candidate.error_code = Some("project_image_capture_failed".into());
                    persist(&self.config, &candidate)?;
                }
                seal_failed_intent(&self.config, &q, request, "project_image_capture_failed")?;
                Ok(proof(
                    &q,
                    "failed",
                    true,
                    Some("project_image_capture_failed"),
                    None,
                    false,
                ))
            }
            Err(error) => Err(error),
        }
    }

    fn project_image_status(
        &self,
        q: &Request,
        request: &ProjectImageRequest,
    ) -> Result<serde_json::Value> {
        if q.version != 1 || q.image.is_some() || q.logical_bytes.is_some() {
            return Err("invalid project image status request".into());
        }
        let lock = intent_lock(&self.config, &request.image_id).ok();
        if let Some(failure) = failed_intent(&self.config, q, &request.image_id)? {
            let mut reply = proof(
                q,
                if failure.retired { "retired" } else { "failed" },
                lock.is_some(),
                Some(&failure.error_code),
                None,
                true,
            );
            if candidate_path(&self.config, &request.image_id).exists() {
                let candidate = load(&self.config, &request.image_id)?;
                candidate.matching(q, request)?;
                reply["logical_released"] = candidate.logical_released.into();
                reply["reclamation_complete"] = candidate.reclamation_complete.into();
            } else {
                reply["logical_released"] = failure.retired.into();
                reply["reclamation_complete"] = failure.retired.into();
            }
            return Ok(reply);
        }
        if !candidate_path(&self.config, &request.image_id).exists() {
            return Ok(proof(q, "absent", false, None, None, true));
        }
        let mut candidate = load(&self.config, &request.image_id)?;
        candidate.matching(q, request)?;
        if !valid_id(&q.volume_id) {
            return Err("invalid source volume".into());
        }
        let source: Record = read(
            &self
                .config
                .root
                .join("volumes")
                .join(&q.volume_id)
                .join("record.json"),
        )?;
        if source.sandbox != q.sandbox_dir {
            return Err("source sandbox binding mismatch".into());
        }
        if matches!(candidate.phase, Phase::Exporting | Phase::Importing) && lock.is_some() {
            // A live node retry cannot obtain this intent lock. A supervisor
            // restart can: no old writer scopes survive, so seal the failure.
            candidate.phase = Phase::Failed;
            candidate.error_code = Some("project_image_capture_interrupted".into());
            persist(&self.config, &candidate)?;
            // A status request has no complete capture payload; the candidate's
            // Failed phase itself is the immutable seal for all later duplicates.
        }
        let phase = match candidate.phase {
            Phase::Ready => "ready",
            Phase::Failed => "failed",
            Phase::Retired => "retired",
            _ => "capturing",
        };
        let mut reply = proof(
            q,
            phase,
            lock.is_some(),
            candidate.error_code.as_deref(),
            if candidate.phase == Phase::Ready {
                Some(info(&candidate)?)
            } else {
                None
            },
            true,
        );
        reply["logical_released"] = candidate.logical_released.into();
        reply["reclamation_complete"] = candidate.reclamation_complete.into();
        Ok(reply)
    }

    fn project_image_request_inner(&self, q: Request) -> Result<serde_json::Value> {
        if q.version != 1 || q.image.is_some() || q.logical_bytes.is_some() {
            return Err("invalid project image request".into());
        }
        if q.operation == "project-image-capabilities" {
            if q.volume_id != "0".repeat(64)
                || q.sandbox_dir != self.config.engine_root
                || q.project_image.is_some()
            {
                return Err("invalid project image capability probe".into());
            }
            let capacity = available(&self.config).and_then(|available| Ok(serde_json::json!({"physical_available_bytes":available,"physical_reserved_bytes":pending_bytes(&self.config, None)?})));
            return Ok(
                serde_json::json!({"ok":true,"volume_id":q.volume_id,"project_images_available":capacity.is_ok(),"project_image_capacity":capacity.ok()}),
            );
        }
        configured_root(&self.config)?;
        if !valid_id(&q.volume_id) {
            return Err("invalid source volume".into());
        }
        let request = q
            .project_image
            .as_ref()
            .ok_or("missing project image request")?;
        if !project_uuid(&request.image_id)
            || request
                .expected_digest
                .as_deref()
                .is_some_and(|s| !project_digest(s))
        {
            return Err("invalid project image identity".into());
        }
        let source = self
            .entries
            .lock()
            .map_err(|_| "volume registry poisoned")?
            .get(&q.volume_id)
            .cloned()
            .ok_or("unknown source volume")?;
        let source_binding: Record = read(
            &self
                .config
                .root
                .join("volumes")
                .join(&q.volume_id)
                .join("record.json"),
        )?;
        if source_binding.sandbox != q.sandbox_dir {
            return Err("source sandbox binding mismatch".into());
        }
        if q.operation == "project-image-status" {
            let candidate = load(&self.config, &request.image_id)?;
            candidate.matching(&q, request)?;
            return Ok(
                serde_json::json!({"ok":true,"volume_id":q.volume_id,"project_image":if candidate.phase == Phase::Ready {Some(info(&candidate)?)} else {None},"logical_released":candidate.logical_released,"reclamation_complete":candidate.reclamation_complete}),
            );
        }
        match q.operation.as_str() {
            "project-image-capture" => {
                let metadata = request
                    .metadata
                    .as_ref()
                    .ok_or("missing frozen project image provenance")?;
                metadata.validate()?;
                if request.image_id != metadata.build_id {
                    return Err("project image ID must equal build ID".into());
                }
                let source_bytes = request
                    .source_logical_bytes
                    .ok_or("missing source image size")?;
                let image_bytes = crate::project_image::logical_bytes(source_bytes)?;
                if image_bytes > self.config.limits.max_volume_bytes {
                    return Err("project image logical size limit".into());
                }
                if candidate_path(&self.config, &request.image_id).exists() {
                    let candidate = load(&self.config, &request.image_id)?;
                    candidate.matching(&q, request)?;
                    if candidate.phase == Phase::Ready {
                        return Ok(
                            serde_json::json!({"ok":true,"volume_id":q.volume_id,"project_image":info(&candidate)?}),
                        );
                    }
                }
                let entry = source.foreground(true)?;
                let r = &entry.record;
                validate_builder_record(r, metadata, source_bytes)?;
                if self.vm(r)?.is_some() {
                    return Err("project image capture requires its stopped builder volume".into());
                }
                self.binding(&r.id, &r.sandbox)?;
                if r.sandbox.join("state.json").exists()
                    && ahvm_engine::Worker::load(r.sandbox.join("state.json"))?.verified_alive()?
                {
                    return Err("builder worker is alive".into());
                }
                if !r.evicted {
                    if nbd_pid(&r.device)?.is_some() {
                        return Err("builder NBD is attached".into());
                    }
                    unused(&r.device)?;
                }
                let raw = self.capture_store(r)?;
                let owner = self.dir(r).join("owner");
                private_dir(&owner)?;
                let disk = OwnedDisk::open(raw.clone(), &r.id, &owner)?;
                let status = disk.status()?;
                if status.local_failed
                    || status.replication_failed
                    || status.pending_bytes != 0
                    || status.local_sequence != status.remote_sequence
                {
                    return Err("builder volume is not fully synchronized".into());
                }
                let mut candidate = if candidate_path(&self.config, &request.image_id).exists() {
                    load(&self.config, &request.image_id)?
                } else {
                    let head = raw.head(&r.id)?.ok_or("source volume unavailable")?;
                    let map = self
                        .entries
                        .lock()
                        .map_err(|_| "volume registry poisoned")?;
                    let mut used = logical_charge(&self.config)?;
                    for id in map.keys() {
                        let record: Record = read(
                            &self
                                .config
                                .root
                                .join("volumes")
                                .join(id)
                                .join("record.json"),
                        )?;
                        if !record.reclaimed && !record.logical_released {
                            used = used
                                .checked_add(record.logical_bytes)
                                .ok_or("image admission overflow")?;
                        }
                    }
                    if used
                        .checked_add(image_bytes)
                        .is_none_or(|n| n > self.config.limits.max_logical_bytes)
                    {
                        return Err("host logical capacity reservation exhausted".into());
                    }
                    check_headroom(&self.config, image_bytes, &request.image_id)?;
                    let candidate = Candidate {
                        schema: 1,
                        image_id: request.image_id.clone(),
                        source_volume_id: r.id.clone(),
                        sandbox_dir: r.sandbox.clone(),
                        output_root: configured_root(&self.config)?.into(),
                        source_logical_bytes: source_bytes,
                        logical_bytes: image_bytes,
                        metadata: metadata.clone(),
                        source_revision: head.revision,
                        source_manifest_sha256: crate::digest(&head.manifest),
                        export_sha256: None,
                        digest: None,
                        base: None,
                        file_identity: None,
                        local_published: false,
                        phase: Phase::Exporting,
                        error_code: None,
                        remote_retired: false,
                        retirement_verified: false,
                        cleanup_passes: 0,
                        remote_clean: false,
                        logical_released: false,
                        reclamation_complete: false,
                    };
                    persist(&self.config, &candidate)?;
                    drop(map);
                    candidate
                };
                candidate.matching(&q, request)?;
                let captured = self.capture_project_image_with(
                    &mut candidate,
                    raw,
                    |file, digest, size, bytes, deadline| {
                        self.import_base_bounded(file, digest, size, bytes, Some(deadline))
                    },
                )?;
                drop(disk);
                Ok(serde_json::json!({"ok":true,"volume_id":q.volume_id,"project_image":captured}))
            }
            "project-image-retire" => {
                let mut candidate = load(&self.config, &request.image_id)?;
                candidate.matching(&q, request)?;
                let retirement = self.retire_project_image(&mut candidate)?;
                Ok(
                    serde_json::json!({"ok":true,"volume_id":q.volume_id,"logical_released":retirement.logical_released,"reclamation_complete":retirement.reclamation_complete}),
                )
            }
            _ => Err("unknown project image operation".into()),
        }
    }

    fn retire_project_image(&self, candidate: &mut Candidate) -> Result<ProjectImageRetirement> {
        self.retire_project_image_with(candidate, || {
            Ok(Arc::new(
                S3Store::with_timeout(
                    S3Config::from_file(&self.config.credentials)?,
                    Duration::from_secs(3),
                )?
                .bases()?,
            ))
        })
    }
    fn retire_project_image_with(
        &self,
        candidate: &mut Candidate,
        store: impl FnOnce() -> Result<Arc<dyn ObjectStore>>,
    ) -> Result<ProjectImageRetirement> {
        if candidate.phase == Phase::Retired {
            return Ok(candidate.retirement());
        }
        // Lock order: project operation -> shared imports -> entries/admission.
        // Wait for existing prepare/prewarm WITHOUT holding entries, then
        // atomically check references and seal Retiring. Alias prewarm has no
        // volume record, so this short import fence must precede the barrier.
        // No Slot is acquired here; slow verification/GC holds neither lock.
        let imports = self
            .imports
            .lock()
            .map_err(|_| "image import lock poisoned")?;
        let entries = self
            .entries
            .lock()
            .map_err(|_| "volume registry poisoned")?;
        ensure_unreferenced(&self.config, candidate, &entries)?;
        if candidate.phase != Phase::Retiring {
            candidate.phase = Phase::Retiring;
            persist(&self.config, candidate)?;
        }
        drop(entries);
        drop(imports);
        if !candidate.retirement_verified {
            if let Some(digest) = candidate.digest.as_ref() {
                let path = if candidate.path()?.exists() {
                    candidate.path()?
                } else {
                    candidate.temporary()
                };
                let mut file = open_file(&path, false, false)?;
                if file.metadata()?.len() != candidate.logical_bytes
                    || crate::project_image::hash_file(&mut file)? != *digest
                    || crate::project_image::read_metadata(&mut file)? != candidate.metadata
                {
                    return Err("project image ownership proof failed".into());
                }
            }
            candidate.retirement_verified = true;
            persist(&self.config, candidate)?;
        }
        if let Some(digest) = candidate.digest.clone() {
            retire_remote(&self.config, candidate, store()?, digest)?;
        } else {
            candidate.remote_retired = true;
            candidate.remote_clean = true;
            persist(&self.config, candidate)?;
        }
        if candidate.remote_clean {
            for path in [
                Some(candidate.temporary()),
                candidate
                    .digest
                    .as_ref()
                    .map(|_| candidate.path())
                    .transpose()?,
            ]
            .into_iter()
            .flatten()
            {
                match fs::remove_file(&path) {
                    Ok(()) => (),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
            }
            File::open(&candidate.output_root)?.sync_all()?;
            candidate.phase = Phase::Retired;
            candidate.logical_released = true;
            candidate.reclamation_complete = true;
            persist(&self.config, candidate)?;
        }
        Ok(candidate.retirement())
    }
}

fn ensure_unreferenced(
    config: &Config,
    candidate: &Candidate,
    entries: &BTreeMap<String, Arc<Slot>>,
) -> Result<()> {
    for id in entries.keys() {
        let record: Record = read(&config.root.join("volumes").join(id).join("record.json"))?;
        if !record.reclaimed
            && (candidate
                .digest
                .as_ref()
                .is_some_and(|d| record.image_hash == *d)
                || candidate.digest.is_some() && record.image == candidate.path()?)
        {
            return Err("project image still referenced by a volume".into());
        }
    }
    Ok(())
}
fn retire_remote(
    config: &Config,
    candidate: &mut Candidate,
    store: Arc<dyn ObjectStore>,
    digest: String,
) -> Result<()> {
    if !candidate.remote_retired {
        if let Some(head) = store.head(&digest)? {
            if !crate::reclaim::retired(&digest, &head)? {
                // The existing indexed validator accepts its supported unowned
                // formats and rejects owned envelopes/unknown fields. Ready
                // imports currently serialize as format 8, not only format 2.
                let indexed = match IndexedVolume::open(store.clone(), &digest) {
                    Ok(disk) => disk,
                    Err(crate::Error::NotReady) => IndexedVolume::resume_import(
                        store.clone(),
                        &digest,
                        candidate.logical_bytes,
                    )?,
                    Err(error) => return Err(error.into()),
                };
                if indexed.size() != candidate.logical_bytes {
                    return Err("private base size changed".into());
                }
            }
        }
        // Private image imports are unowned indexed bases. This never acquires
        // or retires a live volume or an operator-selected base namespace.
        OwnedDisk::retire(store.clone(), &digest, &registry(config))?;
        candidate.remote_retired = true;
        persist(config, candidate)?;
    }
    if !candidate.remote_clean {
        let pass = crate::reclaim::sweep(store.as_ref(), &digest, 1000)?;
        candidate.cleanup_passes = candidate
            .cleanup_passes
            .checked_add(1)
            .ok_or("cleanup checkpoint overflow")?;
        candidate.remote_clean = pass.complete;
        persist(config, candidate)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
