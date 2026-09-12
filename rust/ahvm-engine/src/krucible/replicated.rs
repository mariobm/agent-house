use super::*;
use crate::{SandboxStorage, StorageMode};

pub(super) fn validate_storage_record(record: &SandboxRecord) -> Result<()> {
    let storage = &record.info.storage;
    if record.spec.storage_mode.unwrap_or(StorageMode::Local) != storage.mode {
        return Err(Error::InvalidState(
            "persisted storage mode mismatch".into(),
        ));
    }
    match (storage.mode, storage.volume_id.as_deref()) {
        (StorageMode::Local, None) => Ok(()),
        (StorageMode::Replicated, Some(id)) => crate::replicated::validate_volume(id),
        _ => Err(Error::InvalidState(
            "persisted volume identity mismatch".into(),
        )),
    }
}

impl KrucibleBackend {
    pub(super) fn replica(&self) -> Result<&crate::ReplicatedConfig> {
        self.cfg
            .replicated
            .as_ref()
            .ok_or_else(|| Error::InvalidState("replicated storage is not configured".into()))
    }
    pub(super) fn is_replicated(&self, id: &str) -> Result<bool> {
        Ok(self
            .lock()
            .sandboxes
            .get(id)
            .is_some_and(|r| r.record.info.storage.mode == StorageMode::Replicated))
    }
    pub(super) fn replicated_record(&self, id: &str) -> Result<(PathBuf, SandboxRecord)> {
        let inner = self.lock();
        let rec = inner
            .sandboxes
            .get(id)
            .ok_or_else(|| Error::NotFound(id.into()))?;
        validate_storage_record(&rec.record)?;
        if rec.record.info.storage.mode != StorageMode::Replicated {
            return Err(Error::InvalidState("sandbox uses local storage".into()));
        }
        Ok((rec.dir.clone(), rec.record.clone()))
    }
    fn save_replica(&self, id: &str, record: SandboxRecord) -> Result<()> {
        let dir = self.cfg.data_dir.join(id);
        // Update memory even when persistence fails: never keep reporting Running
        // after the worker has been terminated. Retry uses the same volume ID.
        self.lock()
            .sandboxes
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(id.into()))?
            .record = record.clone();
        self.persist_record(&dir, &record)
    }
    pub(super) fn create_replicated(&self, spec: &SandboxSpec) -> Result<SandboxInfo> {
        self.replica()?;
        if !cfg!(any(target_os = "linux", test))
            || spec.backend != BackendKind::Krucible
            || spec.cpus == 0
            || spec.cpus > 8
            || spec.memory_mb < 128
        {
            return Err(Error::InvalidState(
                "replicated storage requires Linux krucible and valid sizing".into(),
            ));
        }
        // Resource/quota accounting for the external sidecar is a later host
        // service integration. Never imply that the VM's quota covers it.
        if self.cfg.resources.is_some() || self.cfg.storage.is_some() {
            return Err(Error::InvalidState(
                "replicated service resource accounting is not integrated".into(),
            ));
        }
        if self.lock().sandboxes.contains_key(&spec.name) {
            return Err(Error::Conflict("sandbox already exists".into()));
        }
        let backing = spec
            .root_image
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cfg.base_image.clone());
        let backing = backing.canonicalize()?;
        if !backing.is_file() {
            return Err(Error::InvalidState("base image missing".into()));
        }
        let dir = self.cfg.data_dir.join(&spec.name);
        // An interrupted create owns its directory. Never reuse/remove it under
        // a newly generated volume ID, even when sandbox.json was not written.
        std::fs::create_dir(&dir)?;
        let volume_id = crate::replicated::new_volume()?;
        let record = SandboxRecord {
            spec: SandboxSpec {
                storage_mode: Some(StorageMode::Replicated),
                ..spec.clone()
            },
            info: SandboxInfo {
                id: spec.name.clone(),
                name: spec.name.clone(),
                state: State::Creating,
                thermal: Thermal::Cold,
                ip: if self.networks.is_some() {
                    "100.64.0.2".into()
                } else {
                    String::new()
                },
                storage: SandboxStorage {
                    mode: StorageMode::Replicated,
                    volume_id: Some(volume_id.clone()),
                    replication: None,
                },
            },
            backing,
            deleting: false,
            volume_prepared: false,
        };
        // The intent is durable BEFORE requesting any remote allocation.
        self.persist_record(&dir, &record)?;
        std::fs::File::open(&self.cfg.data_dir)?.sync_all()?;
        self.lock().sandboxes.insert(
            spec.name.clone(),
            LiveRec {
                record: record.clone(),
                dir,
                worker: None,
                needs_resume: false,
            },
        );
        // A failed/ambiguous prepare is retained as an accounted Failed record;
        // destroy can delete its known ID, start retries idempotent preparation.
        if let Err(e) = self.start_replicated(&spec.name) {
            let (_, mut record) = self.replicated_record(&spec.name)?;
            record.info.state = State::Failed;
            let _ = self.save_replica(&spec.name, record);
            return Err(e);
        }
        self.replicated_status(&spec.name)
    }
    pub(super) fn start_replicated(&self, id: &str) -> Result<()> {
        let (dir, mut record) = self.replicated_record(id)?;
        if record.deleting {
            return Err(Error::Conflict("sandbox deletion is pending".into()));
        }
        let service = self.replica()?;
        let volume_id = record.info.storage.volume_id.clone().unwrap();
        let volume = volume_id.as_str();
        let alive = {
            let mut inner = self.lock();
            let rec = inner.sandboxes.get_mut(id).unwrap();
            let alive = rec.worker.as_mut().is_some_and(|w| w.alive());
            if !alive {
                rec.worker = None;
            }
            alive
        };
        if alive {
            service.inspect(volume)?;
            self.ready(&dir)?;
            record.info.state = State::Running;
            return self.save_replica(id, record);
        }
        record.info.state = State::Failed;
        self.save_replica(id, record.clone())?;
        // prepare is idempotent: it must never reimport an existing volume.
        if !record.volume_prepared {
            service.prepare(volume, &record.backing)?;
            record.volume_prepared = true;
            self.save_replica(id, record.clone())?;
        }
        let device = service.attach(volume)?;
        let mut worker = match self.boot_worker(&dir, &device, &record.spec, None) {
            Ok(w) => w,
            Err(e) => {
                let _ = service.detach(volume);
                return Err(e);
            }
        };
        if let Err(e) = self.ready(&dir) {
            // Keep the handle if termination fails; never detach a live VM disk.
            if worker.terminate().is_err() {
                self.lock().sandboxes.get_mut(id).unwrap().worker =
                    Some(WorkerHandle::Owned(worker));
            } else {
                let _ = service.detach(volume);
                if let Some(net) = &self.networks {
                    let _ = net.remove(&dir);
                }
            }
            return Err(e);
        }
        self.lock().sandboxes.get_mut(id).unwrap().worker = Some(WorkerHandle::Owned(worker));
        record.info.state = State::Running;
        record.info.thermal = Thermal::Hot; // cold boot; RAM is never restored
        self.save_replica(id, record)
    }
    fn kill_replica_worker(&self, id: &str) -> Result<()> {
        // Operation reservation excludes start/stop/delete; take the handle out
        // so termination never holds the global map mutex.
        let mut worker = self.lock().sandboxes.get_mut(id).unwrap().worker.take();
        if let Some(handle) = &mut worker {
            if let Err(e) = handle.terminate() {
                self.lock().sandboxes.get_mut(id).unwrap().worker = worker;
                return Err(e);
            }
        }
        Ok(())
    }
    pub(super) fn stop_replicated(&self, id: &str) -> Result<()> {
        let (dir, mut record) = self.replicated_record(id)?;
        if record.deleting {
            return Err(Error::Conflict("sandbox deletion is pending".into()));
        }
        let alive = self
            .lock()
            .sandboxes
            .get_mut(id)
            .unwrap()
            .worker
            .as_mut()
            .is_some_and(|w| w.alive());
        if alive {
            let result = rpc_exec(
                &forge_sock(&dir),
                &["/bin/sync".into()],
                Duration::from_secs(30),
            )?;
            if result.exit_code != 0 {
                return Err(Error::Control("guest disk sync failed".into()));
            }
        }
        self.kill_replica_worker(id)?;
        record.info.state = State::Failed;
        self.save_replica(id, record.clone())?;
        if let Some(net) = &self.networks {
            net.remove(&dir)?;
        }
        let volume = record.info.storage.volume_id.as_deref().unwrap();
        // Stop has a stronger promise than guest fsync: drain before detaching.
        // Failure leaves the same journal tracked and stop/destroy retryable.
        let status = self.replica()?.sync(volume)?;
        self.replica()?.detach(volume)?;
        let _ = std::fs::remove_file(dir.join("state.json"));
        record.info.state = State::Stopped;
        record.info.thermal = Thermal::Cold;
        record.info.storage.replication = Some(status);
        self.save_replica(id, record)
    }
    pub(super) fn destroy_replicated(&self, id: &str) -> Result<()> {
        let (dir, mut record) = self.replicated_record(id)?;
        record.deleting = true;
        record.info.state = State::Failed;
        self.save_replica(id, record.clone())?;
        self.kill_replica_worker(id)?;
        if let Some(net) = &self.networks {
            net.remove(&dir)?;
        }
        // Idempotent service deletion tombstones the ID. GC is a later phase.
        self.replica()?
            .delete(record.info.storage.volume_id.as_deref().unwrap())?;
        self.remove_storage(&dir)?;
        std::fs::File::open(&self.cfg.data_dir)?.sync_all()?;
        self.lock().sandboxes.remove(id);
        Ok(())
    }
    pub(super) fn replicated_status(&self, id: &str) -> Result<SandboxInfo> {
        let (_, record) = self.replicated_record(id)?;
        let mut info = record.info;
        let service = self.replica()?;
        info.storage.replication = service
            .status(info.storage.volume_id.as_deref().unwrap())
            .ok();
        let alive = self
            .lock()
            .sandboxes
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(id.into()))?
            .worker
            .as_mut()
            .is_some_and(|w| w.alive());
        if record.deleting
            || (info.state == State::Running
                && (!alive
                    || info
                        .storage
                        .replication
                        .as_ref()
                        .is_none_or(|s| s.local_failed)))
        {
            info.state = State::Failed;
        }
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn config(tag: &str) -> KrucibleConfig {
        let root = PathBuf::from(format!("/tmp/ahvm-rep-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.join("base"), b"fixture").unwrap();
        let mut cfg = KrucibleConfig::new(
            "/usr/bin/true".into(),
            root.join("base"),
            root.join("vms"),
            String::new(),
        );
        cfg.replicated = Some(crate::ReplicatedConfig {
            socket: root.join("s.sock"),
        });
        cfg
    }
    fn spec() -> SandboxSpec {
        serde_json::from_value(serde_json::json!({"name":"probe","cpus":1,"memory_mb":1024,"backend":"krucible","storage_mode":"replicated"})).unwrap()
    }
    fn replies(
        cfg: &KrucibleConfig,
        operations: Vec<(&'static str, bool)>,
    ) -> std::thread::JoinHandle<Vec<String>> {
        let listener = UnixListener::bind(&cfg.replicated.as_ref().unwrap().socket).unwrap();
        std::thread::spawn(move || {
            let mut ids = Vec::new();
            for (expected, ok) in operations {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["operation"], expected);
                ids.push(request["volume_id"].as_str().unwrap().to_string());
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"ok":ok,"volume_id":request["volume_id"]})
                )
                .unwrap();
            }
            ids
        })
    }
    #[test]
    fn ambiguous_create_retains_id_and_prepared_restart_never_reimports() {
        let cfg = config("prepare");
        let task = replies(
            &cfg,
            vec![
                ("prepare", false),
                ("prepare", true),
                ("attach", false),
                ("attach", false),
            ],
        );
        let be = KrucibleBackend::open(cfg.clone()).unwrap();
        assert!(be.create(&spec()).is_err());
        let (_, record) = be.replicated_record("probe").unwrap();
        assert_eq!(record.info.state, State::Failed);
        assert!(!record.volume_prepared);
        assert!(be.create(&spec()).is_err());
        assert!(be.start("probe").is_err());
        assert!(be.replicated_record("probe").unwrap().1.volume_prepared);
        drop(be);
        let be = KrucibleBackend::open(cfg.clone()).unwrap();
        assert!(be.start("probe").is_err());
        let ids = task.join().unwrap();
        assert_eq!(ids.len(), 4);
        assert!(ids.iter().all(|id| id == &ids[0]));
        let mut unavailable = cfg.clone();
        unavailable.replicated = None;
        assert!(KrucibleBackend::open(unavailable).is_err());
        std::fs::remove_dir_all(cfg.data_dir.parent().unwrap()).unwrap();
    }
    #[test]
    fn failed_delete_survives_reopen_and_blocks_resurrection() {
        let cfg = config("delete");
        let task = replies(
            &cfg,
            vec![("prepare", false), ("delete", false), ("delete", true)],
        );
        let be = KrucibleBackend::open(cfg.clone()).unwrap();
        assert!(be.create(&spec()).is_err());
        assert!(be.destroy("probe").is_err());
        assert!(be.replicated_record("probe").unwrap().1.deleting);
        drop(be);
        let be = KrucibleBackend::open(cfg.clone()).unwrap();
        assert!(matches!(be.start("probe"), Err(Error::Conflict(_))));
        be.destroy("probe").unwrap();
        assert!(!cfg.data_dir.join("probe").exists());
        let ids = task.join().unwrap();
        assert!(ids.iter().all(|id| id == &ids[0]));
        std::fs::remove_dir_all(cfg.data_dir.parent().unwrap()).unwrap();
    }
    #[test]
    fn local_legacy_records_and_missing_service_fail_closed() {
        let mut cfg = config("local");
        cfg.replicated = None;
        let be = KrucibleBackend::open(cfg.clone()).unwrap();
        assert!(be.create(&spec()).is_err());
        assert!(!cfg.data_dir.join("probe").exists());
        let info: SandboxInfo = serde_json::from_value(
            serde_json::json!({"id":"x","name":"x","state":"stopped","thermal":"cold","ip":""}),
        )
        .unwrap();
        assert_eq!(info.storage.mode, StorageMode::Local);
        let mut rec = SandboxRecord {
            spec: spec(),
            info,
            backing: cfg.base_image.clone(),
            deleting: false,
            volume_prepared: false,
        };
        assert!(validate_storage_record(&rec).is_err());
        rec.spec.storage_mode = None;
        assert!(validate_storage_record(&rec).is_ok());
        rec.info.storage.volume_id = Some("a".repeat(64));
        assert!(validate_storage_record(&rec).is_err());
        std::fs::remove_dir_all(cfg.data_dir.parent().unwrap()).unwrap();
    }
}
