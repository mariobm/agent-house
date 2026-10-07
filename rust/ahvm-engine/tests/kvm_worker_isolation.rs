//! One disposable guest at a time: custom-image snapshots, raw disks and inherited FDs.
#![cfg(target_os = "linux")]
use ahvm_engine::{
    Backend, KrucibleBackend, KrucibleConfig, LiveWorker, SpawnConfig, WorkerSandbox,
};
use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
use std::{
    fs,
    io::BufReader,
    os::{fd::AsRawFd, unix::net::UnixStream},
    path::PathBuf,
    time::{Duration, Instant},
};

struct Guests {
    backend: KrucibleBackend,
}
impl Drop for Guests {
    fn drop(&mut self) {
        for id in ["original", "restored"] {
            let _ = self.backend.destroy(id);
        }
    }
}
struct Raw(LiveWorker);
impl Drop for Raw {
    fn drop(&mut self) {
        let _ = self.0.terminate();
    }
}

#[test]
fn custom_snapshot_raw_disk_and_descriptor_isolation() {
    if std::env::var("AHVM_KVM_WORKER_TEST").as_deref() != Ok("1") {
        return;
    }
    let value = |key| std::env::var(key).expect(key);
    let work = PathBuf::from(value("AHVM_WORKER_TEST_DIR"));
    assert!(!work.exists(), "requires a fresh disposable directory");
    fs::create_dir_all(work.join("images")).unwrap();
    let base = PathBuf::from(value("AHVM_GUEST_IMAGE"));
    let custom = work.join("images/custom.ext4");
    fs::copy(&base, &custom).unwrap();
    let mut cfg = KrucibleConfig::new(
        value("AHVM_VMM_BIN").into(),
        base.clone(),
        work.join("data"),
        value("LD_LIBRARY_PATH"),
    );
    cfg.ready_timeout = Duration::from_secs(30);
    cfg.resources = std::env::var_os("AHVM_WORKER_CGROUP_ROOT")
        .map(|root| ahvm_engine::ResourceConfig { root: root.into() });
    let guests = Guests {
        backend: KrucibleBackend::open(cfg.clone()).unwrap(),
    };
    let spec = serde_json::from_value(serde_json::json!({"name":"original","cpus":1,"memory_mb":256,"backend":"krucible","root_image":custom})).unwrap();
    guests.backend.create(&spec).unwrap();
    let exec = |id, script: &str| {
        let result = guests
            .backend
            .exec(id, &["/bin/sh".into(), "-ec".into(), script.into()])
            .unwrap();
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
        result.stdout
    };
    assert_eq!(
        exec(
            "original",
            "printf persisted >/workspace/proof; sync; printf CUSTOM-OK"
        ),
        "CUSTOM-OK"
    );
    let mut raw_spec: serde_json::Value =
        serde_json::from_slice(&fs::read(cfg.data_dir.join("original/spec.json")).unwrap())
            .unwrap();
    let snapshot = guests
        .backend
        .create_snapshot("original", "custom-image")
        .unwrap();
    guests.backend.stop("original").unwrap();
    guests.backend.restore(&snapshot, "restored").unwrap();
    assert_eq!(exec("restored", "cat /workspace/proof"), "persisted");
    guests.backend.stop("restored").unwrap();
    guests.backend.start("restored").unwrap();
    assert_eq!(exec("restored", "cat /workspace/proof"), "persisted");
    guests.backend.stop("restored").unwrap();
    println!("PASS custom-image snapshot, restore-as-new and second stop/start");

    let raw_dir = work.join("raw");
    for name in ["sock", "tmp", "runtime"] {
        fs::create_dir_all(raw_dir.join(name)).unwrap();
    }
    let disk = raw_dir.join("root.ext4");
    fs::copy(base, &disk).unwrap();
    let mut policy: WorkerSandbox =
        serde_json::from_value(raw_spec["worker_sandbox"].take()).unwrap();
    policy
        .read_only
        .retain(|path| path != &cfg.data_dir.join("original") && path != &custom);
    policy.read_only.push(raw_dir.clone());
    policy.read_write = [
        disk.clone(),
        raw_dir.join("sock"),
        raw_dir.join("tmp"),
        raw_dir.join("runtime"),
        "/dev/kvm".into(),
        "/dev/null".into(),
        "/dev/urandom".into(),
    ]
    .into();
    raw_spec["worker_sandbox"] = serde_json::to_value(policy).unwrap();
    raw_spec["root_disk"] = disk.to_string_lossy().into();
    raw_spec["root_disk_format"] = "raw".into();
    raw_spec["vsock_control_uds"] = raw_dir.join("sock/c.sock").to_string_lossy().into();
    raw_spec["vsock_forward_uds"] = raw_dir.join("sock/f.sock").to_string_lossy().into();
    raw_spec["control_socket_uds"] = raw_dir.join("sock/control.sock").to_string_lossy().into();
    let path = raw_dir.join("spec.json");
    fs::write(&path, serde_json::to_vec(&raw_spec).unwrap()).unwrap();
    let secret = work.join("synthetic-inherited-secret");
    fs::write(&secret, b"must-not-reach-worker").unwrap();
    let inherited = fs::File::open(&secret).unwrap();
    // Deliberately leak this synthetic file across exec; the worker must close it.
    #[allow(unsafe_code)]
    unsafe {
        assert_eq!(libc::fcntl(inherited.as_raw_fd(), libc::F_SETFD, 0), 0);
    }
    let group = cfg
        .resources
        .as_ref()
        .map(|resource| resource.root.join("vm-raw"));
    if let Some(group) = &group {
        fs::create_dir(group).unwrap();
        for (name, value) in [
            ("cpu.max", "100000 100000"),
            ("memory.max", "805306368"),
            ("memory.swap.max", "0"),
            ("pids.max", "256"),
        ] {
            fs::write(group.join(name), value).unwrap();
        }
    }
    let env = vec![
        format!("LD_LIBRARY_PATH={}", cfg.lib_path),
        format!("HOME={}", raw_dir.join("runtime").display()),
        format!("TMPDIR={}", raw_dir.join("tmp").display()),
    ];
    let worker = Raw(ahvm_engine::spawn_worker_cfg(&SpawnConfig {
        cgroup: group.as_deref(),
        vmm_binary: cfg.vmm_bin.as_os_str(),
        spec_arg: &path,
        state_path: &raw_dir.join("state.json"),
        hermetic: true,
        env: &env,
        stderr_log: Some(&raw_dir.join("vmm.log")),
    })
    .unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let output = loop {
        assert!(
            Instant::now() < deadline,
            "raw guest never answered; inspect {}",
            raw_dir.join("vmm.log").display()
        );
        if let Ok(mut socket) = UnixStream::connect(raw_dir.join("sock/c.sock")) {
            socket
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let request = Frame { msg_type: FrameType::ExecReq, payload: br#"{"argv":["/bin/sh","-ec","printf RAW-OK > /workspace/raw; sync; cat /workspace/raw"]}"#.to_vec() };
            if write_frame(&mut socket, &request).is_ok() {
                if let Ok(response) = read_frame(&mut BufReader::new(socket)) {
                    break response;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(output.msg_type, FrameType::ExecResp);
    let response: serde_json::Value = serde_json::from_slice(&output.payload).unwrap();
    assert_eq!(response["exit_code"], 0);
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(response["stdout_b64"].as_str().unwrap())
            .unwrap(),
        b"RAW-OK"
    );
    for fd in fs::read_dir(format!("/proc/{}/fd", worker.0.pid())).unwrap() {
        assert_ne!(
            fs::read_link(fd.unwrap().path()).ok().as_ref(),
            Some(&secret)
        );
    }
    println!(
        "PASS raw disk read/write/sync and inherited descriptor closure; {}",
        fs::read_to_string(raw_dir.join("vmm.log"))
            .unwrap()
            .lines()
            .next()
            .unwrap()
    );
    drop(worker);
    if let Some(group) = group {
        fs::remove_dir(group).unwrap();
    }
}
