//! ahvm-vmm: per-VM libkrun worker (the only component that links libkrun).
//!
//! Reads a [`VmSpec`] JSON file, configures one libkrun context, and calls
//! `krun_start_enter` — at which point THIS PROCESS BECOMES THE VM and never
//! returns. The daemon spawns one worker per sandbox and drives it
//! out-of-band (forge over bridged vsock UDS, lifecycle over the control
//! socket). A VMM crash takes down only this process, never the daemon.
//!
//! Also serves the storage primitive `create-overlay` (qcow2 CoW overlay via
//! libkrun/imago) so the daemon itself never links libkrun.
//!
//! Call sequence mirrors the proven Go `cmd/vmm` worker.

mod ffi;

use ffi as krun;
use serde::Deserialize;
use std::ffi::CString;
use std::os::raw::c_char;

/// VM spec (v2 JSON). Same semantics as the Go VMSpec, renamed fields allowed
/// (clean break — the daemon is the only writer).
#[derive(Debug, Deserialize)]
struct VmSpec {
    vcpus: u8,
    mem_mib: u32,
    #[serde(default)]
    log_level: u32,
    /// External (lean) kernel image. Empty = bundled libkrunfw kernel.
    #[serde(default)]
    kernel_image: String,
    #[serde(default)]
    kernel_cmdline: String,
    /// Block-root disk (raw ext4 or qcow2 overlay). Empty = virtio-fs root.
    #[serde(default)]
    root_disk: String,
    #[serde(default)]
    root_disk_format: String,
    #[serde(default)]
    rootfs_dir: String,
    #[serde(default)]
    mounts: Vec<FsMount>,
    #[serde(default)]
    volumes: Vec<Volume>,
    /// PID-1 mode: ExecPath boots as PID 1 (external kernel: via init=).
    #[serde(default)]
    pid1: bool,
    #[serde(default)]
    exec_path: String,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    net_uds: String,
    #[serde(default)]
    net_mac: String,
    /// Host UDS bridged to guest vsock 1024/1025 (host dials, listen=true).
    #[serde(default)]
    vsock_control_uds: String,
    #[serde(default)]
    vsock_forward_uds: String,
    /// Host UDS the guest dials on vsock 1026 for boot config (listen=false).
    #[serde(default)]
    vsock_config_uds: String,
    #[serde(default)]
    control_socket_uds: String,
    /// Cold restore from this snapshot bundle instead of cold-booting.
    #[serde(default)]
    snapshot_dir: String,
}

#[derive(Debug, Deserialize)]
struct FsMount {
    tag: String,
    host_path: String,
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Deserialize)]
struct Volume {
    block_id: String,
    path: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    read_only: bool,
}

/// CString arena: every C string handed to libkrun lives here until the
/// process becomes the VM (start_enter never returns), so no use-after-free
/// and no leaks that matter.
#[derive(Default)]
struct Arena(Vec<CString>);

impl Arena {
    fn put(&mut self, s: &str) -> *const c_char {
        self.0.push(CString::new(s).unwrap_or_else(|_| {
            eprintln!("vmm: refusing string with interior NUL");
            std::process::exit(1);
        }));
        self.0.last().unwrap().as_ptr()
    }
}

// NOTE: raw FFI once lived behind this macro; all calls now go through
// the safe wrappers in ffi.rs (Arena-lifetime reasoning lives there).

