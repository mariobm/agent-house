//! Long-lived sessions: spawn once, attach many times, scroll back.
//!
//! stdout+stderr merge into one ordered-by-arrival byte stream with absolute
//! sequence numbers (byte offsets from session start). The ring keeps the
//! last [`RING_CAP`] bytes; attachers pass `from_seq` to resume where they
//! left off and learn `truncated=true` when the head they asked for is gone.
//! PTY sessions use a pty master (ptmx) so interactive programs see a TTY.

#![allow(unsafe_code)]
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex, OnceLock,
};

/// Bytes of scrollback retained per session.
pub const RING_CAP: usize = 256 << 10;
/// Wire chunk size per SessionData frame.
pub const CHUNK: usize = 32 << 10;
/// Max number of sessions retained (including completed). Oldest completed
/// is evicted first; if none, creation fails.
const MAX_SESSIONS: usize = 100;

static MANAGER: OnceLock<SessionManager> = OnceLock::new();
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

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
    master: Mutex<Option<std::fs::File>>,
    is_pty: bool,
    active_pumps: Arc<AtomicUsize>,
}

#[derive(Debug, Default)]
struct Ring {
    base: u64,
    total: u64,
    buf: VecDeque<u8>,
}

#[allow(unsafe_code)]
fn open_ptmx() -> Result<(std::fs::File, String), String> {
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return Err(format!("posix_openpt: {}", std::io::Error::last_os_error()));
    }
    if unsafe { libc::grantpt(fd) } != 0 {
        unsafe { libc::close(fd); }
        return Err(format!("grantpt: {}", std::io::Error::last_os_error()));
    }
    if unsafe { libc::unlockpt(fd) } != 0 {
        unsafe { libc::close(fd); }
        return Err(format!("unlockpt: {}", std::io::Error::last_os_error()));
    }
    let cstr_ptr = unsafe { libc::ptsname(fd) };
    if cstr_ptr.is_null() {
        unsafe { libc::close(fd); }
        return Err(format!("ptsname: {}", std::io::Error::last_os_error()));
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(cstr_ptr) };
    let path = cstr.to_string_lossy().into_owned();
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    Ok((file, path))
}

impl SessionManager {
    pub fn create(
        &self,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
        pty: bool,
    ) -> Result<String, String> {
        if argv.is_empty() {
            return Err("empty argv".into());
        }
        // Bounded retention: evict oldest completed if at limit
        {
            let mut map = self.sessions.lock().unwrap();
            if map.len() >= MAX_SESSIONS {
                // Find oldest completed
                let mut oldest: Option<(String, i64)> = None;
                for (id, s) in map.iter() {
                    if s.exit.lock().unwrap().is_some() {
                        let started = s.started_at;
                        let candidate = (id.clone(), started);
                        let is_older = match &oldest {
                            None => true,
                            Some((oid, ot)) => (started, id) < (*ot, oid),
                        };
                        if is_older {
                            oldest = Some(candidate);
                        }
                    }
                }
                if let Some((evict_id, _)) = oldest {
                    map.remove(&evict_id);
                } else {
                    return Err("too many active sessions".into());
                }
            }
        }
        let id = format!(
            "s-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        );

        if pty {
            return self.create_pty(id, argv, env, cwd);
        }

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
        let active_pumps = Arc::new(AtomicUsize::new(2));
        let session = Arc::new(Session {
            id: id.clone(),
            argv,
            started_at: unix_now(),
            pgid: Mutex::new(Some(child.id() as i32)),
            ring: Mutex::new(Ring::default()),
            exit: Mutex::new(None),
            stdin: Mutex::new(child.stdin.take()),
            master: Mutex::new(None),
            is_pty: false,
            active_pumps: active_pumps.clone(),
        });
        let pump = |pipe: Option<Box<dyn Read + Send>>, session: Arc<Session>, counter: Arc<AtomicUsize>| {
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
                counter.fetch_sub(1, Ordering::SeqCst);
            })
        };
        pump(
            child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
            session.clone(),
            active_pumps.clone(),
        );
        pump(
            child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
            session.clone(),
            active_pumps.clone(),
        );
        std::thread::spawn({
            let session = session.clone();
            let pumps = active_pumps.clone();
            move || {
                let code = child.wait().ok().and_then(|s| s.code());
                // Wait for pumps to drain buffered output before publishing exit
                // (otherwise attach sees exit+currently drained as EOF with missing tail)
                for _ in 0..50 {
                    if pumps.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                *session.exit.lock().unwrap() = Some(code.unwrap_or(124));
                // Close I/O to free FDs; drop stdin
                *session.stdin.lock().unwrap() = None;
                // Invalidate pgid so later kill cannot hit reused PID
                let pgid = session.pgid.lock().unwrap().take();
                if let Some(pgid) = pgid {
                    kill_group(pgid);
                }
            }
        });
        self.sessions.lock().unwrap().insert(id.clone(), session);
        Ok(id)
    }

