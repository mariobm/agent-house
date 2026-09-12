//! Opt-in isolated engine gate. The qualification service owns one NBD device.
#![cfg(target_os = "linux")]
use ahvm_engine::{
    Backend, KrucibleBackend, KrucibleConfig, ReplicatedConfig, State, StorageMode, Worker,
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[test]
fn replicated_engine_lifecycle() {
    if std::env::var("AHVM_REPLICATED_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: set AHVM_REPLICATED_TEST=1 with the isolated volume service");
        return;
    }
    let env = |key| std::env::var(key).expect(key);
    let data = PathBuf::from(env("AHVM_REPLICATED_DATA"));
    assert!(!data.exists(), "use a fresh test tree");
    let mut cfg = KrucibleConfig::new(
        env("AHVM_VMM_BIN").into(),
        env("AHVM_GUEST_IMAGE").into(),
        data.clone(),
        env("LD_LIBRARY_PATH"),
    );
    cfg.replicated = Some(ReplicatedConfig {
        socket: env("AHVM_VOLUME_SOCKET").into(),
    });
    cfg.ready_timeout = Duration::from_secs(120);
    cfg.default_storage_mode = StorageMode::Replicated;
    let spec = serde_json::from_value(
        serde_json::json!({"name":"replica","backend":"krucible","cpus":1,"memory_mb":1024}),
    )
    .unwrap();
    let be = KrucibleBackend::open(cfg.clone()).unwrap();
    let info = be.create(&spec).unwrap();
    assert_eq!(info.storage.mode, StorageMode::Replicated);
    let volume = info.storage.volume_id.clone();
    println!("Fixture volume: {}", volume.as_deref().unwrap());
    assert!(!data.join("replica/root.qcow2").exists());
    assert!(!be.sandbox_capabilities("replica").unwrap().typed_snapshots);
    assert!(be.create_snapshot("replica", "nope").is_err());
    assert!(be.fork("replica", "nope").is_err());
    let command = |be: &KrucibleBackend, s: &str| {
        let result = be
            .exec("replica", &["/bin/sh".into(), "-c".into(), s.into()])
            .unwrap();
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        result.stdout
    };
    command(&be, "printf persisted >/root/engine-proof; sync");
    be.stop("replica").unwrap();
    assert_eq!(be.status("replica").unwrap().state, State::Stopped);
    assert!(!data.join("replica/bundle").exists());
    assert_eq!(be.sync_remote("replica").unwrap().pending_bytes, 0);
    be.start("replica").unwrap();
    assert_eq!(command(&be, "cat /root/engine-proof"), "persisted");
    let worker = Worker::load(data.join("replica/state.json")).unwrap();
    // This is the just-created owned test child; simulate abrupt worker loss.
    #[allow(unsafe_code)]
    unsafe {
        assert_eq!(libc::kill(worker.pid as i32, libc::SIGKILL), 0);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while be.status("replica").unwrap().state != State::Failed {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    be.start("replica").unwrap();
    assert_eq!(command(&be, "cat /root/engine-proof"), "persisted");
    let pid = Worker::load(data.join("replica/state.json")).unwrap().pid;
    drop(be); // workers and storage service survive engine restart
    cfg.default_storage_mode = StorageMode::Local;
    let be = KrucibleBackend::open(cfg).unwrap();
    let info = be.status("replica").unwrap();
    assert_eq!(info.state, State::Running);
    assert_eq!(info.storage.volume_id, volume);
    assert_eq!(
        Worker::load(data.join("replica/state.json")).unwrap().pid,
        pid
    );
    assert_eq!(command(&be, "cat /root/engine-proof"), "persisted");
    be.destroy("replica").unwrap();
    assert!(be.list().unwrap().is_empty());
    assert!(!data.join("replica").exists());
    std::fs::remove_dir_all(&data).unwrap();
    println!("PASS replicated create/exec/stop/start/crash/adoption/destroy");
}
