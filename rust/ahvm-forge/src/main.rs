//! ahvm-forge: guest agent. Listens for v2-framed connections (TCP in
//! tests/dev, vsock once the engine wires it), enforces an optional
//! bearer token, and serves exec + file operations.
//!
//! Sessions and PTY Support are explicitly deferred to the next chunk;
//! every request here runs to completion on its own connection.

mod agent;
mod config;
mod exec;
mod files;
mod forward;
mod sessions;
mod transport;

use std::net::TcpListener;

fn main() {
    let cfg = config::Config::from_env();
    // Either transport failing alone is fine; only BOTH failing is fatal.
    // (In a guest without TCP, vsock is the only way in — a TCP bind
    // failure there must never exit PID 1, which would halt the whole VM.)
    let tcp = match TcpListener::bind(&cfg.listen_addr) {
        Ok(l) => {
            let bound = l
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or(cfg.listen_addr.clone());
            eprintln!("forge: listening on {bound}");
            Some(l)
        }
        Err(e) => {
            eprintln!("forge: tcp listen {}: {e}", cfg.listen_addr);
            None
        }
    };
    let vsock = if cfg.vsock_port > 0 {
        match transport::VsockListener::bind(cfg.vsock_port) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("forge: vsock port {}: {e}", cfg.vsock_port);
                None
            }
        }
    } else {
        None
    };
    match (tcp, vsock) {
        (None, None) => {
            eprintln!("forge: no listener (tcp and vsock both failed); exiting");
            std::process::exit(1);
        }
        (Some(t), Some(v)) => {
            let thread_cfg = cfg.clone();
            std::thread::spawn(move || v.serve(&thread_cfg));
            serve_tcp(t, &cfg);
        }
        (Some(t), None) => serve_tcp(t, &cfg),
        (None, Some(v)) => v.serve(&cfg),
    }
}

/// TCP accept loop. Never returns.
fn serve_tcp(listener: TcpListener, cfg: &config::Config) -> ! {
    // Report the BOUND address (differs from config when :0 was given) —
    // integration tests parse this line to find us without port probing.
    // (Already logged in main; kept here for single-entry clarity.)
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let cfg = cfg.clone();
                std::thread::spawn(move || agent::handle(stream.into(), &cfg));
            }
            Err(e) => eprintln!("forge: accept: {e}"),
        }
    }
    unreachable!("TcpListener::incoming never ends");
}
