//! Opt-in Linux/KVM gate: synced files survive SIGKILL before any checkpoint.
#![cfg(target_os = "linux")]

use ahvm_engine::{Backend, KrucibleBackend, KrucibleConfig, SandboxSpec, State, Worker};
use rustix::process::{pidfd_open, pidfd_send_signal, Pid, PidfdFlags, Signal};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Fixture {
    backend: KrucibleBackend,
    data: PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = self.backend.destroy("crash-disk") {
            eprintln!("crash-disk cleanup failed: {error}");
            return;
        }
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

#[test]
fn synced_disk_survives_repeated_sigkill_without_checkpoint() {
    if std::env::var("AHVM_KVM_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP kvm_crash_disk: set AHVM_KVM_TEST=1 with VMM/image/lib paths");
        return;
    }
    let data = std::env::temp_dir().join(format!("ahvm-crash-disk-{}", std::process::id()));
    assert!(!data.exists(), "refusing to reuse prior test state");
    let cfg = KrucibleConfig::new(
        std::env::var_os("AHVM_VMM_BIN")
            .expect("AHVM_VMM_BIN")
            .into(),
        std::env::var_os("AHVM_GUEST_IMAGE")
            .expect("AHVM_GUEST_IMAGE")
            .into(),
        data.clone(),
        std::env::var("LD_LIBRARY_PATH").expect("LD_LIBRARY_PATH"),
    );
    let fixture = Fixture {
        backend: KrucibleBackend::open(cfg).unwrap(),
        data,
    };
    let be = &fixture.backend;
    let spec: SandboxSpec = serde_json::from_value(serde_json::json!({
        "name": "crash-disk", "cpus": 1, "memory_mb": 1024, "backend": "krucible"
    }))
    .unwrap();
    be.create(&spec).unwrap();
    for cycle in 0..5 {
        let command = format!("printf durable-{cycle} > /tmp/ahvm-crash-{cycle}; sync");
        let result = be
            .exec("crash-disk", &["/bin/sh".into(), "-c".into(), command])
            .unwrap();
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert!(!fixture
            .data
            .join("crash-disk/bundle/manifest.json")
            .exists());
        let record = Worker::load(fixture.data.join("crash-disk/state.json")).unwrap();
        let fd = pidfd_open(
            Pid::from_raw(record.pid as i32).unwrap(),
            PidfdFlags::empty(),
        )
        .unwrap();
        assert_eq!(ahvm_engine::process_starttime(record.pid), record.starttime);
        pidfd_send_signal(&fd, Signal::KILL).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while be.status("crash-disk").unwrap().state != State::Failed {
            assert!(Instant::now() < deadline, "worker death was never observed");
            std::thread::sleep(Duration::from_millis(10));
        }
        be.start("crash-disk").unwrap();
        for saved in 0..=cycle {
            let result = be
                .exec(
                    "crash-disk",
                    &["/bin/cat".into(), format!("/tmp/ahvm-crash-{saved}")],
                )
                .unwrap();
            assert_eq!(result.exit_code, 0, "{result:?}");
            assert_eq!(result.stdout, format!("durable-{saved}"));
        }
    }
}
