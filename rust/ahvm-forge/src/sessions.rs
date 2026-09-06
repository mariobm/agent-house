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

#[derive(Debug, Default)]
struct Lifecycle {
    pgid: Option<i32>,
    exit: Option<i32>,
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub argv: Vec<String>,
    pub started_at: i64,
    lifecycle: Mutex<Lifecycle>,
    ring: Mutex<Ring>,
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

        // Reserve slot atomically before spawning: insert placeholder so
        // concurrent creators see the reservation. Released on spawn failure.
        // We do eviction and reservation under same lock.
        let placeholder = Arc::new(Session {
            id: id.clone(),
            argv: argv.clone(),
            started_at: unix_now(),
            lifecycle: Mutex::new(Lifecycle { pgid: None, exit: None }),
            ring: Mutex::new(Ring::default()),
            stdin: Mutex::new(None),
            master: Mutex::new(None),
            is_pty: pty,
            active_pumps: Arc::new(AtomicUsize::new(0)),
        });
        {
            let mut map = self.sessions.lock().unwrap();
            if map.len() >= MAX_SESSIONS {
                let mut oldest: Option<(String, i64)> = None;
                for (oid, s) in map.iter() {
                    let lc = s.lifecycle.lock().unwrap();
                    if lc.exit.is_some() {
                        let started = s.started_at;
                        let is_older = match &oldest {
                            None => true,
                            Some((ooid, ot)) => (started, oid) < (*ot, ooid),
                        };
                        if is_older {
                            oldest = Some((oid.clone(), started));
                        }
                    }
                }
                if let Some((evict_id, _)) = oldest {
                    map.remove(&evict_id);
                } else {
                    return Err("too many active sessions".into());
                }
            }
            map.insert(id.clone(), placeholder);
        }

        let result = if pty {
            self.create_pty_inner(id.clone(), argv.clone(), env.clone(), cwd.clone())
        } else {
            self.create_piped_inner(id.clone(), argv.clone(), env.clone(), cwd.clone())
        };

