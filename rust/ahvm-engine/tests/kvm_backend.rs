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
use std::time::{Duration, Instant};

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
        storage_mode: None,
        name: name.to_string(),
        cpus: 1,
        memory_mb: 512,
        backend: ahvm_engine::BackendKind::Krucible,
        root_image: None,
        kernel_image: None,
        desktop: false,
        desktop_gpu: false,
        network_bytes_per_sec: None,
        extra_env: Default::default(),
    }
}

fn sh(cmd: &str) -> Vec<String> {
    vec!["/bin/sh".to_string(), "-c".to_string(), cmd.to_string()]
}

// SAFETY: kill(pid, SIGKILL) performs no action on the caller; used to
// simulate worker death behind the backend's back.
#[allow(unsafe_code)]
fn sigkill(pid: i32) {
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
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

    // A live session emits output and stays open. Interactive reads must
    // return the marker before EOF/the two-second guest timeout.
    let live = be
        .session_create(&info.id, &sh("printf ready; sleep 5"), false)
        .unwrap();
    let began = Instant::now();
    let chunk = be
        .session_poll(&info.id, &live, 0, Duration::from_secs(3))
        .unwrap();
    assert_eq!(chunk.data, b"ready");
    assert!(!chunk.eof);
    assert_eq!(chunk.next_seq, 5);
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "output was buffered until the drain deadline"
    );
    be.session_delete(&info.id, &live).unwrap();

    // Slow exec: a valid reply arriving after the old 15s readiness
    // timeout must be waited out, not abandoned (forge allows 300s).
    let r = be.exec(&info.id, &sh("sleep 16; printf slow")).unwrap();
    assert_eq!(r.exit_code, 0);
    assert_eq!(r.stdout, "slow");

    // Worker death behind the backend's back: start() must reboot,
    // not trust the cached Running state. SIGKILL is asynchronous, so
    // first wait until the backend OBSERVES the death (status reconciles
    // via try_reap): calling start() in the microseconds between kill()
    // and actual death is indistinguishable from alive for ANY supervisor
    // (kill/reap race), and start() correctly reports Ok in that window.
    let data = std::env::temp_dir().join(format!("ahvm-kvm-be-{}", std::process::id()));
    let state_raw = std::fs::read(data.join("be-1").join("state.json")).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&state_raw).unwrap();
    sigkill(state["pid"].as_u64().unwrap() as i32);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if be.status(&info.id).unwrap().state == State::Failed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "backend never observed the SIGKILLed worker's death"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    be.start(&info.id).unwrap();
    let r = be.exec(&info.id, &sh("printf rebooted")).unwrap();
    assert_eq!(r.stdout, "rebooted");

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

    // ---- files + sessions through the backend ----
    assert_eq!(
        be.file_write("be-3", "/workspace/be-proof", b"FILEVAL")
            .unwrap(),
        7
    );
    let c = be.file_read("be-3", "/workspace/be-proof", 0, 100).unwrap();
    assert_eq!(c.data, b"FILEVAL");
    assert!(c.eof);
    let l = be.file_list("be-3", "/workspace", 0, 100).unwrap();
    assert!(l.entries.iter().any(|e| e.name == "be-proof" && !e.is_dir));
    let sid = be
        .session_create("be-3", &sh("echo SESSMARKER"), false)
        .unwrap();
    let chunk = be
        .session_read("be-3", &sid, 0, Duration::from_secs(20))
        .unwrap();
    assert!(
        chunk
            .data
            .windows(b"SESSMARKER\n".len())
            .any(|w| w == b"SESSMARKER\n"),
        "no marker: {:?}",
        String::from_utf8_lossy(&chunk.data)
    );
    assert!(chunk.eof, "quick session should EOF");
    assert!(!chunk.truncated);
    assert_eq!(chunk.next_seq as usize, chunk.data.len());
    let list = be.session_list("be-3").unwrap();
    assert!(list.iter().any(|s| s.id == sid));
    // Kill on an already-exited session is a guest error, not a silent
    // no-op — delete it directly.
    assert!(be.session_kill("be-3", &sid).is_err());
    be.session_delete("be-3", &sid).unwrap();
    assert!(be.session_list("be-3").unwrap().iter().all(|s| s.id != sid));

    // Idle attach hygiene (review P1): repeated short-budget reads must
    // not accumulate guest threads — each forge attach self-closes.
    // The guest image ships without /proc mounted; mount it (idempotent,
    // one exec) so the Threads counter is readable.
    let threads = || -> usize {
        let out = be
            .exec(
                "be-3",
                &sh("mkdir -p /proc; mount -t proc proc /proc 2>/dev/null; grep Threads /proc/1/status"),
            )
            .unwrap()
            .stdout;
        out.split_whitespace()
            .nth(1)
            .and_then(|n| n.parse().ok())
            .expect("Threads line")
    };
    let idle = be.session_create("be-3", &sh("sleep 120"), false).unwrap();
    let before = threads();
    for _ in 0..12 {
        let c = be
            .session_read("be-3", &idle, 0, Duration::from_secs(1))
            .unwrap();
        assert!(!c.eof);
    }
    let after = threads();
    assert!(
        after <= before + 1,
        "guest threads leaked: {before} -> {after}"
    );
    be.session_kill("be-3", &idle).unwrap();
    be.session_delete("be-3", &idle).unwrap();

    // ---- supervisor restart: drop the backend (workers survive:
    // LiveWorker Drop never kills a running child), reopen, adopt ----
    let data = {
        // Re-derive the data dir from a known record is overkill: the
        // temp dir embeds our pid, which is stable within this test.
        std::env::temp_dir().join(format!("ahvm-kvm-be-{}", std::process::id()))
    };
    // Simulate a daemon dying during snapshot/stop after PAUSE. Adoption
    // must resume the survivor before reporting it as Running.
    assert_eq!(
        ahvm_engine::send_ctl(data.join("be-1/sock/control.sock"), "PAUSE").unwrap(),
        "OK paused"
    );
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
