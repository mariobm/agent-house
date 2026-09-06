//! AF_VSOCK listener so the future VMM worker can reach forge without
//! TCP/network. Served IN ADDITION to TCP whenever `vsock_port > 0`.
//!
//! Runtime paths only run on Linux/KVM. This compiles on macOS too (libc
//! 0.2.189 exposes `AF_VSOCK` + `sockaddr_vm` on both targets, so no
//! fallback consts are needed — only the struct layout differs, handled in
//! `build_sockaddr`), but without a hypervisor the socket/bind calls fail at
//! runtime. That is fine: `serve_vsock` logs and returns, TCP is unaffected.

use crate::{agent::Conn, config::Config};
use std::io;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;

/// Build a bind address for `port` on CID_ANY, plus its length for bind(2).
/// Field names are identical on both targets; only the layout differs —
/// Apple's `sockaddr_vm` leads with an `svm_len` byte (packed) and has no
/// trailing `svm_zero`, while Linux leads with `svm_family`.
#[cfg(target_vendor = "apple")]
fn build_sockaddr(port: u32) -> (libc::sockaddr_vm, libc::socklen_t) {
    let addr = libc::sockaddr_vm {
        svm_len: std::mem::size_of::<libc::sockaddr_vm>() as u8,
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: libc::VMADDR_CID_ANY,
    };
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    (addr, len)
}

#[cfg(not(target_vendor = "apple"))]
fn build_sockaddr(port: u32) -> (libc::sockaddr_vm, libc::socklen_t) {
    let addr = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: port,
        svm_cid: libc::VMADDR_CID_ANY,
        svm_zero: [0; 4],
    };
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    (addr, len)
}

// SAFETY: socket(2) with constant family/type/protocol takes no pointers and
// returns an owned fd (or -1); nothing shared to alias.
#[allow(unsafe_code)]
fn vsock_socket() -> libc::c_int {
    unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) }
}

// SAFETY: `addr` is a live local of the exact type bind(2) expects for
// AF_VSOCK and `len` is its size; the fd is only borrowed here.
#[allow(unsafe_code)]
fn vsock_bind(fd: libc::c_int, addr: &libc::sockaddr_vm, len: libc::socklen_t) -> bool {
    unsafe {
        libc::bind(
            fd,
            addr as *const libc::sockaddr_vm as *const libc::sockaddr,
            len,
        ) == 0
    }
}

// SAFETY: listen(2) on an owned, bound fd; no pointers, no shared state.
#[allow(unsafe_code)]
fn vsock_listen(fd: libc::c_int) -> bool {
    unsafe { libc::listen(fd, 128) == 0 }
}

// SAFETY: accept(2) with NULL addr/len returns an owned fd (or -1); no
// out-pointers to keep alive.
#[allow(unsafe_code)]
fn vsock_accept(fd: libc::c_int) -> libc::c_int {
    unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) }
}

// SAFETY: `fd` is owned by the caller and never used afterwards; single close.
#[allow(unsafe_code)]
fn vsock_close(fd: libc::c_int) {
    unsafe {
        libc::close(fd);
    }
}

// SAFETY: `cfd` is a fresh owned stream fd from accept(2); from_raw_fd takes
// ownership exactly once. A vsock accept fd IS a stream socket, so the
// resulting UnixStream only drives plain read/write syscalls (no
// sockaddr_un-specific use) — read/write/shutdown/try_clone all behave.
#[allow(unsafe_code)]
fn vsock_stream(cfd: libc::c_int) -> UnixStream {
    unsafe { UnixStream::from_raw_fd(cfd) }
}