        match result {
            Ok(real) => {
                let mut map = self.sessions.lock().unwrap();
                map.insert(id.clone(), real);
                Ok(id)
            }
            Err(e) => {
                let mut map = self.sessions.lock().unwrap();
                map.remove(&id);
                Err(e)
            }
        }
    }

    fn create_piped_inner(
        &self,
        id: String,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
    ) -> Result<Arc<Session>, String> {
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
            lifecycle: Mutex::new(Lifecycle { pgid: Some(child.id() as i32), exit: None }),
            ring: Mutex::new(Ring::default()),
            stdin: Mutex::new(child.stdin.take()),
            master: Mutex::new(None),
            is_pty: false,
            active_pumps: active_pumps.clone(),
        });
        let pump = |pipe: Option<Box<dyn Read + Send>>, session: Arc<Session>, counter: Arc<AtomicUsize>| {
            std::thread::spawn(move || pump_stream(pipe, &session, &counter))
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
        let pumps = active_pumps.clone();
        let session_clone = session.clone();
        std::thread::spawn(move || {
            reap_session(child, &session_clone, &pumps);
            // Close I/O to free FDs (reap published exit already).
            *session_clone.stdin.lock().unwrap() = None;
        });
        Ok(session)
    }

    fn create_pty_inner(
        &self,
        id: String,
        argv: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
    ) -> Result<Arc<Session>, String> {
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
        let child = cmd.spawn().map_err(|e| format!("spawn pty: {e}"))?;
        let active_pumps = Arc::new(AtomicUsize::new(1));
        let session = Arc::new(Session {
            id: id.clone(),
            argv,
            started_at: unix_now(),
            lifecycle: Mutex::new(Lifecycle { pgid: Some(child.id() as i32), exit: None }),
            ring: Mutex::new(Ring::default()),
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
                pump_stream(
                    Some(Box::new(master_clone) as Box<dyn Read + Send>),
                    &session_clone,
                    &pumps,
                );
            });
        }
        let pumps = active_pumps.clone();
        let session_clone = session.clone();
        std::thread::spawn(move || {
            reap_session(child, &session_clone, &pumps);
            // Close master to free the FD (reap published exit already).
            *session_clone.master.lock().unwrap() = None;
        });
        Ok(session)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    pub fn kill(&self, id: &str) -> Result<(), String> {
        let s = self
            .get(id)
            .ok_or_else(|| format!("no such session: {id}"))?;
        let lc = s.lifecycle.lock().unwrap();
        if lc.exit.is_some() {
            return Err(format!("session {id} already completed"));
        }
        match lc.pgid {
            Some(pgid) => {
                kill_group(pgid);
                Ok(())
            }
            None => Err(format!("session {id} already reaped")),
        }
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        // Remove from the map first so new lookups fail fast, and release
        // the map lock before any other lock: global order is map ->
        // lifecycle -> I/O, never nested in reverse (writer restore takes
        // lifecycle-then-I/O as separate snapshots for the same reason).
        let s = {
            let mut map = self.sessions.lock().unwrap();
            map.remove(id)
                .ok_or_else(|| format!("no such session: {id}"))?
        };
        {
            let mut lc = s.lifecycle.lock().unwrap();
            if lc.exit.is_none() {
                if let Some(pgid) = lc.pgid.take() {
                    kill_group(pgid);
                }
            }
            lc.exit = Some(124);
            lc.pgid = None;
        }
        *s.stdin.lock().unwrap() = None;
        *s.master.lock().unwrap() = None;
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
            .map(|s| {
                let lc = s.lifecycle.lock().unwrap();
                SessionInfo {
                    id: s.id.clone(),
                    argv: s.argv.clone(),
                    running: lc.exit.is_none(),
                    started_at: s.started_at,
                }
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
        let exit = self.lifecycle.lock().unwrap().exit;
        (chunk, next, exit, truncated)
    }

    pub fn total(&self) -> u64 {
        self.ring.lock().unwrap().total
    }

    pub fn pumps_done(&self) -> bool {
        self.active_pumps.load(Ordering::SeqCst) == 0
    }

    pub fn write_stdin(&self, data: &[u8]) -> Result<(), String> {
        if self.is_pty {
            // Take master out, write without holding lock, then put back if still valid
            let file = {
                let mut guard = self.master.lock().unwrap();
                guard.take().ok_or("pty closed")?
            };
            // Use poll with timeout to make cancellable: set non-blocking and wait
            let fd = file.as_raw_fd();
            let orig_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if orig_flags >= 0 {
                unsafe { libc::fcntl(fd, libc::F_SETFL, orig_flags | libc::O_NONBLOCK); }
            }
            let mut written = 0;
            let start = std::time::Instant::now();
            let timeout = std::time::Duration::from_secs(5);
            let mut result: Result<(), String> = Ok(());
            while written < data.len() {
                if start.elapsed() > timeout {
                    result = Err("pty write timeout".into());
                    break;
                }
                if self.lifecycle.lock().unwrap().exit.is_some() {
                    result = Err("session completed".into());
                    break;
                }
                let n = unsafe {
                    libc::write(fd, data[written..].as_ptr() as *const _, data.len() - written)
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    } else if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    } else {
                        result = Err(format!("pty write: {err}"));
                        break;
                    }
                } else {
                    written += n as usize;
                }
            }
            if orig_flags >= 0 {
                unsafe { libc::fcntl(fd, libc::F_SETFL, orig_flags); }
            }
            // Snapshot liveness BEFORE taking the I/O lock: acquiring
            // lifecycle while holding master/stdin inverts delete()'s order
            // (lifecycle -> I/O) and deadlocks when both run together.
            let alive = self.lifecycle.lock().unwrap().exit.is_none();
            {
                let mut guard = self.master.lock().unwrap();
                if result.is_ok() && alive && guard.is_none() {
                    *guard = Some(file);
                }
                // else: reaper/delete closed it, or the write failed —
                // drop the file either way.
            }
            result
        } else {
            // Piped stdin: take, write without holding lock
            let stdin = {
                let mut guard = self.stdin.lock().unwrap();
                guard.take().ok_or("stdin closed")?
            };
            // Cancellable: raw non-blocking write with timeout + exit poll
            // (see pty branch above).
            let fd = stdin.as_raw_fd();
            let orig_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if orig_flags >= 0 {
                unsafe { libc::fcntl(fd, libc::F_SETFL, orig_flags | libc::O_NONBLOCK); }
            }
            let mut written = 0;
            let start = std::time::Instant::now();
            let timeout = std::time::Duration::from_secs(5);
            let mut result: Result<(), String> = Ok(());
            while written < data.len() {
                if start.elapsed() > timeout {
                    result = Err("stdin write timeout".into());
                    break;
                }
                if self.lifecycle.lock().unwrap().exit.is_some() {
                    result = Err("session completed".into());
                    break;
                }
                let n = unsafe {
                    libc::write(fd, data[written..].as_ptr() as *const _, data.len() - written)
                };
                if n < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    } else if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    } else {
                        result = Err(format!("stdin write: {err}"));
                        break;
                    }
                } else {
                    written += n as usize;
                }
            }
            if orig_flags >= 0 {
                unsafe { libc::fcntl(fd, libc::F_SETFL, orig_flags); }
            }
            // Same no-nesting rule as the pty branch above.
            let alive = self.lifecycle.lock().unwrap().exit.is_none();
            {
                let mut guard = self.stdin.lock().unwrap();
                if result.is_ok() && alive && guard.is_none() {
                    *guard = Some(stdin);
                }
            }
            result
        }
    }
}

