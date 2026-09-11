//! In-memory [`Backend`](crate::Backend) for unit tests.
//!
//! Keeps sandboxes in a `HashMap`, fakes exec by echoing argv, and writes
//! real [`SnapshotManifest`](crate::SnapshotManifest) files so snapshot
//! round trips exercise the same manifest IO the real backend will use.
//! `restore` re-checks the manifest compat gate, so incompatible bundles
//! fail here exactly as they would against a real worker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::snapshot::{unix_now, Artifacts, Compat, VmmId, DEVICE_LAYOUT_VER};
use crate::{
    host_caps, Backend, BackendKind, Capabilities, DirEntry, DirListing, Error, ExecResult,
    FileChunk, Result, SandboxInfo, SandboxSpec, SessionChunk, SessionInfo, SnapshotManifest,
    State, Thermal,
};
use std::time::Duration;

/// Kernel digest stamped on mock snapshots. Tests that need a matching
/// host build it with [`host_caps`] + this digest + the crate version.
pub const MOCK_KERNEL_DIGEST: &str = "mock-kernel-dev";
/// Captured-output cap mirrored into [`ExecResult::truncated`].
pub const MAX_EXEC_OUTPUT: usize = 256 * 1024;

#[derive(Debug)]
struct MockSession {
    argv: Vec<String>,
    output: Vec<u8>,
    running: bool,
}

#[derive(Debug)]
struct Inner {
    next: u64,
    sandboxes: HashMap<String, (SandboxSpec, SandboxInfo)>,
    files: HashMap<(String, String), Vec<u8>>,
    sessions: HashMap<(String, String), MockSession>,
    session_next: u64,
}

/// In-memory backend. Create with [`MockBackend::new`] pointing at a
/// scratch dir for snapshot manifests.
#[derive(Debug)]
pub struct MockBackend {
    inner: Mutex<Inner>,
    snapshot_dir: PathBuf,
}

impl MockBackend {
    pub fn new(snapshot_dir: impl AsRef<Path>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                next: 0,
                sandboxes: HashMap::new(),
                files: HashMap::new(),
                sessions: HashMap::new(),
                session_next: 0,
            }),
            snapshot_dir: snapshot_dir.as_ref().to_path_buf(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("mock mutex poisoned")
    }

    fn mock_host() -> crate::HostCaps {
        host_caps("krucible", env!("CARGO_PKG_VERSION"), MOCK_KERNEL_DIGEST)
    }

    fn manifest_for(id: &str, snapshot_id: &str, spec: &SandboxSpec) -> SnapshotManifest {
        let _ = id;
        SnapshotManifest {
            manifest_ver: crate::MANIFEST_VER,
            snapshot_id: snapshot_id.to_string(),
            created_at: unix_now(),
            compat: Compat {
                arch: std::env::consts::ARCH.to_string(),
                vmm: VmmId {
                    name: "krucible".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                kernel_digest: MOCK_KERNEL_DIGEST.to_string(),
                mem_mib: spec.memory_mb,
                vcpus: spec.cpus,
                device_layout_ver: DEVICE_LAYOUT_VER,
            },
            artifacts: Artifacts {
                memory_bytes: u64::from(spec.memory_mb) * 1024 * 1024,
                root_delta_bytes: 0,
            },
        }
    }

    /// The sandbox must exist and be running for any guest-touching op.
    fn live<'a>(
        sandboxes: &'a HashMap<String, (SandboxSpec, SandboxInfo)>,
        id: &str,
    ) -> Result<&'a SandboxInfo> {
        let (_, info) = sandboxes
            .get(id)
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
        if info.state != State::Running {
            return Err(Error::InvalidState(format!(
                "sandbox {id} is not running (state {:?})",
                info.state
            )));
        }
        Ok(info)
    }
}

impl Backend for MockBackend {
    fn create(&self, spec: &SandboxSpec) -> Result<SandboxInfo> {
        let mut inner = self.lock();
        inner.next += 1;
        let n = inner.next;
        let id = format!("mock-{n:04}");
        let info = SandboxInfo {
            id: id.clone(),
            name: spec.name.clone(),
            state: State::Running,
            thermal: Thermal::Hot,
            ip: format!("10.42.0.{}", (n % 250) + 2),
        };
        inner.sandboxes.insert(id, (spec.clone(), info.clone()));
        Ok(info)
    }

