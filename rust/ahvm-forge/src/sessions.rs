//! Long-lived sessions: spawn once, attach many times, scroll back.
//!
//! stdout+stderr merge into one ordered-by-arrival byte stream with absolute
//! sequence numbers (byte offsets from session start). The ring keeps the
//! last [`RING_CAP`] bytes; attachers pass `from_seq` to resume where they
//! left off and learn `truncated=true` when the head they asked for is gone.
//! PTY mode is rejected explicitly until the PTY chunk lands.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};

/// Bytes of scrollback retained per session.
pub const RING_CAP: usize = 256 << 10;
/// Wire chunk size per SessionData frame.
pub const CHUNK: usize = 32 << 10;

static MANAGER: OnceLock<SessionManager> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn manager() -> &'static SessionManager {
    MANAGER.get_or_init(SessionManager::default)
}

#[derive(Debug, Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub argv: Vec<String>,
    pub started_at: i64,
    pgid: Mutex<Option<i32>>,
    ring: Mutex<Ring>,
    exit: Mutex<Option<i32>>,
    stdin: Mutex<Option<std::process::ChildStdin>>,
}

#[derive(Debug, Default)]
struct Ring {
    /// Absolute seq of `buf[0]` (total bytes ever appended minus len).
    base: u64,
    /// Total bytes ever appended.
    total: u64,
    buf: VecDeque<u8>,
}

impl SessionManager {
    pub fn create(
        &self,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
        pty: bool,
    ) -> Result<String, String> {
        if pty {
            return Err("pty sessions not yet supported".into());
        }
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        let id = format!(
            "s-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        use std::os::unix::process::CommandExt;
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn: {e}"))?;
        let session = Arc::new(Session {
            id: id.clone(),
            argv,
            started_at: unix_now(),
            pgid: Mutex::new(Some(child.id() as i32)),
            ring: Mutex::new(Ring::default()),
            exit: Mutex::new(None),
            stdin: Mutex::new(child.stdin.take()),
        });
        // Pumps: both streams into the one ring, arrival-ordered.
        let pump = |pipe: Option<Box<dyn Read + Send>>, session: Arc<Session>| {
            std::thread::spawn(move || {
                if let Some(mut pipe) = pipe {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match pipe.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => session.append(&chunk[..n]),
                            Err(_) => break,
                        }
                    }
                }
            })
        };
        pump(
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            session.clone(),
        );
        pump(
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
            session.clone(),
        );
        // Reaper: record exit, then kill the group so strays can't linger
        // past the session (same discipline as one-shot exec).
        std::thread::spawn({
            let session = session.clone();
            move || {
                let code = child.wait().ok().and_then(|s| s.code());
                *session.exit.lock().unwrap() = Some(code.unwrap_or(124));
                if let Some(pgid) = *session.pgid.lock().unwrap() {
                    kill_group(pgid);
                }
            }
        });
        self.sessions.lock().unwrap().insert(id.clone(), session);
        Ok(id)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn kill(&self, id: &str) -> Result<(), String> {
        let s = self
            .get(id)
            .ok_or_else(|| format!("no such session: {id}"))?;
        let pgid = *s.pgid.lock().unwrap();
        match pgid {
            Some(pgid) => {
                kill_group(pgid);
                Ok(())
            }
            None => Err(format!("session {id} already reaped")),
        }
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let map = self.sessions.lock().unwrap();
        let mut out: Vec<SessionInfo> = map
            .values()
            .map(|s| SessionInfo {
                id: s.id.clone(),
                argv: s.argv.clone(),
                running: s.exit.lock().unwrap().is_none(),
                started_at: s.started_at,
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInfo {
    pub id: String,
    pub argv: Vec<String>,
    pub running: bool,
    pub started_at: i64,
}

impl Session {
    fn append(&self, data: &[u8]) {
        let mut ring = self.ring.lock().unwrap();
        ring.total += data.len() as u64;
        ring.buf.extend(data.iter());
        if ring.buf.len() > RING_CAP {
            let drop = ring.buf.len() - RING_CAP;
            ring.buf.drain(..drop);
            ring.base += drop as u64;
        }
    }

    /// Snapshot of new bytes since `sent` (absolute seq), plus exit state.
    /// Returns (chunk, next_seq, exited_with_code, truncated).
    pub fn read_from(&self, sent: u64) -> (Vec<u8>, u64, Option<i32>, bool) {
        let ring = self.ring.lock().unwrap();
        let start = (sent.max(ring.base) - ring.base).min(ring.buf.len() as u64) as usize;
        // Clipped head: the caller asked below what we retain.
        let truncated = sent < ring.base;
        let end = (start + CHUNK).min(ring.buf.len());
        let chunk = ring.buf.range(start..end).copied().collect();
        let next = ring.base + end as u64;
        let exit = *self.exit.lock().unwrap();
        (chunk, next, exit, truncated)
    }

    pub fn total(&self) -> u64 {
        self.ring.lock().unwrap().total
    }

    pub fn write_stdin(&self, data: &[u8]) -> Result<(), String> {
        use std::io::Write as _;
        let mut guard = self.stdin.lock().unwrap();
        match guard.as_mut() {
            Some(stdin) => stdin.write_all(data).map_err(|e| format!("stdin: {e}")),
            None => Err("stdin closed".into()),
        }
    }
}

#[allow(unsafe_code)]
fn kill_group(pgid: i32) {
    // SAFETY: pgid is the spawned leader's pid (setpgid in pre_exec makes
    // group id == leader pid). Worst case the group is gone.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
