//! ahvm-daemon binary: env config, store + backend wiring, serve.
//!
//! Environment:
//!
//! ```text
//! AHVM_LISTEN       bind address (default 127.0.0.1:8080)
//! AHVM_DATA_DIR     daemon state (default ./data; store + sandboxes below)
//! AHVM_VMM_BIN      release ahvm-vmm (required)
//! AHVM_BASE_IMAGE   backing guest ext4 (required)
//! AHVM_LIB          LD_LIBRARY_PATH value for workers (default: inherited)
//! AHVM_ADMIN_TOKEN  bootstrap admin token (created once when missing)
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn required(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("ahvm-daemon: {key} is required");
        std::process::exit(1);
    })
}

#[tokio::main]
async fn main() {
    let data_dir = PathBuf::from(env("AHVM_DATA_DIR", "./data"));
    let store_path = data_dir.join("daemon.db");
    let sandbox_dir = data_dir.join("sandboxes");

    let store = ahvm_store::Store::open(&store_path).unwrap_or_else(|e| {
        eprintln!("ahvm-daemon: open store {}: {e}", store_path.display());
        std::process::exit(1);
    });
    if let Ok(token) = std::env::var("AHVM_ADMIN_TOKEN") {
        match ahvm_daemon::auth::ensure_admin(&store, &token) {
            Ok(true) => eprintln!("ahvm-daemon: bootstrapped admin user"),
            Ok(false) => {}
            Err(e) => {
                eprintln!("ahvm-daemon: admin bootstrap: {e}");
                std::process::exit(1);
            }
        }
    }

    let lib_path = std::env::var("AHVM_LIB")
        .or_else(|_| std::env::var("LD_LIBRARY_PATH"))
        .unwrap_or_default();
    let backend = ahvm_engine::KrucibleBackend::open(ahvm_engine::KrucibleConfig::new(
        PathBuf::from(required("AHVM_VMM_BIN")),
        PathBuf::from(required("AHVM_BASE_IMAGE")),
        sandbox_dir,
        lib_path,
    ))
    .unwrap_or_else(|e| {
        eprintln!("ahvm-daemon: open backend: {e}");
        std::process::exit(1);
    });

    let state = ahvm_daemon::AppState {
        store: Arc::new(store),
        backend: Arc::new(backend),
    };
    let addr: SocketAddr = env("AHVM_LISTEN", "127.0.0.1:8080")
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("ahvm-daemon: bad AHVM_LISTEN: {e}");
            std::process::exit(1);
        });
    eprintln!("ahvm-daemon: listening on {addr}");
    axum::serve(
        tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!("ahvm-daemon: bind {addr}: {e}");
                std::process::exit(1);
            }),
        ahvm_daemon::build_router(state),
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("ahvm-daemon: serve: {e}");
        std::process::exit(1);
    });
}