    fn destroy(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        inner
            .sandboxes
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))
    }

    fn start(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        let (_, info) = inner
            .sandboxes
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
        info.state = State::Running;
        info.thermal = Thermal::Hot;
        Ok(())
    }

    fn stop(&self, id: &str) -> Result<()> {
        let mut inner = self.lock();
        let (_, info) = inner
            .sandboxes
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
        info.state = State::Stopped;
        info.thermal = Thermal::Cold;
        Ok(())
    }

    fn status(&self, id: &str) -> Result<SandboxInfo> {
        let inner = self.lock();
        inner
            .sandboxes
            .get(id)
            .map(|(_, info)| info.clone())
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))
    }

    fn list(&self) -> Result<Vec<SandboxInfo>> {
        let inner = self.lock();
        let mut infos: Vec<SandboxInfo> =
            inner.sandboxes.values().map(|(_, i)| i.clone()).collect();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(infos)
    }

    fn exec(&self, id: &str, argv: &[String]) -> Result<ExecResult> {
        let inner = self.lock();
        let (_, info) = inner
            .sandboxes
            .get(id)
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
        if info.state != State::Running {
            return Err(Error::InvalidState(format!(
                "sandbox {id} is not running (state {:?})",
                info.state
            )));
        }
        let mut stdout = argv.join(" ");
        stdout.push('\n');
        let truncated = stdout.len() > MAX_EXEC_OUTPUT;
        if truncated {
            stdout = stdout.chars().take(MAX_EXEC_OUTPUT).collect();
        }
        Ok(ExecResult {
            exit_code: 0,
            stdout,
            stderr: String::new(),
            truncated,
        })
    }

    fn create_snapshot(&self, id: &str, snapshot_id: &str) -> Result<SnapshotManifest> {
        let inner = self.lock();
        let (spec, _) = inner
            .sandboxes
            .get(id)
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?;
        let manifest = Self::manifest_for(id, snapshot_id, spec);
        manifest.write_to(&self.snapshot_dir.join(snapshot_id))?;
        Ok(manifest)
    }

    fn restore(&self, snapshot: &SnapshotManifest, new_id: &str) -> Result<SandboxInfo> {
        snapshot.check_compat(&Self::mock_host())?;
        let mut inner = self.lock();
        if inner.sandboxes.contains_key(new_id) {
            return Err(Error::Conflict(format!("sandbox {new_id} already exists")));
        }
        let spec = SandboxSpec {
            name: new_id.to_string(),
            cpus: snapshot.compat.vcpus,
            memory_mb: snapshot.compat.mem_mib,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            desktop: false,
            extra_env: HashMap::new(),
        };
        let n = inner.sandboxes.len() as u64 + 2;
        let info = SandboxInfo {
            id: new_id.to_string(),
            name: new_id.to_string(),
            state: State::Running,
            thermal: Thermal::Warm,
            ip: format!("10.42.0.{}", (n % 250) + 2),
        };
        inner
            .sandboxes
            .insert(new_id.to_string(), (spec, info.clone()));
        Ok(info)
    }

    fn fork(&self, id: &str, new_id: &str) -> Result<SandboxInfo> {
        if !self.capabilities().fork {
            return Err(Error::InvalidState(
                "backend does not support fork".to_string(),
            ));
        }
        let mut inner = self.lock();
        let (spec, _) = inner
            .sandboxes
            .get(id)
            .ok_or_else(|| Error::NotFound(format!("sandbox {id}")))?
            .clone();
        if inner.sandboxes.contains_key(new_id) {
            return Err(Error::Conflict(format!("sandbox {new_id} already exists")));
        }
        let n = inner.sandboxes.len() as u64 + 2;
        let info = SandboxInfo {
            id: new_id.to_string(),
            name: new_id.to_string(),
            state: State::Running,
            thermal: Thermal::Hot,
            ip: format!("10.42.0.{}", (n % 250) + 2),
        };
        inner
            .sandboxes
            .insert(new_id.to_string(), (spec, info.clone()));
        Ok(info)
    }

    fn capabilities(&self) -> Capabilities {
        BackendKind::Krucible.capabilities()
    }

    fn snapshot_manifest(&self, snapshot_id: &str) -> Result<SnapshotManifest> {
        SnapshotManifest::read_from(&self.snapshot_dir.join(snapshot_id))
            .map_err(|_| Error::NotFound(format!("snapshot {snapshot_id}")))
    }

    fn file_read(&self, id: &str, path: &str, offset: u64, limit: u64) -> Result<FileChunk> {
        let inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        let data = inner
            .files
            .get(&(id.to_string(), path.to_string()))
            .ok_or_else(|| Error::NotFound(format!("sandbox {id} file {path}")))?;
        let start = (offset as usize).min(data.len());
        let end = start.saturating_add(limit as usize).min(data.len());
        Ok(FileChunk {
            data: data[start..end].to_vec(),
            eof: end >= data.len(),
        })
    }

    fn file_upload(&self, id: &str, path: &str, input: &mut dyn std::io::Read) -> Result<u64> {
        // The in-memory fake stores complete files; production streams to disk.
        let mut data = Vec::new();
        input.read_to_end(&mut data)?;
        self.file_write(id, path, &data)
    }

    fn file_write(&self, id: &str, path: &str, data: &[u8]) -> Result<u64> {
        let mut inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        inner
            .files
            .insert((id.to_string(), path.to_string()), data.to_vec());
        Ok(data.len() as u64)
    }

    fn file_list(&self, id: &str, path: &str, offset: u64, limit: u64) -> Result<DirListing> {
        let inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        // Immediate children of `path` over flat (sandbox, path) keys.
        let prefix = if path == "/" {
            "/".to_string()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        let mut children: HashMap<String, DirEntry> = HashMap::new();
        let mut keys: Vec<(String, String)> = inner.files.keys().cloned().collect();
        keys.sort();
        for (sid, p) in keys {
            if sid != id || !p.starts_with(&prefix) {
                continue;
            }
            let rest = &p[prefix.len()..];
            if rest.is_empty() {
                continue;
            }
            match rest.split_once('/') {
                Some((dir, _)) => {
                    children.entry(dir.to_string()).or_insert(DirEntry {
                        name: dir.to_string(),
                        is_dir: true,
                        size: 0,
                    });
                }
                None => {
                    children.insert(
                        rest.to_string(),
                        DirEntry {
                            name: rest.to_string(),
                            is_dir: false,
                            size: inner.files[&(sid, p)].len() as u64,
                        },
                    );
                }
            }
        }
        let mut entries: Vec<DirEntry> = children.into_values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let start = (offset as usize).min(entries.len());
        let end = start.saturating_add(limit as usize).min(entries.len());
        let next_offset = (end < entries.len()).then_some(end as u64);
        entries.truncate(end);
        Ok(DirListing {
            entries: entries[start..].to_vec(),
            next_offset,
        })
    }

    fn session_create(&self, id: &str, argv: &[String], _pty: bool) -> Result<String> {
        let mut inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        inner.session_next += 1;
        let sid = format!("sess-{:04}", inner.session_next);
        // Echo-style session: output exists from birth, already exited.
        // Enough to exercise create/read/kill/delete/list flows.
        let mut output = argv.join(" ").into_bytes();
        output.push(b'\n');
        inner.sessions.insert(
            (id.to_string(), sid.clone()),
            MockSession {
                argv: argv.to_vec(),
                output,
                running: false,
            },
        );
        Ok(sid)
    }

    fn session_read(
        &self,
        id: &str,
        session_id: &str,
        from_seq: u64,
        _budget: Duration,
    ) -> Result<SessionChunk> {
        let inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        let sess = inner
            .sessions
            .get(&(id.to_string(), session_id.to_string()))
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
        let start = (from_seq as usize).min(sess.output.len());
        Ok(SessionChunk {
            data: sess.output[start..].to_vec(),
            eof: true,
            exit_code: Some(0),
            // No eviction in the mock: the stream total is authoritative.
            next_seq: sess.output.len() as u64,
            truncated: false,
        })
    }

    fn session_input(&self, id: &str, session_id: &str, data: &[u8]) -> Result<u64> {
        let mut inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        let sess = inner
            .sessions
            .get_mut(&(id.to_string(), session_id.to_string()))
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
        if !sess.running {
            return Err(Error::InvalidState(format!(
                "session {session_id} already exited"
            )));
        }
        Ok(data.len() as u64)
    }

    fn session_kill(&self, id: &str, session_id: &str) -> Result<()> {
        let mut inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        let sess = inner
            .sessions
            .get_mut(&(id.to_string(), session_id.to_string()))
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
        if !sess.running {
            // Mirrors forge: killing an exited session is an error, not a
            // silent no-op (callers delete instead).
            return Err(Error::InvalidState(format!(
                "session {session_id} already completed"
            )));
        }
        sess.running = false;
        Ok(())
    }

    fn session_delete(&self, id: &str, session_id: &str) -> Result<()> {
        let mut inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        inner
            .sessions
            .remove(&(id.to_string(), session_id.to_string()))
            .map(|_| ())
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))
    }

    fn session_list(&self, id: &str) -> Result<Vec<SessionInfo>> {
        let inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        let mut out: Vec<SessionInfo> = inner
            .sessions
            .iter()
            .filter(|((sid, _), _)| sid == id)
            .map(|((_, sess_id), s)| SessionInfo {
                id: sess_id.clone(),
                argv: s.argv.clone(),
                running: s.running,
                started_at: 0,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    fn session_resize(&self, id: &str, session_id: &str, _rows: u16, _cols: u16) -> Result<()> {
        let inner = self.lock();
        Self::live(&inner.sandboxes, id)?;
        inner
            .sessions
            .get(&(id.to_string(), session_id.to_string()))
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{VmmId, MANIFEST_VER};

    fn spec() -> SandboxSpec {
        SandboxSpec {
            name: "web".to_string(),
            cpus: 2,
            memory_mb: 512,
            backend: BackendKind::Krucible,
            root_image: None,
            kernel_image: None,
            desktop: false,
            extra_env: HashMap::new(),
        }
    }

    #[test]
    fn mock_backend_lifecycle_roundtrip() {
        let dir = crate::test_scratch("mock-roundtrip");
        let snap_dir = dir.join("snapshots");
        let backend = MockBackend::new(&snap_dir);
        // Exercise the seam as a trait object, like the daemon will hold it.
        let be: &dyn Backend = &backend;
        assert!(be.capabilities().fork);

        let info = be.create(&spec()).unwrap();
        assert_eq!(info.state, State::Running);
        assert_eq!(info.thermal, Thermal::Hot);
        assert_eq!(be.status(&info.id).unwrap(), info);
        assert_eq!(be.list().unwrap().len(), 1);

        let out = be
            .exec(&info.id, &["echo".to_string(), "hello".to_string()])
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("echo hello"), "got {:?}", out.stdout);
        assert!(!out.truncated);

        let snap = be.create_snapshot(&info.id, "snap-1").unwrap();
        assert_eq!(snap.snapshot_id, "snap-1");
        assert_eq!(snap.manifest_ver, MANIFEST_VER);
        assert!(snap_dir.join("snap-1").join(crate::SIDECAR_NAME).exists());

        // `snapshot` alias resolves to the same operation.
        let snap2 = be.snapshot(&info.id, "snap-2").unwrap();
        assert_eq!(snap2.snapshot_id, "snap-2");

        // Manifest file reads back identical and passes the compat gate.
        let back = SnapshotManifest::read_from(&snap_dir.join("snap-1")).unwrap();
        assert_eq!(back, snap);
        back.check_compat(&host_caps(
            "krucible",
            env!("CARGO_PKG_VERSION"),
            MOCK_KERNEL_DIGEST,
        ))
        .unwrap();

        let restored = be.restore(&snap, "restored-1").unwrap();
        assert_eq!(restored.state, State::Running);
        assert_eq!(restored.thermal, Thermal::Warm);

        let forked = be.fork(&info.id, "fork-1").unwrap();
        assert_eq!(forked.state, State::Running);
        assert_eq!(forked.thermal, Thermal::Hot);

        be.stop(&info.id).unwrap();
        let stopped = be.status(&info.id).unwrap();
        assert_eq!(stopped.state, State::Stopped);
        assert_eq!(stopped.thermal, Thermal::Cold);
        assert!(be.exec(&info.id, &["true".to_string()]).is_err());

        be.start(&info.id).unwrap();
        assert_eq!(be.status(&info.id).unwrap().state, State::Running);

        be.destroy(&info.id).unwrap();
        assert!(matches!(be.status(&info.id), Err(Error::NotFound(_))));
    }

    #[test]
    fn mock_files_roundtrip_with_paging() {
        let dir = crate::test_scratch("mock-files");
        let be = MockBackend::new(dir.join("snapshots"));
        let info = be.create(&spec()).unwrap();

        assert_eq!(
            be.file_write(&info.id, "/a.txt", b"hello world").unwrap(),
            11
        );
        assert_eq!(be.file_write(&info.id, "/sub/b.txt", b"bye").unwrap(), 3);
        let c = be.file_read(&info.id, "/a.txt", 6, 100).unwrap();
        assert_eq!(c.data, b"world");
        assert!(c.eof);
        let c = be.file_read(&info.id, "/a.txt", 0, 5).unwrap();
        assert_eq!(c.data, b"hello");
        assert!(!c.eof);
        assert!(matches!(
            be.file_read(&info.id, "/missing", 0, 9),
            Err(Error::NotFound(_))
        ));

        let l = be.file_list(&info.id, "/", 0, 10).unwrap();
        assert_eq!(l.next_offset, None);
        let names: Vec<_> = l
            .entries
            .iter()
            .map(|e| (e.name.clone(), e.is_dir))
            .collect();
        assert!(names.contains(&("a.txt".to_string(), false)));
        assert!(names.contains(&("sub".to_string(), true)));
        let l = be.file_list(&info.id, "/", 1, 1).unwrap();
        assert_eq!(l.entries.len(), 1);
        assert_eq!(l.next_offset, None);

        // Stopped sandboxes refuse guest file access like exec does.
        be.stop(&info.id).unwrap();
        assert!(matches!(
            be.file_read(&info.id, "/a.txt", 0, 1),
            Err(Error::InvalidState(_))
        ));
    }

    #[test]
    fn mock_sessions_echo_lifecycle() {
        let dir = crate::test_scratch("mock-sessions");
        let be = MockBackend::new(dir.join("snapshots"));
        let info = be.create(&spec()).unwrap();

        let sid = be
            .session_create(&info.id, &["echo".to_string(), "hi".to_string()], false)
            .unwrap();
        let chunk = be
            .session_read(&info.id, &sid, 0, Duration::from_secs(1))
            .unwrap();
        assert_eq!(chunk.data, b"echo hi\n");
        assert!(chunk.eof);
        assert_eq!(chunk.exit_code, Some(0));
        // Drain past the end: empty but still EOF, not an error.
        let chunk = be
            .session_read(&info.id, &sid, 999, Duration::from_secs(1))
            .unwrap();
        assert!(chunk.data.is_empty() && chunk.eof);

        let list = be.session_list(&info.id).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, sid);
        assert!(!list[0].running);
        be.session_resize(&info.id, &sid, 24, 80).unwrap();
        assert!(
            matches!(be.session_kill(&info.id, &sid), Err(Error::InvalidState(_))),
            "kill on an exited session errors (forge parity)"
        );
        assert!(matches!(
            be.session_input(&info.id, &sid, b"x"),
            Err(Error::InvalidState(_))
        ));
        be.session_delete(&info.id, &sid).unwrap();
        assert!(be.session_list(&info.id).unwrap().is_empty());
        assert!(matches!(
            be.session_read(&info.id, &sid, 0, Duration::from_secs(1)),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn mock_restore_refuses_incompatible_manifest() {
        let dir = crate::test_scratch("mock-incompat");
        let be = MockBackend::new(dir.join("snapshots"));
        let info = be.create(&spec()).unwrap();
        let mut snap = be.create_snapshot(&info.id, "snap-1").unwrap();
        snap.compat.vmm = VmmId {
            name: "firecracker".to_string(),
            version: "1.0".to_string(),
        };
        let err = be.restore(&snap, "restored-x").unwrap_err();
        assert!(matches!(err, Error::Incompatible(_)), "unexpected {err}");
    }

    #[test]
    fn mock_reports_not_found_and_conflicts() {
        let dir = crate::test_scratch("mock-errors");
        let be = MockBackend::new(dir.join("snapshots"));
        assert!(matches!(be.status("nope"), Err(Error::NotFound(_))));
        assert!(matches!(be.destroy("nope"), Err(Error::NotFound(_))));
        assert!(matches!(
            be.exec("nope", &["true".to_string()]),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            be.create_snapshot("nope", "s"),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(be.start("nope"), Err(Error::NotFound(_))));
        assert!(matches!(be.stop("nope"), Err(Error::NotFound(_))));
        assert!(matches!(be.fork("nope", "x"), Err(Error::NotFound(_))));

        let a = be.create(&spec()).unwrap();
        let b = be.create(&spec()).unwrap();
        assert!(matches!(be.fork(&a.id, &b.id), Err(Error::Conflict(_))));
        let snap = be.create_snapshot(&a.id, "s").unwrap();
        assert!(matches!(be.restore(&snap, &b.id), Err(Error::Conflict(_))));
    }
}
