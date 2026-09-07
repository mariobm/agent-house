//! KVM gate for the real krucible [`Backend`](ahvm_engine::Backend):
//! create -> exec -> stop -> start -> snapshot -> restore -> fork ->
//! destroy, plus supervisor-restart recovery by pid adoption.
//!
//! Same gating as `kvm_lifecycle`: runs ONLY with `AHVM_KVM_TEST=1`,
//! `/dev/kvm`, `AHVM_VMM_BIN` and `AHVM_GUEST_IMAGE` set; otherwise a
//! loud skip so plain `cargo test` stays green without KVM.

#![cfg(unix)]

use ahvm_engine::{Backend, KrucibleBackend, KrucibleConfig, State, Thermal};
use std::path::PathBuf;
use std::time::Duration;

fn gated() -> Option<KrucibleBackend> {
    if std::env::var("AHVM_KVM_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP kvm_backend: set AHVM_KVM_TEST=1 on a KVM host");
        return None;
    }
    assert!(
        std::path::Path::new("/dev/kvm").exists(),
        "AHVM_KVM_TEST=1 requires /dev/kvm"
    );
    let vmm = PathBuf::from(std::env::var("AHVM_VMM_BIN").expect("AHVM_VMM_BIN required"));
    let image =
        PathBuf::from(std::env::var("AHVM_GUEST_IMAGE").expect("AHVM_GUEST_IMAGE required"));
    assert!(vmm.is_file(), "AHVM_VMM_BIN missing: {}", vmm.display());
    assert!(
        image.is_file(),
        "AHVM_GUEST_IMAGE missing: {}",
        image.display()
    );
    let lib_path = std::env::var("LD_LIBRARY_PATH").expect("LD_LIBRARY_PATH for libkrunfw");
    let data = std::env::temp_dir().join(format!("ahvm-kvm-be-{}", std::process::id()));
    let mut cfg = KrucibleConfig::new(vmm, image, data, lib_path);
    cfg.ready_timeout = Duration::from_secs(60);
    Some(KrucibleBackend::open(cfg).unwrap())
}

fn spec(name: &str) -> ahvm_engine::SandboxSpec {
    ahvm_engine::SandboxSpec {
        name: name.to_string(),
        cpus: 1,
        memory_mb: 512,
        backend: ahvm_engine::BackendKind::Krucible,
        root_image: None,
        kernel_image: None,
        extra_env: Default::default(),
    }
}

fn sh(cmd: &str) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        cmd.to_string(),
    ]
}

#[test]
fn kvm_backend_lifecycle_and_recovery() {
    let Some(be) = gated() else { return };

    // ---- create + exec ----
    let info = be.create(&spec("be-1")).unwrap();
    assert_eq!(info.state, State::Running);
    assert_eq!(info.thermal, Thermal::Hot);
    let r = be.exec(&info.id, &sh("printf hello")).unwrap();
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.stdout, "hello");
    assert!(!r.truncated);

    // ---- stop: exec refuses, state goes Cold ----
    be.stop(&info.id).unwrap();
    let stopped = be.status(&info.id).unwrap();
    assert_eq!(stopped.state, State::Stopped);
    assert_eq!(stopped.thermal, Thermal::Cold);
    assert!(be.exec(&info.id, &sh("true")).is_err());

    // ---- start: restores from the stop bundle (Warm) ----
    be.start(&info.id).unwrap();
    let running = be.status(&info.id).unwrap();
    assert_eq!(running.state, State::Running);
    assert_eq!(running.thermal, Thermal::Warm);
    let r = be.exec(&info.id, &sh("printf back")).unwrap();
    assert_eq!(r.stdout, "back");

    // ---- typed snapshot + restore-as-new ----
    let snap = be.create_snapshot(&info.id, "snap-a").unwrap();
    assert_eq!(snap.snapshot_id, "snap-a");
    let r2 = be.restore(&snap, "be-2").unwrap();
    assert_eq!(r2.state, State::Running);
    assert_eq!(r2.thermal, Thermal::Warm);
    let r = be.exec("be-2", &sh("printf restored")).unwrap();
    assert_eq!(r.stdout, "restored");

    // ---- fork: live clone answers too ----
    let f = be.fork(&info.id, "be-3").unwrap();
    assert_eq!(f.state, State::Running);
    let r = be.exec("be-3", &sh("printf forked")).unwrap();
    assert_eq!(r.stdout, "forked");

    // ---- supervisor restart: drop the backend (workers survive:
    // LiveWorker Drop never kills a running child), reopen, adopt ----
    let data = {
        // Re-derive the data dir from a known record is overkill: the
        // temp dir embeds our pid, which is stable within this test.
        std::env::temp_dir().join(format!("ahvm-kvm-be-{}", std::process::id()))
    };
    drop(be);
    let lib_path = std::env::var("LD_LIBRARY_PATH").unwrap();
    let vmm = PathBuf::from(std::env::var("AHVM_VMM_BIN").unwrap());
    let image = PathBuf::from(std::env::var("AHVM_GUEST_IMAGE").unwrap());
    let be2 = KrucibleBackend::open(KrucibleConfig::new(vmm, image, data, lib_path)).unwrap();
    // Adopted workers answer without any respawn.
    assert_eq!(be2.status("be-1").unwrap().state, State::Running);
    let r = be2.exec("be-1", &sh("printf adopted")).unwrap();
    assert_eq!(r.stdout, "adopted");

    // ---- destroy everything (adopted destroy uses the SIGTERM path) ----
    for id in ["be-1", "be-2", "be-3"] {
        be2.destroy(id).unwrap();
        assert!(matches!(
            be2.status(id),
            Err(ahvm_engine::Error::NotFound(_))
        ));
    }
    assert!(be2.list().unwrap().is_empty());
    assert!(matches!(
        be2.destroy("be-1"),
        Err(ahvm_engine::Error::NotFound(_))
    ));
}
