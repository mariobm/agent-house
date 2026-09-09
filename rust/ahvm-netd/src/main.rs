//! One sandbox's outbound TCP/DNS gateway. Policy is supplied by the host.
mod dns;
mod gateway;

use nix::poll::{poll, PollFd, PollFlags};
use serde::Deserialize;
use std::net::Ipv4Addr;
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    socket: PathBuf,
    resolver: Ipv4Addr,
    #[serde(default)]
    private_access: Vec<std::net::SocketAddrV4>,
}

fn host_ips() -> std::io::Result<Vec<Ipv4Addr>> {
    Ok(nix::ifaddrs::getifaddrs()?
        .filter_map(|interface| {
            interface
                .address
                .and_then(|addr| addr.as_sockaddr_in().map(|v| v.ip()))
        })
        .collect())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: ahvm-netd CONFIG.json")?;
    let cfg: Config = serde_json::from_slice(&std::fs::read(path)?)?;
    if cfg.resolver.is_unspecified() || cfg.resolver.is_multicast() || cfg.resolver.is_broadcast() {
        return Err("resolver must be a unicast IPv4 address".into());
    }
    if cfg.private_access.len() > 64
        || cfg
            .private_access
            .iter()
            .any(|r| !ahvm_proto::valid_private_endpoint(*r))
    {
        return Err("invalid private-access policy".into());
    }
    // Only the supervisor may remove an old socket after verifying its owner died.
    let listener = UnixListener::bind(&cfg.socket)?;
    std::fs::set_permissions(&cfg.socket, std::fs::Permissions::from_mode(0o600))?;
    host_ips()?;
    eprintln!("netd: ready {}", cfg.socket.display());
    let connecting = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    listener.set_nonblocking(true)?;
    let mut waiting_since = Instant::now();
    loop {
        if waiting_since.elapsed() > Duration::from_secs(30) {
            return Ok(());
        }
        let mut ready = [PollFd::new(listener.as_fd(), PollFlags::POLLIN)];
        match poll(&mut ready, 1000u16) {
            Ok(0) | Err(nix::errno::Errno::EINTR) => continue,
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        match listener.accept().map(|(stream, _)| stream) {
            Ok(stream) => {
                if let Err(e) = gateway::serve(
                    stream,
                    cfg.resolver,
                    &cfg.private_access,
                    connecting.clone(),
                ) {
                    eprintln!("netd: guest link closed: {e}");
                }
                waiting_since = Instant::now();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
}
