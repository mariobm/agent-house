//! KVM lifecycle gate: boot -> exec -> snapshot -> kill -> restore -> exec.
//!
//! Gated: runs ONLY when `AHVM_KVM_TEST=1` with `/dev/kvm` present and
//! `AHVM_VMM_BIN` (release ahvm-vmm) + `AHVM_GUEST_IMAGE` (ext4 with Rust
//! forge as /init.krun + busybox) set. Everywhere else this is a loud skip,
//! so plain `cargo test` stays green without KVM.
//!
//! What it proves (the review's item-3 plan):
//! - `/bin/sh -c 'printf hello'` -> exit 0, exactly `hello`; stderr and
//!   nonzero-exit cases behave.
//! - Snapshot bundle gets the engine sidecar (`ahvm-manifest.json`) next to
//!   libkrun's own `manifest.json`; restore is compat-gated (plus a tampered
//!   negative case).
//! - Repeated SIGKILL / fresh-worker restores preserve session memory and
//!   roll the disk back from an independently copied root delta.

#![cfg(unix)]

use ahvm_engine::{host_caps, send_ctl, SnapshotManifest};
use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Readiness budget, overridable for slow hosts:
/// `AHVM_KVM_READY_SECS=600 cargo test ...`. The default suits fast iron;
/// shared/loaded CI workers need more. Always prefer fixing slowness over
/// raising this — a rising budget here once caught a real boot hang.
fn ready_timeout() -> Duration {
    std::env::var("AHVM_KVM_READY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(READY_TIMEOUT)
}
const VMM_VERSION: &str = "libkrun-2.0.0-dev";

fn test_vcpus() -> u8 {
    std::env::var("AHVM_KVM_VCPUS")
        .map(|v| v.parse().expect("AHVM_KVM_VCPUS must be u8"))
        .unwrap_or(1)
}

struct Cfg {
    vmm: PathBuf,
    image: PathBuf,
    work: PathBuf,
}

fn gated() -> Option<Cfg> {
    if std::env::var("AHVM_KVM_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP kvm_lifecycle: set AHVM_KVM_TEST=1 on a KVM host");
        return None;
    }
    assert!(
        Path::new("/dev/kvm").exists(),
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
    let work = std::env::temp_dir().join(format!("ahvm-kvm-{}", std::process::id()));
    std::fs::create_dir_all(work.join("sock")).unwrap();
    Some(Cfg { vmm, image, work })
}

fn base_image_bytes(image: &Path) -> u64 {
    std::fs::metadata(image).unwrap().len()
}

fn write_spec(path: &Path, image: &Path, sock: &Path, snapshot_dir: Option<&Path>) {
    let mut spec = serde_json::json!({
        "vcpus": test_vcpus(),
        "mem_mib": 512,
        "log_level": 3,
        "root_disk": image.to_string_lossy(),
        "root_disk_format": "qcow2",
        "pid1": true,
        "exec_path": "/init.krun",
        "vsock_control_uds": sock.join("c.sock").to_string_lossy(),
        "vsock_forward_uds": sock.join("f.sock").to_string_lossy(),
        "control_socket_uds": sock.join("k.sock").to_string_lossy(),
        "env": [],
    });
    // Omit (never null): an explicit null is a parse error for String fields.
    if let Some(dir) = snapshot_dir {
        spec["snapshot_dir"] = serde_json::Value::String(dir.to_string_lossy().into_owned());
    }
    std::fs::write(path, serde_json::to_vec_pretty(&spec).unwrap()).unwrap();
}

struct Guest {
    child: Child,
    sock: PathBuf,
}

impl Guest {
    fn boot(cfg: &Cfg, image: &Path, snapshot_dir: Option<&Path>) -> Self {
        let sock = cfg.work.join("sock");
        for stale in ["c.sock", "f.sock", "k.sock"] {
            let _ = std::fs::remove_file(sock.join(stale));
        }
        let spec = cfg.work.join("spec.json");
        write_spec(&spec, image, &sock, snapshot_dir);
        // Keep worker environment independent of the invoking test runner.
        let ld_path = std::env::var("LD_LIBRARY_PATH").expect("LD_LIBRARY_PATH for libkrunfw");
        // Worker stderr goes to a log file (NOT null): a dead-on-arrival
        // worker must leave evidence instead of failing silently.
        let log_name = if snapshot_dir.is_some() {
            "restore.log"
        } else {
            "boot.log"
        };
        let log = std::fs::File::create(cfg.work.join(log_name)).unwrap();
        let console = log.try_clone().unwrap();
        let child = Command::new(&cfg.vmm)
            .arg(&spec)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("HOME", "/root")
            .env("LANG", "C.UTF-8")
            .env("LD_LIBRARY_PATH", ld_path)
            .stdin(Stdio::null())
            .stdout(console)
            .stderr(log)
            .spawn()
            .expect("spawn ahvm-vmm");
        Self { child, sock }
    }

    fn conn(&self) -> UnixStream {
        let deadline = Instant::now() + ready_timeout();
        loop {
            match UnixStream::connect(self.sock.join("c.sock")) {
                Ok(c) => {
                    c.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
                    c.set_write_timeout(Some(Duration::from_secs(15))).unwrap();
                    return c;
                }
                Err(e) => {
                    if Instant::now() > deadline {
                        panic!("bridge UDS never accepted: {e}");
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    fn rpc(&self, msg_type: FrameType, payload: Vec<u8>) -> Frame {
        let mut c = self.conn();
        write_frame(&mut c, &Frame { msg_type, payload }).unwrap();
        let mut r = BufReader::new(c.try_clone().unwrap());
        read_frame(&mut r).unwrap()
    }

    fn exec(&self, argv: &[&str]) -> serde_json::Value {
        let body = serde_json::json!({ "argv": argv }).to_string().into_bytes();
        let f = self.rpc(FrameType::ExecReq, body);
        assert_eq!(f.msg_type, FrameType::ExecResp, "want ExecResp");
        serde_json::from_slice(&f.payload).unwrap()
    }

    /// Poll `true` until the agent answers or the budget runs out.
    fn wait_ready(&self) {
        let deadline = Instant::now() + ready_timeout();
        loop {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.exec(&["/bin/true"])
            })) {
                Ok(v) if v["exit_code"] == 0 => return,
                _ => {
                    if Instant::now() > deadline {
                        panic!("agent not ready within {:?}", ready_timeout());
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }

    fn session_create(&self, argv: &[&str]) -> String {
        let body = serde_json::json!({ "op": "create", "argv": argv })
            .to_string()
            .into_bytes();
        let f = self.rpc(FrameType::SessionReq, body);
        assert_eq!(f.msg_type, FrameType::SessionResp, "want SessionResp");
        let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
        v["session_id"].as_str().unwrap().to_owned()
    }

    /// Attach from `seq`, collecting until `needle` is seen or the budget
    /// runs out. Returns (bytes, found). NOTE: must NOT wait for EOF — the
    /// marker session stays alive (sleep) across snapshot cycles by design,
    /// so EOF may never come; waiting for it hits the socket read timeout.
    fn session_drain_until(
        &self,
        id: &str,
        from_seq: u64,
        needle: &[u8],
        budget: Duration,
    ) -> (Vec<u8>, bool) {
        let body = serde_json::json!({ "op": "attach", "session_id": id, "from_seq": from_seq })
            .to_string()
            .into_bytes();
        let mut c = self.conn();
        write_frame(
            &mut c,
            &Frame {
                msg_type: FrameType::SessionReq,
                payload: body,
            },
        )
        .unwrap();
        let mut r = BufReader::new(c.try_clone().unwrap());
        let mut out = Vec::new();
        let deadline = Instant::now() + budget;
        loop {
            if Instant::now() > deadline {
                return (out, false);
            }
            // Silence (incl. socket read timeout) means "marker absent":
            // return, don't panic — the caller asserts with context.
            let f = match read_frame(&mut r) {
                Ok(f) => f,
                Err(_) => return (out, false),
            };
            assert_eq!(f.msg_type, FrameType::SessionData);
            let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
            let chunk = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                v["data_b64"].as_str().unwrap(),
            )
            .unwrap();
            out.extend_from_slice(&chunk);
            if out.windows(needle.len()).any(|w| w == needle) {
                return (out, true);
            }
            if v["eof"].as_bool().unwrap() {
                return (out, false);
            }
        }
    }

    fn snapshot(&self, bundle: &Path) {
        let reply = send_ctl(
            self.sock.join("k.sock"),
            &format!("SNAPSHOT {}", bundle.display()),
        )
        .expect("SNAPSHOT command");
        assert!(reply.starts_with("OK"), "SNAPSHOT refused: {reply}");
    }

    /// SIGKILL + reap (mirrors LiveWorker discipline without owning it here:
    /// the test holds the Child directly, so wait() reaps — no zombie).
    fn kill_reap(&mut self) {
        let pid = self.child.id();
        self.child.kill().ok();
        let _ = self.child.wait();
        assert_zombie_free(pid);
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.child.kill().ok();
        let _ = self.child.wait();
    }
}

fn assert_zombie_free(pid: u32) {
    assert!(!is_zombie(pid), "worker {pid} left a zombie");
}

fn is_zombie(pid: u32) -> bool {
    // Zombie => kill(0) succeeds but state is Z. Gone entirely => also fine.
    // SAFETY: kill(pid, 0) performs no action, only error checking.
    #[allow(unsafe_code)]
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
    if !alive {
        return false;
    }
    let path = format!("/proc/{pid}/stat");
    let stat = std::fs::read_to_string(path).unwrap_or_default();
    stat.split_whitespace().nth(2) == Some("Z")
}

fn b64(v: &serde_json::Value, k: &str) -> Vec<u8> {
    base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        v[k].as_str().unwrap(),
    )
    .unwrap()
}

#[test]
fn kvm_snapshot_restore_cycle() {
    let Some(cfg) = gated() else { return };

    // The caller freezes and copies the disk while SNAPSHOT leaves vCPUs
    // paused. libkrun's memory checkpoint does not copy the root overlay.
    let overlay = cfg.work.join("root.qcow2");
    let out = Command::new(&cfg.vmm)
        .args([
            "create-overlay",
            &overlay.to_string_lossy(),
            &cfg.image.to_string_lossy(),
            &base_image_bytes(&cfg.image).to_string(),
        ])
        .output()
        .expect("create-overlay");
    assert!(out.status.success(), "create-overlay failed");

    let bundle = cfg.work.join("bundle");
    let host = host_caps("krucible", VMM_VERSION, "bundled:libkrunfw.so.5");

    // ---- boot 1: smoke + state ----
    let mut g = Guest::boot(&cfg, &overlay, None);
    g.wait_ready();

    let v = g.exec(&["/bin/sh", "-c", "printf hello"]);
    assert_eq!(v["exit_code"], 0);
    assert_eq!(b64(&v, "stdout_b64"), b"hello");
    let v = g.exec(&["/bin/sh", "-c", "echo oops >&2; exit 3"]);
    assert_eq!(v["exit_code"], 3);
    assert_eq!(b64(&v, "stderr_b64"), b"oops\n");

    // Filesystem state inside the overlay (frozen by the snapshot).
    let v = g.exec(&[
        "/bin/sh",
        "-c",
        "echo FSVAL > /workspace/fs-proof; /bin/busybox sync",
    ]);
    assert_eq!(v["exit_code"], 0);
    // RAM state: session output exists only in guest memory. The session
    // stays alive across ALL cycles (long sleep), so attach with a marker
    // search — never drain-to-EOF here (EOF may never come).
    // NOTE: MARKER_SLEEP_SECS only needs to exceed total test time (~minutes);
    // nothing ever waits for it — the worker is killed at test end.
    const MARKER_SLEEP_SECS: u32 = 600;
    let sess = g.session_create(&[
        "/bin/sh",
        "-c",
        &format!("echo SNAPMARKER; sleep {MARKER_SLEEP_SECS}"),
    ]);
    let (first, found) = g.session_drain_until(&sess, 0, b"SNAPMARKER\n", Duration::from_secs(20));
    assert!(
        found,
        "no marker pre-snapshot: {:?}",
        String::from_utf8_lossy(&first)
    );

    // ---- snapshot + sidecar ----
    g.snapshot(&bundle);
    let frozen_root = bundle.join("root.qcow2");
    let root_bytes = std::fs::copy(&overlay, &frozen_root).unwrap();
    assert!(root_bytes > 0);
    let manifest = SnapshotManifest::new(
        "kvm-smoke",
        ahvm_engine::Compat {
            arch: std::env::consts::ARCH.to_string(),
            vmm: ahvm_engine::VmmId {
                name: "krucible".into(),
                version: VMM_VERSION.into(),
            },
            kernel_digest: "bundled:libkrunfw.so.5".into(),
            mem_mib: 512,
            vcpus: test_vcpus(),
            device_layout_ver: ahvm_engine::DEVICE_LAYOUT_VER,
        },
        ahvm_engine::Artifacts {
            memory_bytes: std::fs::metadata(bundle.join("memory.img"))
                .map(|m| m.len())
                .unwrap_or(0),
            root_delta_bytes: root_bytes,
        },
    );
    manifest.write_to(&bundle).unwrap();
    // libkrun's own manifest.json must be untouched by the sidecar.
    assert!(bundle.join("manifest.json").is_file(), "no vmm manifest");
    assert!(bundle.join("ahvm-manifest.json").is_file(), "no sidecar");

    // Negative: tampered sidecar must refuse (gate is real, not decorative).
    let mut tampered = SnapshotManifest::read_from(&bundle).unwrap();
    tampered.compat.arch = "riscv64".into();
    tampered.write_to(&cfg.work.join("tampered")).unwrap();
    let bad = SnapshotManifest::read_from(&cfg.work.join("tampered")).unwrap();
    assert!(bad.check_compat(&host).is_err());

    assert!(send_ctl(g.sock.join("k.sock"), "RESUME")
        .unwrap()
        .starts_with("OK"));
    g.wait_ready();
    let v = g.exec(&[
        "/bin/sh",
        "-c",
        "echo AFTER > /workspace/fs-proof; /bin/busybox sync",
    ]);
    assert_eq!(v["exit_code"], 0);

    // ---- repeated kill/restore (old #3 flake shape) ----
    let cycles: usize = std::env::var("AHVM_KVM_CYCLES")
        .map(|s| s.parse().expect("AHVM_KVM_CYCLES must be a number"))
        .unwrap_or(3);
    assert!(cycles >= 3, "run at least three recovery cycles");
    for cycle in 0..cycles {
        g.kill_reap();
        std::fs::copy(&frozen_root, &overlay).unwrap();
        if let Ok(secs) = std::env::var("AHVM_KVM_RESTORE_DELAY_SECS") {
            std::thread::sleep(Duration::from_secs(secs.parse().unwrap()));
        }
        // Compat gate runs BEFORE the restore spawn, on the real bundle.
        let back = SnapshotManifest::read_from(&bundle).unwrap();
        back.check_compat(&host)
            .unwrap_or_else(|e| panic!("cycle {cycle}: compat refused real bundle: {e}"));
        g = Guest::boot(&cfg, &overlay, Some(&bundle));
        g.wait_ready();

        let v = g.exec(&["/bin/sh", "-c", "echo CONSOLE_OK > /dev/hvc0; printf hello"]);
        assert_eq!(v["exit_code"], 0, "cycle {cycle}: exec dead after restore");
        assert_eq!(b64(&v, "stdout_b64"), b"hello");

        // RAM proof: the pre-snapshot session resumes with its output.
        // A fresh boot has no such session and errors here instead.
        let (out, found) =
            g.session_drain_until(&sess, 0, b"SNAPMARKER\n", Duration::from_secs(20));
        assert!(
            found,
            "cycle {cycle}: RAM marker lost: {:?}",
            String::from_utf8_lossy(&out)
        );

        // Disk proof: file written pre-snapshot is back.
        let v = g.exec(&[
            "/bin/sh",
            "-c",
            "echo 3 > /proc/sys/vm/drop_caches; cat /workspace/fs-proof",
        ]);
        assert_eq!(b64(&v, "stdout_b64"), b"FSVAL\n", "cycle {cycle}: fs lost");
        let v = g.exec(&[
            "/bin/sh",
            "-c",
            "echo AFTER > /workspace/fs-proof; /bin/busybox sync",
        ]);
        assert_eq!(v["exit_code"], 0);
        eprintln!("PASS recovery cycle {cycle}: exec, session RAM, disk rollback");
    }

    g.kill_reap();

    // Exercise the actual worker's refusal, before a vCPU can resume.
    let mut checkpoint = std::fs::read(bundle.join("checkpoint.bin")).unwrap();
    checkpoint[8..12].copy_from_slice(&1u32.to_le_bytes());
    let incompatible = cfg.work.join("old-format");
    std::fs::create_dir_all(&incompatible).unwrap();
    std::fs::write(incompatible.join("checkpoint.bin"), checkpoint).unwrap();
    let mut rejected = Guest::boot(&cfg, &overlay, Some(&incompatible));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = rejected.child.try_wait().unwrap() {
            assert!(!status.success(), "old checkpoint must fail");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker failed to reject old checkpoint"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let log = std::fs::read_to_string(cfg.work.join("restore.log")).unwrap();
    assert!(
        log.contains("checkpoint version 1 != 2"),
        "wrong refusal: {log}"
    );
}
