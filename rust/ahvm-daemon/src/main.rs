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
//! AHVM_NETD_BIN     optional managed Rust gateway binary (Linux)
//! AHVM_NETWORK_BYTES_PER_SEC optional per-VM per-direction Ethernet cap (65536..=1000000000)
//! AHVM_DNS_RESOLVER required IPv4 resolver when netd is enabled
//! AHVM_PREVIEW_LISTEN optional separate HTTP preview bind address
//! AHVM_PREVIEW_DOMAIN dedicated domain for per-port preview hosts
//! AHVM_PRIVATE_ACCESS_FILE optional owner-bound exact TCP grant JSON
//! AHVM_ADMIN_TOKEN  bootstrap admin token (created once when missing)
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ahvm_engine::Backend as _;

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
    // The store cannot create parent directories itself: a fresh data dir
    // must exist before SQLite opens the database file inside it.
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("ahvm-daemon: create {}: {e}", data_dir.display());
        std::process::exit(1);
    }
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
    let mut backend_cfg = ahvm_engine::KrucibleConfig::new(
        PathBuf::from(required("AHVM_VMM_BIN")),
        PathBuf::from(required("AHVM_BASE_IMAGE")),
        sandbox_dir,
        lib_path,
    );
    backend_cfg.resources = std::env::var_os("AHVM_CGROUP_ROOT")
        .map(|root| ahvm_engine::ResourceConfig { root: root.into() });
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Grant {
        owner_user_id: String,
        destinations: Vec<std::net::SocketAddrV4>,
    }
    let grants: std::collections::BTreeMap<String, Grant> =
        std::env::var_os("AHVM_PRIVATE_ACCESS_FILE")
            .map(|path| {
                let bytes = std::fs::read(path).expect("read private-access policy");
                serde_json::from_slice(&bytes).expect("parse private-access policy")
            })
            .unwrap_or_default();
    for (id, grant) in &grants {
        assert!(
            !grant.owner_user_id.is_empty(),
            "private grant owner is required"
        );
        match store.get_sandbox(id) {
            Ok(row) => assert_eq!(
                row.owner_user_id, grant.owner_user_id,
                "private grant owner mismatch for {id}"
            ),
            Err(ahvm_store::Error::NotFound(_)) => {}
            Err(e) => panic!("private grant ownership: {e}"),
        }
    }
    if !grants.is_empty() && std::env::var_os("AHVM_NETD_BIN").is_none() {
        panic!("private grants require managed networking");
    }
    let private_owners = Arc::new(
        grants
            .iter()
            .map(|(id, g)| (id.clone(), g.owner_user_id.clone()))
            .collect(),
    );
    if let Some(bin) = std::env::var_os("AHVM_NETD_BIN") {
        backend_cfg.network = Some(ahvm_engine::NetworkConfig {
            bandwidth_bytes_per_sec: std::env::var("AHVM_NETWORK_BYTES_PER_SEC").ok().map(|v| {
                v.parse()
                    .expect("AHVM_NETWORK_BYTES_PER_SEC must be an integer")
            }),
            private_access: grants
                .into_iter()
                .map(|(id, g)| (id, g.destinations))
                .collect(),
            netd_bin: PathBuf::from(bin),
            resolver: required("AHVM_DNS_RESOLVER").parse().unwrap_or_else(|e| {
                eprintln!("ahvm-daemon: invalid DNS resolver: {e}");
                std::process::exit(1);
            }),
        });
    }
    let backend = ahvm_engine::KrucibleBackend::open(backend_cfg).unwrap_or_else(|e| {
        eprintln!("ahvm-daemon: open backend: {e}");
        std::process::exit(1);
    });

    // Startup reconcile: backend sandboxes with no store row are pre-commit
    // orphans (a create crashed between boot and record insert). Destroy
    // them so quota accounting has no invisible consumers. Only NotFound
    // rows qualify — any other store error leaves workers alone.
    match backend.list() {
        Ok(infos) => {
            // Fresh store handle for the reconcile reads.
            let store2 = ahvm_store::Store::open(&store_path).unwrap_or_else(|e| {
                eprintln!("ahvm-daemon: reopen store: {e}");
                std::process::exit(1);
            });
            for info in infos {
                match store2.get_sandbox(&info.id) {
                    Ok(_) => {}
                    Err(ahvm_store::Error::NotFound(_)) => {
                        eprintln!("ahvm-daemon: destroying orphan worker {}", info.id);
                        let _ = backend.destroy(&info.id);
                    }
                    Err(e) => {
                        eprintln!("ahvm-daemon: orphan check {}: {e}", info.id);
                    }
                }
            }
        }
        Err(e) => eprintln!("ahvm-daemon: backend list for reconcile: {e}"),
    }

    // Previous request tasks died with the previous daemon process. Preserve
    // their receipts so a late retry cannot execute an old command again.
    store
        .interrupt_lifecycle_operations()
        .expect("recover lifecycle receipts");

    let state = ahvm_daemon::AppState {
        private_owners,
        store: Arc::new(store),
        backend: Arc::new(backend),
        quotas: ahvm_daemon::quotas::Registry::new(),
        activity: ahvm_daemon::thermal::ActivityTracker::new(),
        ops: ahvm_daemon::scheduler::OpsLimiter::new(
            std::env::var("AHVM_MAX_CONCURRENT_OPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(4),
        ),
        lifecycle: ahvm_daemon::scheduler::LifecycleLocks::new(),
    };
    // Thermal sweep (idle stop + reconcile) runs for the daemon lifetime.
    // Shutdown is process exit: activity rebuilds, records persist per-op.
    let thermal_state = state.clone();
    let thermal_cfg = ahvm_daemon::thermal::ThermalConfig {
        idle_secs: std::env::var("AHVM_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3600),
        sweep_secs: std::env::var("AHVM_SWEEP_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60),
    };
    tokio::spawn(async move { ahvm_daemon::thermal::run(thermal_state, thermal_cfg).await });
    let addr: SocketAddr = env("AHVM_LISTEN", "127.0.0.1:8080")
        .parse()
        .unwrap_or_else(|e| {
            eprintln!("ahvm-daemon: bad AHVM_LISTEN: {e}");
            std::process::exit(1);
        });
    if let Ok(preview_addr) = std::env::var("AHVM_PREVIEW_LISTEN") {
        let domain = required("AHVM_PREVIEW_DOMAIN");
        let listener = tokio::net::TcpListener::bind(&preview_addr)
            .await
            .expect("bind preview listener");
        let app =
            ahvm_daemon::previews::router(state.clone(), &domain).expect("invalid preview domain");
        eprintln!("ahvm-daemon: preview listener {preview_addr}, domain {domain}");
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                eprintln!("preview listener: {e}");
            }
        });
    }
    eprintln!("ahvm-daemon: listening on {addr}");
    use axum::serve::ListenerExt;
    axum::serve(
        tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                eprintln!("ahvm-daemon: bind {addr}: {e}");
                std::process::exit(1);
            })
            .tap_io(|stream| {
                // Interactive WebSocket frames should leave immediately.
                if let Err(error) = stream.set_nodelay(true) {
                    eprintln!("ahvm-daemon: TCP_NODELAY: {error}");
                }
            }),
        ahvm_daemon::build_router(state),
    )
    .await
    .unwrap_or_else(|e| {
        eprintln!("ahvm-daemon: serve: {e}");
        std::process::exit(1);
    });
}