/// Pump `pipe` to EOF into the session ring, then release one pump slot.
///
/// WouldBlock is transient, not terminal: a concurrent input write flips the
/// shared O_NONBLOCK flag (dup/clone share file status), so the reader waits
/// for readiness instead of exiting and dropping the tail.
fn pump_stream(
    pipe: Option<Box<dyn Read + Send>>,
    session: &Arc<Session>,
    counter: &Arc<AtomicUsize>,
) {
    if let Some(mut pipe) = pipe {
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => session.append(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                Err(_) => break,
            }
        }
    }
    counter.fetch_sub(1, Ordering::SeqCst);
}

/// Exit code from a raw wait status: real code, or our 124 kill/timeout
/// bucket when killed by a signal.
fn code_of(status: libc::c_int) -> Option<i32> {
    if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        Some(124)
    } else {
        None
    }
}

/// Block until `pid` has exited, WITHOUT reaping it, and return the
/// observed exit code.
///
/// This MUST be `waitid`, not `waitpid`: `WNOWAIT` belongs to `waitid` —
/// `waitpid(..., WNOWAIT)` is EINVAL on both Linux and macOS (verified by C
/// probe on both, 2026-09-06). `waitid(P_PID, pid, WEXITED | WNOWAIT)` blocks
/// until exit and leaves the child as a reserved zombie, so its PID cannot
/// be recycled underneath us. Callers must still reap afterwards; every
/// return path below requires the caller to wait, never leaks a zombie.
fn wait_zombie(pid: i32) -> Option<i32> {
    loop {
        // SAFETY: zeroed siginfo_t written by waitid on success.
        let mut si: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut si,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if r == 0 {
            // WEXITED without WSTOPPED/WCONTINUED only reports real exits.
            // NOTE: si_pid/si_status are accessor METHODS on Linux (union
            // fields); plain `.si_pid` field access only builds on macOS.
            return match si.si_code {
                libc::CLD_EXITED => Some(unsafe { si.si_status() }),
                libc::CLD_KILLED | libc::CLD_DUMPED => Some(124),
                _ => continue,
            };
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if err.raw_os_error() == Some(libc::ECHILD) {
            return None; // already reaped elsewhere; nothing reserved
        }
        // Unexpected: blocking reap (old behavior). Unreachable in practice.
        let mut status = 0;
        let r2 = unsafe { libc::waitpid(pid, &mut status as *mut _, 0) };
        return if r2 == pid { code_of(status) } else { None };
    }
}

/// Reap protocol — the single lifecycle owner for session teardown:
///
/// 1. observe leader exit via [`wait_zombie`] (PID still reserved);
/// 2. take the pgid under the lifecycle lock and signal the group while no
///    unrelated process can hold the number (also unblocks any writer parked
///    on a descendant-held pipe);
/// 3. reap (`Child::wait`), drain pumps, publish exit.
///
/// `kill()`/`delete()` only ever signal a pgid taken under the same lock
/// while `exit` is unset, so a reaped number is never signaled.
fn reap_session(mut child: std::process::Child, session: &Arc<Session>, pumps: &Arc<AtomicUsize>) {
    let leader = child.id() as i32;
    let observed = wait_zombie(leader);
    let pgid = {
        let mut lc = session.lifecycle.lock().unwrap();
        lc.pgid.take()
    };
    if let Some(pgid) = pgid {
        kill_group(pgid);
    }
    // Linux: the leader is a reserved zombie here, so wait() reaps exactly
    // it. Elsewhere wait_zombie already reaped; wait() then fails and the
    // observed code carries the exit.
    let code = child.wait().ok().and_then(|s| s.code()).or(observed);
    for _ in 0..50 {
        if pumps.load(Ordering::SeqCst) == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    {
        let mut lc = session.lifecycle.lock().unwrap();
        lc.exit = Some(code.unwrap_or(124));
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Reader script: WouldBlock injections between data, then EOF.
    /// Models a pump racing a concurrent input write (shared O_NONBLOCK).
    struct Scripted {
        steps: std::collections::VecDeque<Result<Vec<u8>, std::io::ErrorKind>>,
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.steps.pop_front() {
                None => Ok(0),
                Some(Ok(data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    if data.len() > n {
                        self.steps.push_front(Ok(data[n..].to_vec()));
                    }
                    Ok(n)
                }
                Some(Err(kind)) => Err(std::io::Error::new(kind, "injected")),
            }
        }
    }

    fn test_session() -> (Arc<Session>, Arc<AtomicUsize>) {
        let pumps = Arc::new(AtomicUsize::new(1));
        let s = Arc::new(Session {
            id: "test".into(),
            argv: vec![],
            started_at: 0,
            lifecycle: Mutex::new(Lifecycle { pgid: None, exit: None }),
            ring: Mutex::new(Ring::default()),
            stdin: Mutex::new(None),
            master: Mutex::new(None),
            is_pty: false,
            active_pumps: pumps.clone(),
        });
        (s, pumps)
    }

    /// Deterministic guard for the reap protocol: after wait_zombie, the
    /// PID must still be reserved (signalable) with exactly one reap left.
    /// A waitpid-based implementation takes the EINVAL/fallback path here
    /// and fails loudly — which is the point.
    #[test]
    fn wait_zombie_keeps_pid_reserved() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 42")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        assert_eq!(wait_zombie(pid), Some(42));
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "pid must still be reserved after wait_zombie"
        );
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &mut status as *mut _, 0) }, pid);
        let _ = child.wait(); // ECHILD now; must not panic or hang
    }

    /// Deterministic guard for the PTY tail-loss: WouldBlock mid-stream must
    /// wait for readiness, never terminate the pump. (The live repro —
    /// concurrent input during output — only trips on timing; this pins the
    /// logic.)
    #[test]
    fn pump_survives_wouldblock() {
        let (s, pumps) = test_session();
        let reader = Scripted {
            steps: vec![
                Ok(b"READY ".to_vec()),
                Err(std::io::ErrorKind::WouldBlock),
                Err(std::io::ErrorKind::WouldBlock),
                Ok(b"FIRST ".to_vec()),
                Err(std::io::ErrorKind::WouldBlock),
                Ok(b"SECOND".to_vec()),
            ]
            .into(),
        };
        pump_stream(Some(Box::new(reader)), &s, &pumps);
        assert_eq!(pumps.load(Ordering::SeqCst), 0);
        let (chunk, next, _, _) = s.read_from(0);
        assert_eq!(chunk, b"READY FIRST SECOND");
        assert_eq!(next, 18);
    }
}
