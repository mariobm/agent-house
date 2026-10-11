//! One bounded background checksum job. Operator image updates invalidate the
//! proof through both open-file and pathname identities; no disk bytes are read
//! on a cached profile request.
use super::{
    project_digest, ProjectImageBaseProof as Proof, ProjectImageBaseStatus as Status,
    PROJECT_IMAGE_MAX_BYTES, PROJECT_IMAGE_TRAILER_BYTES,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata},
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    bytes: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
    mode: u32,
    uid: u32,
    gid: u32,
    links: u64,
}
impl Identity {
    fn read(m: &Metadata) -> Self {
        Self {
            dev: m.dev(),
            ino: m.ino(),
            bytes: m.len(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            links: m.nlink(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    path: PathBuf,
    artifact_digest: String,
    identity: Identity,
}
fn source(path: &Path, artifact_digest: &str) -> Option<(File, Key)> {
    if !project_digest(artifact_digest) || path.canonicalize().ok().as_deref() != Some(path) {
        return None;
    }
    let before = fs::symlink_metadata(path).ok()?;
    if !before.is_file()
        || before.mode() & 0o022 != 0
        || before.len() == 0
        || before.len().checked_add(PROJECT_IMAGE_TRAILER_BYTES)? > PROJECT_IMAGE_MAX_BYTES
        || !before.len().is_multiple_of(65536)
    {
        return None;
    }
    let file = File::open(path).ok()?;
    let identity = Identity::read(&before);
    if Identity::read(&file.metadata().ok()?) != identity {
        return None;
    }
    Some((
        file,
        Key {
            path: path.to_path_buf(),
            artifact_digest: artifact_digest.into(),
            identity,
        },
    ))
}
fn hash(mut file: File, key: &Key) -> Proof {
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut buffer = vec![0; 8 * 1024 * 1024];
    let mut h = Sha256::new();
    let mut bytes = 0u64;
    loop {
        if Instant::now() >= deadline {
            return Proof::unavailable(Status::Invalid);
        }
        let Ok(n) = file.read(&mut buffer) else {
            return Proof::unavailable(Status::Invalid);
        };
        if n == 0 {
            break;
        }
        bytes = bytes.saturating_add(n as u64);
        if bytes > key.identity.bytes {
            return Proof::unavailable(Status::Invalid);
        }
        h.update(&buffer[..n]);
    }
    let Ok(after) = file.metadata() else {
        return Proof::unavailable(Status::Invalid);
    };
    let Ok(path) = fs::symlink_metadata(&key.path) else {
        return Proof::unavailable(Status::Invalid);
    };
    if bytes != key.identity.bytes
        || Identity::read(&after) != key.identity
        || Identity::read(&path) != key.identity
    {
        Proof::unavailable(Status::Invalid)
    } else {
        Proof {
            status: Status::Verified,
            content_digest: Some(format!("{:x}", h.finalize())),
        }
    }
}
#[derive(Default)]
struct Cache {
    running: bool,
    jobs: u64,
    entries: Vec<(Key, Proof, Option<Instant>)>,
}
#[derive(Default)]
struct Verifier {
    cache: Arc<Mutex<Cache>>,
}
impl Verifier {
    fn status(&self, path: &Path, artifact_digest: &str) -> Proof {
        self.status_with(path, artifact_digest, hash)
    }
    fn status_with(
        &self,
        path: &Path,
        artifact_digest: &str,
        compute: impl FnOnce(File, &Key) -> Proof + Send + 'static,
    ) -> Proof {
        let Some((file, key)) = source(path, artifact_digest) else {
            return Proof::unavailable(Status::Invalid);
        };
        let Ok(mut cache) = self.cache.lock() else {
            return Proof::unavailable(Status::Invalid);
        };
        if let Some(index) = cache.entries.iter().position(|(old, _, _)| old == &key) {
            let (_, proof, retry) = &cache.entries[index];
            if retry.is_none_or(|at| Instant::now() < at) {
                return proof.clone();
            }
            cache.entries.remove(index);
        }
        if cache.running {
            return Proof::unavailable(Status::Verifying);
        }
        cache.running = true;
        cache.jobs = cache.jobs.saturating_add(1);
        let state = self.cache.clone();
        let saved = key.clone();
        if std::thread::Builder::new()
            .name("project-base-sha".into())
            .spawn(move || {
                let status = compute(file, &saved);
                if let Ok(mut cache) = state.lock() {
                    cache.running = false;
                    if cache.entries.len() >= 4 {
                        cache.entries.remove(0);
                    }
                    let (status, retry) = if status.status == Status::Invalid {
                        (
                            Proof::unavailable(Status::Verifying),
                            Some(Instant::now() + Duration::from_secs(30)),
                        )
                    } else {
                        (status, None)
                    };
                    cache.entries.push((saved, status, retry));
                }
            })
            .is_err()
        {
            cache.running = false;
            return Proof::unavailable(Status::Invalid);
        }
        Proof::unavailable(Status::Verifying)
    }
}
pub(crate) fn status(path: &Path, artifact_digest: &str) -> Proof {
    static PROOF: OnceLock<Verifier> = OnceLock::new();
    PROOF
        .get_or_init(Verifier::default)
        .status(path, artifact_digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
    };
    fn fixture() -> (PathBuf, String) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "project-base-proof-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = vec![7; 65536];
        fs::write(&path, &bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        (
            path.canonicalize().unwrap(),
            format!("{:x}", Sha256::digest(&bytes)),
        )
    }
    fn completed(v: &Verifier, path: &Path, artifact_digest: &str) -> Proof {
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let s = v.status(path, artifact_digest);
            if s.status != Status::Verifying {
                return s;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn artifact_and_content_differ_and_cached_proof_is_fast() {
        let (p, d) = fixture();
        let v = Verifier::default();
        assert_eq!(
            completed(&v, &p, &d).content_digest.as_deref(),
            Some(d.as_str())
        );
        let jobs = v.cache.lock().unwrap().jobs;
        for _ in 0..100 {
            assert_eq!(v.status(&p, &d).status, Status::Verified);
        }
        assert_eq!(v.cache.lock().unwrap().jobs, jobs);
        assert_eq!(
            completed(&v, &p, &"f".repeat(64)).content_digest.as_deref(),
            Some(d.as_str())
        );
        fs::remove_file(p).unwrap();
    }
    #[test]
    fn mutation_and_replacement_invalidate_cached_proof() {
        let (p, d) = fixture();
        let v = Verifier::default();
        assert_eq!(
            completed(&v, &p, &d).content_digest.as_deref(),
            Some(d.as_str())
        );
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&p, vec![8; 65536]).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o444)).unwrap();
        assert_ne!(
            completed(&v, &p, &d).content_digest.as_deref(),
            Some(d.as_str())
        );
        let (other, other_digest) = fixture();
        fs::rename(other, &p).unwrap();
        assert_eq!(
            completed(&v, &p, &other_digest).content_digest.as_deref(),
            Some(other_digest.as_str())
        );
        assert_eq!(v.cache.lock().unwrap().jobs, 3);
        fs::remove_file(p).unwrap();
    }
    #[test]
    fn in_flight_deduplicates_and_rechecks_path_after_hash() {
        let (p, d) = fixture();
        let v = Verifier::default();
        let (go, wait) = mpsc::channel();
        let (entered, ready) = mpsc::channel();
        assert_eq!(
            v.status_with(&p, &d, move |f, k| {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                hash(f, k)
            }),
            Proof::unavailable(Status::Verifying)
        );
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..100 {
            assert_eq!(v.status(&p, &d).status, Status::Verifying);
        }
        assert_eq!(v.cache.lock().unwrap().jobs, 1);
        let (replacement, _) = fixture();
        fs::rename(replacement, &p).unwrap();
        go.send(()).unwrap();
        while v.cache.lock().unwrap().running {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            v.cache.lock().unwrap().entries[0].1.status,
            Status::Verifying
        );
        assert_eq!(
            completed(&v, &p, &d).content_digest.as_deref(),
            Some(d.as_str())
        );
        fs::remove_file(p).unwrap();
    }
    #[test]
    fn transient_read_failure_backs_off_then_retries_without_touching_source() {
        let (path, digest) = fixture();
        let verifier = Verifier::default();
        assert_eq!(
            verifier
                .status_with(&path, &digest, |_, _| Proof::unavailable(Status::Invalid))
                .status,
            Status::Verifying
        );
        while verifier.cache.lock().unwrap().running {
            std::thread::yield_now();
        }
        assert_eq!(verifier.status(&path, &digest).status, Status::Verifying);
        assert_eq!(verifier.cache.lock().unwrap().jobs, 1);
        verifier.cache.lock().unwrap().entries[0].2 = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(
            completed(&verifier, &path, &digest)
                .content_digest
                .as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(verifier.cache.lock().unwrap().jobs, 2);
        fs::remove_file(path).unwrap();
    }
}