fn default_cmdline(exec_path: &str) -> String {
    let init = if exec_path.is_empty() {
        "/init.krun"
    } else {
        exec_path
    };
    #[cfg(target_arch = "x86_64")]
    let clock = " clocksource=kvm-clock";
    #[cfg(not(target_arch = "x86_64"))]
    let clock = "";
    format!(
        "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 root=/dev/vda rootfstype=ext4 rw quiet no-kvmapf{clock} init={init}"
    )
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        mac[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(mac)
}

fn create_overlay(args: &[String]) {
    if args.len() != 3 {
        eprintln!("vmm: usage: vmm create-overlay <overlay> <backing> <size_bytes>");
        std::process::exit(1);
    }
    let size: u64 = args[2].parse().unwrap_or_else(|_| {
        eprintln!("vmm: create-overlay: bad size {:?}", args[2]);
        std::process::exit(1);
    });
    let mut arena = Arena::default();
    let overlay = arena.put(&args[0]);
    let backing = arena.put(&args[1]);
    krun::create_disk_overlay(overlay, backing, size);
}

fn run(spec: VmSpec) {
    let mut arena = Arena::default();
    krun::init_log(-1, spec.log_level);
    let ctx = krun::create_ctx();
    if ctx < 0 {
        eprintln!("vmm: krun_create_ctx: {ctx}");
        std::process::exit(1);
    }
    let cid = ctx as u32;

    krun::set_vm_config(cid, spec.vcpus, spec.mem_mib);

    let external_kernel = !spec.kernel_image.is_empty();
    if external_kernel {
        let kernel = arena.put(&spec.kernel_image);
        let cmdline = if spec.kernel_cmdline.is_empty() {
            default_cmdline(&spec.exec_path)
        } else {
            spec.kernel_cmdline.clone()
        };
        let cmdline_p = arena.put(&cmdline);
        #[cfg(target_arch = "x86_64")]
        let format = krun::KERNEL_ELF;
        #[cfg(not(target_arch = "x86_64"))]
        let format = krun::KERNEL_RAW;
        krun::set_kernel(cid, kernel, format, cmdline_p);
    }

    if spec.pid1 && !external_kernel {
        krun::disable_implicit_init(cid);
    }

    if !spec.root_disk.is_empty() {
        let disk = arena.put(&spec.root_disk);
        krun::set_root_disk(cid, disk, spec.root_disk_format == "qcow2");
    } else {
        let root = arena.put(&spec.rootfs_dir);
        let tag = arena.put("/dev/root");
        krun::add_virtiofs(cid, tag, root, false);
    }

    for m in &spec.mounts {
        let tag = arena.put(&m.tag);
        let path = arena.put(&m.host_path);
        krun::add_virtiofs(cid, tag, path, m.read_only);
    }

    for v in &spec.volumes {
        let bid = arena.put(&v.block_id);
        let path = arena.put(&v.path);
        krun::add_disk(cid, bid, path, v.format == "qcow2", v.read_only);
    }

    krun::add_console(cid);
    krun::add_vsock(cid, spec.net_uds.is_empty());

    if !spec.net_uds.is_empty() {
        let mac = parse_mac(&spec.net_mac).unwrap_or_else(|| {
            eprintln!("vmm: bad net_mac {:?}", spec.net_mac);
            std::process::exit(1);
        });
        let uds = arena.put(&spec.net_uds);
        krun::add_net_unixstream(cid, uds, &mac);
    }

    // Host-dialed bridges (listen=true) to guest 1024/1025.
    for (port, uds) in [(1024u32, &spec.vsock_control_uds), (1025, &spec.vsock_forward_uds)] {
        if uds.is_empty() {
            continue;
        }
        let p = arena.put(uds);
        krun::add_vsock_port(cid, port, p, true);
    }
    // Guest-dialed config fetch on 1026 (listen=false).
    if !spec.vsock_config_uds.is_empty() {
        let p = arena.put(&spec.vsock_config_uds);
        krun::add_vsock_port(cid, 1026, p, false);
    }

    if !spec.control_socket_uds.is_empty() {
        let p = arena.put(&spec.control_socket_uds);
        krun::set_control_socket(cid, p);
    }

    if !spec.snapshot_dir.is_empty() {
        let p = arena.put(&spec.snapshot_dir);
        krun::set_snapshot(cid, p);
    }

    if !external_kernel {
        let exec = arena.put(&spec.exec_path);
        if spec.env.is_empty() {
            // argv NULL, envp NULL: no args, inherit host environment.
            krun::set_exec(cid, exec, &[]);
        } else {
            let env_cs: Vec<CString> =
                spec.env.iter().map(|e| CString::new(e.as_str()).unwrap()).collect();
            let mut ptrs: Vec<*const c_char> = env_cs.iter().map(|c| c.as_ptr()).collect();
            ptrs.push(std::ptr::null());
            krun::set_exec(cid, exec, &ptrs);
        }
    }

    eprintln!(
        "vmm: start_enter pid1={} vcpus={} mem={}MiB",
        spec.pid1, spec.vcpus, spec.mem_mib
    );
    // Becomes the VM; returns only on boot error.
    let r = krun::start_enter(cid);
    eprintln!("vmm: krun_start_enter returned ({r}) — boot failed");
    std::process::exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("vmm: usage: vmm <spec.json> | vmm create-overlay <overlay> <backing> <size>");
        std::process::exit(1);
    }
    if args[1] == "create-overlay" {
        create_overlay(&args[2..]);
        return;
    }
    let data = std::fs::read(&args[1]).unwrap_or_else(|e| {
        eprintln!("vmm: read spec {:?}: {e}", args[1]);
        std::process::exit(1);
    });
    let spec: VmSpec = serde_json::from_slice(&data).unwrap_or_else(|e| {
        eprintln!("vmm: parse spec: {e}");
        std::process::exit(1);
    });
    run(spec);
}
