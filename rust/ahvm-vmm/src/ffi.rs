//! Raw FFI to the libkrucible C ABI (mirrors `libkrun.h`).
//!
//! The symbols come from the `krun` crate itself (re-exported below), NOT
//! from a hand-written extern block: declaring `extern "C"` locally creates
//! no link edge, and cargo silently omits the rlib (undefined symbols at
//! link time). `use krun::...` is what pulls it into the link.
//!
//! Every function returns 0 on success or a negative errno on failure.
//! All `*const c_char` params borrow from the caller's CString arena.
//!
//! Unsafe is allowed file-wide here and nowhere else in this crate: this
//! file contains ONLY FFI calls (no logic).

#![allow(unsafe_code)]

pub use krun::{
    krun_add_disk2, krun_add_net_unixstream, krun_add_virtio_console_default, krun_add_virtiofs3,
    krun_add_vsock, krun_add_vsock_port2, krun_create_ctx, krun_create_disk_overlay,
    krun_disable_implicit_init, krun_init_log, krun_set_control_socket, krun_set_exec,
    krun_set_kernel, krun_set_root_disk2, krun_set_snapshot, krun_set_vm_config, krun_start_enter,
};

use std::ffi::c_char;

/// Disk image formats (`KRUN_DISK_FORMAT_*`).
pub const DISK_RAW: u32 = 0;
pub const DISK_QCOW2: u32 = 1;

/// TSI features (`KRUN_TSI_*`).
pub const TSI_HIJACK_INET: u32 = 1;

/// Kernel image formats: raw Image (arm64) vs ELF vmlinux (x86).
/// Each is used on one arch only; gate accordingly to avoid dead_code.
#[cfg(not(target_arch = "x86_64"))]
pub const KERNEL_RAW: u32 = 0;
#[cfg(target_arch = "x86_64")]
pub const KERNEL_ELF: u32 = 1;

/// virtio-net features (from uapi/linux/virtio_net.h, per libkrun.h).
pub const NET_FEATURE_CSUM: u32 = 1 << 0;
pub const NET_FEATURE_GUEST_CSUM: u32 = 1 << 1;
pub const NET_FEATURE_GUEST_TSO4: u32 = 1 << 7;
pub const NET_FEATURE_GUEST_UFO: u32 = 1 << 10;
pub const NET_FEATURE_HOST_TSO4: u32 = 1 << 4;
pub const NET_FEATURE_HOST_UFO: u32 = 1 << 5;

// Safe wrappers: each centralizes one FFI call so the driver (main.rs)
// contains zero unsafe code. Pointer args must borrow from a CString arena
// that outlives the call; the driver guarantees this (see Arena).

pub fn init_log(target_fd: i32, level: u32) {
    unsafe { krun_init_log(target_fd, level, 0, 0) };
}

pub fn create_ctx() -> i32 {
    unsafe { krun_create_ctx() }
}

pub fn check(r: i32, what: &'static str) {
    if r != 0 {
        eprintln!("vmm: {what}: libkrun error {r}");
        std::process::exit(1);
    }
}

pub fn set_vm_config(cid: u32, vcpus: u8, mem_mib: u32) {
    check(unsafe { krun_set_vm_config(cid, vcpus, mem_mib) }, "krun_set_vm_config");
}

pub fn set_kernel(cid: u32, path: *const c_char, format: u32, cmdline: *const c_char) {
    check(
        unsafe { krun_set_kernel(cid, path, format, std::ptr::null(), cmdline) },
        "krun_set_kernel",
    );
}

pub fn disable_implicit_init(cid: u32) {
    check(unsafe { krun_disable_implicit_init(cid) }, "krun_disable_implicit_init");
}

pub fn set_root_disk(cid: u32, path: *const c_char, qcow2: bool) {
    check(
        unsafe { krun_set_root_disk2(cid, path, if qcow2 { DISK_QCOW2 } else { DISK_RAW }) },
        "krun_set_root_disk2",
    );
}

pub fn create_disk_overlay(overlay: *const c_char, backing: *const c_char, size: u64) {
    check(
        unsafe { krun_create_disk_overlay(overlay, backing, size) },
        "krun_create_disk_overlay",
    );
}

pub fn add_disk(cid: u32, block_id: *const c_char, path: *const c_char, qcow2: bool, ro: bool) {
    check(
        unsafe {
            krun_add_disk2(
                cid,
                block_id,
                path,
                if qcow2 { DISK_QCOW2 } else { DISK_RAW },
                ro,
            )
        },
        "krun_add_disk2",
    );
}

pub fn add_virtiofs(cid: u32, tag: *const c_char, path: *const c_char, read_only: bool) {
    check(
        unsafe { krun_add_virtiofs3(cid, tag, path, 0, read_only) },
        "krun_add_virtiofs3",
    );
}

pub fn add_console(cid: u32) {
    check(
        unsafe { krun_add_virtio_console_default(cid, 0, 1, 2) },
        "krun_add_virtio_console_default",
    );
}

pub fn add_vsock(cid: u32, hijack_inet: bool) {
    check(
        unsafe { krun_add_vsock(cid, if hijack_inet { TSI_HIJACK_INET } else { 0 }) },
        "krun_add_vsock",
    );
}

pub fn add_vsock_port(cid: u32, port: u32, uds: *const c_char, listen: bool) {
    check(
        unsafe { krun_add_vsock_port2(cid, port, uds, listen) },
        "krun_add_vsock_port2",
    );
}

pub fn add_net_unixstream(cid: u32, uds: *const c_char, mac: &[u8; 6]) {
    let features = NET_FEATURE_CSUM
        | NET_FEATURE_GUEST_CSUM
        | NET_FEATURE_GUEST_TSO4
        | NET_FEATURE_GUEST_UFO
        | NET_FEATURE_HOST_TSO4
        | NET_FEATURE_HOST_UFO;
    check(
        unsafe { krun_add_net_unixstream(cid, uds, -1, mac.as_ptr(), features, 0) },
        "krun_add_net_unixstream",
    );
}

pub fn set_control_socket(cid: u32, uds: *const c_char) {
    check(
        unsafe { krun_set_control_socket(cid, uds) },
        "krun_set_control_socket",
    );
}

pub fn set_snapshot(cid: u32, dir: *const c_char) {
    check(unsafe { krun_set_snapshot(cid, dir) }, "krun_set_snapshot");
}

pub fn set_exec(cid: u32, path: *const c_char, env: &[*const c_char]) {
    // env is already null-terminated by the caller (or empty for NULL).
    let envp = if env.is_empty() {
        std::ptr::null()
    } else {
        env.as_ptr()
    };
    check(
        unsafe { krun_set_exec(cid, path, std::ptr::null(), envp) },
        "krun_set_exec",
    );
}

/// Becomes the VM. Returns ONLY on boot error (returns the libkrun code).
pub fn start_enter(cid: u32) -> i32 {
    unsafe { krun_start_enter(cid) }
}
