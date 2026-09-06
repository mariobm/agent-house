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
mod sessions;
mod transport;

use std::net::TcpListener;

fn main() {
    let cfg = config::Config::from_env();
    // Vsock rides alongside TCP (guest VMM path); a runtime failure there
    // must never take TCP down — serve_vsock logs and returns instead, so
    // tests/dev on hosts without vsock keep working unchanged.
    if cfg.vsock_port > 0 {
        let vsock_cfg = cfg.clone();
        std::thread::spawn(move || transport::serve_vsock(vsock_cfg.vsock_port, &vsock_cfg));
    }
    let listener = TcpListener::bind(&cfg.listen_addr).unwrap_or_else(|e| {
        eprintln!("forge: listen {}: {e}", cfg.listen_addr);
        std::process::exit(1);
    });
    // Report the BOUND address (differs from config when :0 was given) —
    // integration tests parse this line to find us without port probing.
    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or(cfg.listen_addr.clone());
    eprintln!("forge: listening on {bound}");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let cfg = cfg.clone();
                std::thread::spawn(move || agent::handle(stream.into(), &cfg));
            }
            Err(e) => eprintln!("forge: accept: {e}"),
        }
    }
}