/// Bind CID_ANY:`port` and serve the agent protocol on every accepted
/// connection. Never panics out: any setup failure is logged and TCP keeps
/// serving (this is what lets tests/dev run unchanged on hosts without
/// vsock, e.g. macOS).
pub fn serve_vsock(port: u32, cfg: &Config) {
    let fd = vsock_socket();
    if fd < 0 {
        eprintln!("forge: vsock socket: {}", io::Error::last_os_error());
        return;
    }
    let (addr, len) = build_sockaddr(port);
    if !vsock_bind(fd, &addr, len) {
        eprintln!(
            "forge: vsock bind port {port}: {}",
            io::Error::last_os_error()
        );
        vsock_close(fd);
        return;
    }
    if !vsock_listen(fd) {
        eprintln!(
            "forge: vsock listen port {port}: {}",
            io::Error::last_os_error()
        );
        vsock_close(fd);
        return;
    }
    // NOTE: this line deliberately does NOT share the "forge: listening on "
    // prefix — integration tests parse that line as a TCP dial address.
    eprintln!(
        "forge: vsock listening on cid {} port {port}",
        libc::VMADDR_CID_ANY
    );
    // The listener fd lives for the process lifetime; this loop never returns.
    loop {
        let cfd = vsock_accept(fd);
        if cfd < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("forge: vsock accept: {e}");
            // Avoid a hot spin if the listener fd went bad.
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }
        let cfg = cfg.clone();
        std::thread::spawn(move || crate::agent::handle(Conn::Vsock(vsock_stream(cfd)), &cfg));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Apple's sockaddr_vm is packed(1): direct field reads are E0793, so
    // copy fields out through addr_of + read_unaligned (a plain aligned copy
    // on Linux's naturally-aligned layout).
    // SAFETY (all four): addr_of! creates no reference, so no misaligned
    // reference ever exists; read_unaligned copies ≤4 bytes from a live local.
    #[allow(unsafe_code)]
    fn family(a: &libc::sockaddr_vm) -> libc::sa_family_t {
        unsafe { std::ptr::addr_of!(a.svm_family).read_unaligned() }
    }

    // SAFETY: see above.
    #[allow(unsafe_code)]
    fn reserved1(a: &libc::sockaddr_vm) -> u16 {
        unsafe { std::ptr::addr_of!(a.svm_reserved1).read_unaligned() }
    }

    // SAFETY: see above.
    #[allow(unsafe_code)]
    fn port(a: &libc::sockaddr_vm) -> u32 {
        unsafe { std::ptr::addr_of!(a.svm_port).read_unaligned() }
    }

    // SAFETY: see above.
    #[allow(unsafe_code)]
    fn cid(a: &libc::sockaddr_vm) -> u32 {
        unsafe { std::ptr::addr_of!(a.svm_cid).read_unaligned() }
    }

    #[test]
    fn sockaddr_family_and_port_layout() {
        let (addr, len) = build_sockaddr(1024);
        assert_eq!(family(&addr) as i32, libc::AF_VSOCK);
        assert_eq!(reserved1(&addr), 0);
        assert_eq!(port(&addr), 1024);
        assert_eq!(cid(&addr), libc::VMADDR_CID_ANY);
        assert_eq!(len as usize, std::mem::size_of::<libc::sockaddr_vm>());
        // Wire order: length byte first on Apple, family first on Linux.
        // Port sits at offset 4 and cid at offset 8 on both layouts.
        // SAFETY: re-reading our own live local through a byte pointer, in
        // bounds for exactly size_of::<sockaddr_vm>() bytes.
        #[allow(unsafe_code)]
        let raw = unsafe {
            std::slice::from_raw_parts(
                &addr as *const libc::sockaddr_vm as *const u8,
                std::mem::size_of::<libc::sockaddr_vm>(),
            )
        };
        #[cfg(target_vendor = "apple")]
        {
            assert_eq!(raw[0] as usize, std::mem::size_of::<libc::sockaddr_vm>());
            assert_eq!(raw[1], libc::AF_VSOCK as u8);
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            assert_eq!(
                u16::from_ne_bytes(raw[0..2].try_into().unwrap()),
                libc::AF_VSOCK as u16
            );
        }
        assert_eq!(
            u32::from_ne_bytes(raw[4..8].try_into().unwrap()),
            1024
        );
        assert_eq!(
            u32::from_ne_bytes(raw[8..12].try_into().unwrap()),
            libc::VMADDR_CID_ANY
        );
    }
}
