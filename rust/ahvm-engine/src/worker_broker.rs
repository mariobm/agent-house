//! The unprivileged daemon can request only fixed worker roles from a root broker.
use crate::Worker;
use serde::{Deserialize, Serialize};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerRole {
    Vmm,
    Netd,
}
impl WorkerRole {
    pub fn name(self) -> &'static str {
        match self {
            Self::Vmm => "vmm",
            Self::Netd => "netd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerIdentity {
    pub socket: PathBuf,
    pub role: WorkerRole,
    pub uid: u32,
    pub gid: u32,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerAction {
    Check,
    Launch,
    Status,
    Stop,
    Inspect,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerRequest {
    pub action: BrokerAction,
    pub id: String,
    pub role: WorkerRole,
    pub worker: Option<Worker>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerReply {
    pub error: Option<String>,
    pub worker: Option<Worker>,
    pub alive: bool,
    pub data_dir: PathBuf,
    pub vmm_bin: PathBuf,
    pub netd_bin: Option<PathBuf>,
    pub cgroup_root: PathBuf,
    pub lib_path: String,
    /// Sealed approved disk of the exact root-owned VMM launch record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_disk: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct WorkerBrokerConfig {
    pub socket: PathBuf,
}

impl WorkerBrokerConfig {
    pub fn request(&self, request: &BrokerRequest) -> io::Result<BrokerReply> {
        let mut socket = UnixStream::connect(&self.socket)?;
        #[cfg(target_os = "linux")]
        if rustix::net::sockopt::socket_peercred(&socket)?.uid.as_raw() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "worker broker peer must be root",
            ));
        }
        // Destructive actor work may precede an otherwise read-only request.
        // Ordinary live observation uses local proc credentials, avoiding this
        // deadline on the steady-state supervisor path.
        let timeout = Duration::from_secs(15);
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        serde_json::to_writer(&mut socket, request).map_err(io::Error::other)?;
        socket.write_all(b"\n")?;
        let mut line = String::new();
        BufReader::new(socket.take(16385)).read_line(&mut line)?;
        if line.len() > 16384 || !line.ends_with('\n') {
            return Err(io::Error::other("invalid worker broker reply"));
        }
        let reply: BrokerReply = serde_json::from_str(&line).map_err(io::Error::other)?;
        if let Some(error) = reply.error {
            return Err(io::Error::other(format!("worker broker: {error}")));
        }
        Ok(reply)
    }

    pub fn check(
        &self,
        data: &Path,
        binary: &Path,
        libraries: &str,
        cgroups: &Path,
    ) -> io::Result<()> {
        let reply = self.request(&BrokerRequest {
            action: BrokerAction::Check,
            id: String::new(),
            role: WorkerRole::Vmm,
            worker: None,
        })?;
        if reply.data_dir != data.canonicalize()?
            || reply.vmm_bin != binary.canonicalize()?
            || reply.lib_path != libraries
            || reply.cgroup_root != cgroups.canonicalize()?
        {
            return Err(io::Error::other(
                "worker broker configuration differs from daemon; stop workers before migration",
            ));
        }
        Ok(())
    }

    pub fn launch(&self, dir: &Path, role: WorkerRole) -> io::Result<crate::LiveWorker> {
        let id = dir
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| io::Error::other("invalid worker directory"))?;
        let reply = self.request(&BrokerRequest {
            action: BrokerAction::Launch,
            id: id.into(),
            role,
            worker: None,
        })?;
        let worker = reply
            .worker
            .ok_or_else(|| io::Error::other("worker broker omitted launch record"))?;
        if worker.state_path.parent() != Some(dir)
            || worker.id != id
            || worker.isolation.as_ref().is_none_or(|i| {
                i.socket != self.socket || i.role != role || i.uid == 0 || i.gid == 0
            })
        {
            return Err(io::Error::other(
                "worker broker returned an invalid identity",
            ));
        }
        if let Err(error) = worker.persist() {
            let _ = self.stop(&worker);
            return Err(error);
        }
        Ok(crate::LiveWorker::broker_owned(worker))
    }

    pub fn status(&self, worker: &Worker) -> io::Result<bool> {
        Ok(self.operation(BrokerAction::Status, worker)?.alive)
    }
    pub fn stop(&self, worker: &Worker) -> io::Result<()> {
        self.operation(BrokerAction::Stop, worker)?;
        Ok(())
    }
    fn operation(&self, action: BrokerAction, worker: &Worker) -> io::Result<BrokerReply> {
        let identity = worker
            .isolation
            .as_ref()
            .ok_or_else(|| io::Error::other("missing broker worker identity"))?;
        if identity.socket != self.socket {
            return Err(io::Error::other("worker broker identity mismatch"));
        }
        self.request(&BrokerRequest {
            action,
            id: worker.id.clone(),
            role: identity.role,
            worker: Some(worker.clone()),
        })
    }
}

impl Worker {
    /// Errors are not death: callers must not overwrite disks or remove groups
    /// when an authenticated broker cannot confirm that a worker has exited.
    pub fn verified_alive(&self) -> io::Result<bool> {
        if let Some(identity) = &self.isolation {
            // Steady-state observation needs no broker round trip or lock.
            // Adoption/destructive operations separately consult its root-owned
            // records. If the main PID disappears, only the broker can confirm
            // that its descendants and open FDs have also been drained.
            if crate::is_alive(self.pid)
                && self
                    .starttime
                    .is_some_and(|t| crate::process_starttime(self.pid) == Some(t))
            {
                let status = std::fs::read_to_string(format!("/proc/{}/status", self.pid))?;
                let ids = |key: &str, expected: u32| {
                    status
                        .lines()
                        .find_map(|line| line.strip_prefix(key))
                        .is_some_and(|line| {
                            line.split_whitespace()
                                .take(3)
                                .all(|value| value.parse::<u32>() == Ok(expected))
                        })
                };
                if ids("Uid:", identity.uid) && ids("Gid:", identity.gid) {
                    return Ok(true);
                }
            }
            WorkerBrokerConfig {
                socket: identity.socket.clone(),
            }
            .status(self)
        } else {
            Ok(crate::is_alive(self.pid)
                && self
                    .starttime
                    .is_some_and(|t| crate::process_starttime(self.pid) == Some(t)))
        }
    }
}