    #[allow(unsafe_code)]
    fn create_pty(
        &self,
        id: String,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
    ) -> Result<String, String> {
        let (master_file, slave_path) = open_ptmx()?;
        let master_fd = master_file.as_raw_fd();
        {
            let ws = libc::winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws);
            }
        }
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        if let Some(dir) = cwd.clone() {
            cmd.current_dir(dir);
        }
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let c_slave = CString::new(slave_path.clone()).map_err(|e| format!("CString: {e}"))?;
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let slave_fd = libc::open(c_slave.as_ptr(), libc::O_RDWR);
                if slave_fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                    libc::close(slave_fd);
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(slave_fd, 0) < 0
                    || libc::dup2(slave_fd, 1) < 0
                    || libc::dup2(slave_fd, 2) < 0
                {
                    libc::close(slave_fd);
                    return Err(std::io::Error::last_os_error());
                }
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
                libc::close(master_fd);
                Ok(())
            });
        }
        let mut child = cmd.spawn().map_err(|e| format!("spawn pty: {e}"))?;
        let active_pumps = Arc::new(AtomicUsize::new(1));
        let session = Arc::new(Session {
            id: id.clone(),
            argv,
            started_at: unix_now(),
            pgid: Mutex::new(Some(child.id() as i32)),
            ring: Mutex::new(Ring::default()),
            exit: Mutex::new(None),
            stdin: Mutex::new(None),
            master: Mutex::new(Some(master_file)),
            is_pty: true,
            active_pumps: active_pumps.clone(),
        });
        {
            let master_clone = {
                let guard = session.master.lock().unwrap();
                guard.as_ref().unwrap().try_clone().unwrap()
            };
            let session_clone = session.clone();
            let pumps = active_pumps.clone();
            std::thread::spawn(move || {
                let mut file = master_clone;
                let mut chunk = [0u8; 8192];
                loop {
                    match file.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => session_clone.append(&chunk[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                pumps.fetch_sub(1, Ordering::SeqCst);
            });
        }
        std::thread::spawn({
            let session = session.clone();
            let pumps = active_pumps.clone();
            move || {
                let code = child.wait().ok().and_then(|s| s.code());
                for _ in 0..50 {
                    if pumps.load(Ordering::SeqCst) == 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                *session.exit.lock().unwrap() = Some(code.unwrap_or(124));
                // Close master to free FD
                *session.master.lock().unwrap() = None;
                let pgid = session.pgid.lock().unwrap().take();
                if let Some(pgid) = pgid {
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
        // Synchronize: if already exited, kill would hit reused PID
        if s.exit.lock().unwrap().is_some() {
            return Err(format!("session {id} already completed"));
        }
        let pgid = *s.pgid.lock().unwrap();
        match pgid {
            Some(pgid) => {
                kill_group(pgid);
                Ok(())
            }
            None => Err(format!("session {id} already reaped")),
        }
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut map = self.sessions.lock().unwrap();
        let s = map.get(id).ok_or_else(|| format!("no such session: {id}"))?.clone();
        // Kill if still running, then remove
        if s.exit.lock().unwrap().is_none() {
            if let Some(pgid) = *s.pgid.lock().unwrap() {
                kill_group(pgid);
            }
        }
        // Close handles
        *s.stdin.lock().unwrap() = None;
        *s.master.lock().unwrap() = None;
        map.remove(id);
        Ok(())
    }

    pub fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<(), String> {
        let s = self.get(id).ok_or_else(|| format!("no such session: {id}"))?;
        if !s.is_pty {
            return Err("not a pty session".into());
        }
        let guard = s.master.lock().unwrap();
        let file = guard.as_ref().ok_or("master closed")?;
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ret = unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        if ret != 0 {
            return Err(format!("ioctl TIOCSWINSZ: {}", std::io::Error::last_os_error()));
        }
        Ok(())
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

    pub fn read_from(&self, sent: u64) -> (Vec<u8>, u64, Option<i32>, bool) {
        let ring = self.ring.lock().unwrap();
        let start = (sent.max(ring.base) - ring.base).min(ring.buf.len() as u64) as usize;
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

    pub fn pumps_done(&self) -> bool {
        self.active_pumps.load(Ordering::SeqCst) == 0
    }

    pub fn write_stdin(&self, data: &[u8]) -> Result<(), String> {
        use std::io::Write as _;
        if self.is_pty {
            let mut guard = self.master.lock().unwrap();
            match guard.as_mut() {
                Some(f) => f.write_all(data).map_err(|e| format!("pty write: {e}")),
                None => Err("pty closed".into()),
            }
        } else {
            let mut guard = self.stdin.lock().unwrap();
            match guard.as_mut() {
                Some(stdin) => stdin.write_all(data).map_err(|e| format!("stdin: {e}")),
                None => Err("stdin closed".into()),
            }
        }
    }
}

#[allow(unsafe_code)]
fn kill_group(pgid: i32) {
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