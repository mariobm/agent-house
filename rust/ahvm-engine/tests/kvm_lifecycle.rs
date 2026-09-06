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
//! - A pre-snapshot session re-attaches post-restore with its output intact
//!   (RAM survived — a fresh boot cannot pass this, the session would not
//!   exist), and a pre-snapshot file survives (disk path via qcow2 overlay).
//! - Repeated kill/restore cycles (the old #3 flake shape) with bounded
//!   readiness and zombie checks after every kill.

#![cfg(unix)]

use ahvm_engine::{host_caps, send_ctl, SnapshotManifest};
use ahvm_proto::{read_frame, write_frame, Frame, FrameType};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(60);
const VMM_VERSION: &str = "libkrun-2.0.0-dev";

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
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_lifecycle: no /dev/kvm");
        return None;
    }
    let vmm = std::env::var("AHVM_VMM_BIN").ok().map(PathBuf::from)?;
    let image = std::env::var("AHVM_GUEST_IMAGE").ok().map(PathBuf::from)?;
    assert!(vmm.is_file(), "AHVM_VMM_BIN missing: {}", vmm.display());
    assert!(image.is_file(), "AHVM_GUEST_IMAGE missing: {}", image.display());
    let work = std::env::temp_dir().join(format!("ahvm-kvm-{}", std::process::id()));
    std::fs::create_dir_all(work.join("sock")).unwrap();
    Some(Cfg { vmm, image, work })
}

fn base_image_bytes(image: &Path) -> u64 {
    std::fs::metadata(image).unwrap().len()
}

fn write_spec(path: &Path, image: &Path, sock: &Path, snapshot_dir: Option<&Path>) {
    let mut spec = serde_json::json!({
        "vcpus": 1,
        "mem_mib": 512,
        "log_level": 2,
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
        // Worker stderr goes to a log file (NOT null): a dead-on-arrival
        // worker must leave evidence instead of failing silently.
        let log = std::fs::File::create(cfg.work.join("vmm.log")).unwrap();
        let child = Command::new(&cfg.vmm)
            .arg(&spec)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .expect("spawn ahvm-vmm");
        Self { child, sock }
    }

    fn conn(&self) -> UnixStream {
        let deadline = Instant::now() + READY_TIMEOUT;
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
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.exec(&["/bin/true"])
            })) {
                Ok(v) if v["exit_code"] == 0 => return,
                _ => {
                    if Instant::now() > deadline {
                        panic!("agent not ready within {READY_TIMEOUT:?}");
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

    /// Attach from `seq`, collecting until `eof` or the budget runs out.
    fn session_drain(&self, id: &str, from_seq: u64, budget: Duration) -> (Vec<u8>, bool) {
        let body = serde_json::json!({ "op": "attach", "session_id": id, "from_seq": from_seq })
            .to_string()
            .into_bytes();
        let stream = {
            let mut c = self.conn();
            write_frame(&mut c, &Frame { msg_type: FrameType::SessionReq, payload: body })
                .unwrap();
            c
        };
        let mut r = BufReader::new(stream);
        let mut out = Vec::new();
        let deadline = Instant::now() + budget;
        loop {
            if Instant::now() > deadline {
                return (out, false);
            }
            let f = read_frame(&mut r).unwrap();
            assert_eq!(f.msg_type, FrameType::SessionData);
            let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
            let chunk =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, v["data_b64"].as_str().unwrap())
                    .unwrap();
            out.extend_from_slice(&chunk);
            if v["eof"].as_bool().unwrap() {
                return (out, true);
            }
        }
    }

    fn snapshot(&self, bundle: &Path) {
        let reply = send_ctl(self.sock.join("k.sock"), &format!("SNAPSHOT {}", bundle.display()))
            .expect("SNAPSHOT command");
        assert!(
            reply.starts_with("OK"),
            "SNAPSHOT refused: {reply}"
        );
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

    // qcow2 overlay over the base image: the snapshot bundle freezes THIS
    // file, so post-restore reads prove disk restore (a raw shared disk
    // would prove nothing — writes persist regardless of restore).
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
    let v = g.exec(&["/bin/sh", "-c", "echo FSVAL > /workspace/fs-proof"]);
    assert_eq!(v["exit_code"], 0);
    // RAM state: session output exists only in guest memory.
    let sess = g.session_create(&["/bin/sh", "-c", "echo SNAPMARKER; sleep 60"]);
    let (first, _) = g.session_drain(&sess, 0, Duration::from_secs(20));
    assert!(
        first.windows(11).any(|w| w == b"SNAPMARKER\n"),
        "no marker pre-snapshot: {:?}",
        String::from_utf8_lossy(&first)
    );

    // ---- snapshot + sidecar ----
    g.snapshot(&bundle);
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
            vcpus: 1,
            device_layout_ver: ahvm_engine::DEVICE_LAYOUT_VER,
        },
        ahvm_engine::Artifacts {
            memory_bytes: std::fs::metadata(bundle.join("memory.img"))
                .map(|m| m.len())
                .unwrap_or(0),
            root_delta_bytes: 0,
        },
    );
    manifest.write_to(&bundle).unwrap();
    // libkrun's own manifest.json must be untouched by the sidecar.
    assert!(bundle.join("manifest.json").is_file(), "no vmm manifest");
    assert!(bundle.join("ahvm-manifest.json").is_file(), "no sidecar");

    // ---- repeated kill/restore (old #3 flake shape) ----
    for cycle in 0..3 {
        g.kill_reap();
        // Compat gate runs BEFORE the restore spawn, on the real bundle.
        let back = SnapshotManifest::read_from(&bundle).unwrap();
        back.check_compat(&host)
            .unwrap_or_else(|e| panic!("cycle {cycle}: compat refused real bundle: {e}"));
        g = Guest::boot(&cfg, &overlay, Some(&bundle));
        g.wait_ready();

        let v = g.exec(&["/bin/sh", "-c", "printf hello"]);
        assert_eq!(v["exit_code"], 0, "cycle {cycle}: exec dead after restore");
        assert_eq!(b64(&v, "stdout_b64"), b"hello");

        // RAM proof: the pre-snapshot session resumes with its output.
        // A fresh boot has no such session and errors here instead.
        let (out, eof) = g.session_drain(&sess, 0, Duration::from_secs(20));
        assert!(
            out.windows(11).any(|w| w == b"SNAPMARKER\n"),
            "cycle {cycle}: RAM marker lost: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(eof, "cycle {cycle}: session did not reach EOF");

        // Disk proof: file written pre-snapshot is back.
        let v = g.exec(&["/bin/sh", "-c", "cat /workspace/fs-proof"]);
        assert_eq!(b64(&v, "stdout_b64"), b"FSVAL\n", "cycle {cycle}: fs lost");
    }

    // Negative: tampered sidecar must refuse (gate is real, not decorative).
    let mut tampered = SnapshotManifest::read_from(&bundle).unwrap();
    tampered.compat.arch = "riscv64".into();
    tampered.write_to(&cfg.work.join("tampered")).unwrap();
    let bad = SnapshotManifest::read_from(&cfg.work.join("tampered")).unwrap();
    assert!(bad.check_compat(&host).is_err());

    g.kill_reap();
}
