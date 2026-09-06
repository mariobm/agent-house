//! Long-lived sessions: spawn once, attach many times, scroll back.
//!
//! stdout+stderr merge into one ordered-by-arrival byte stream with absolute
//! sequence numbers (byte offsets from session start). The ring keeps the
//! last [`RING_CAP`] bytes; attachers pass `from_seq` to resume where they
//! left off and learn `truncated=true` when the head they asked for is gone.
//! PTY sessions use a pty master (ptmx) so interactive programs see a TTY.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd};
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
    master: Mutex<Option<std::fs::File>>,
    is_pty: bool,
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
        });
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
            child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
            session.clone(),
        );
        pump(
            child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
            session.clone(),
        );
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

    #[allow(unsafe_code)]
    fn create_pty(
        &self,
        id: String,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
    ) -> Result<String, String> {
        let (master_file, slave_path) = open_ptmx()?;
        // Need raw fd for child to close
        let master_fd = master_file.as_raw_fd();
        // Set initial size 24x80
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
        let slave_path_clone = slave_path.clone();
        use std::os::unix::process::CommandExt;
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(move || {
                // Create new session, become leader
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Open slave
                let slave_fd = libc::open(
                    slave_path_clone.as_ptr() as *const i8,
                    libc::O_RDWR,
                );
                if slave_fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Make it controlling terminal
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                    libc::close(slave_fd);
                    return Err(std::io::Error::last_os_error());
                }
                // Dup to stdio
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
                // Close master in child
                libc::close(master_fd);
                Ok(())
            });
        }
        // Child's stdio will be slave, so don't pipe
        let mut child = cmd.spawn().map_err(|e| format!("spawn pty: {e}"))?;
        // Keep master for reading/writing
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
        });
        // Pump from master
        {
            let master_clone = {
                let guard = session.master.lock().unwrap();
                guard.as_ref().unwrap().try_clone().unwrap()
            };
            let session_clone = session.clone();
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
            });
        }
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

    #[allow(unsafe_code)]
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
