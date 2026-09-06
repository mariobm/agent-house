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

use std::net::TcpListener;

fn main() {
    let cfg = config::Config::from_env();
    let listener = TcpListener::bind(&cfg.listen_addr).unwrap_or_else(|e| {
        eprintln!("forge: listen {}: {e}", cfg.listen_addr);
        std::process::exit(1);
    });
    eprintln!("forge: listening on {}", cfg.listen_addr);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let cfg = cfg.clone();
                std::thread::spawn(move || agent::handle(stream, &cfg));
            }
            Err(e) => eprintln!("forge: accept: {e}"),
        }
    }
}
