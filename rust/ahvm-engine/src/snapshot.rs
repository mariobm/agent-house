//! v2 snapshot manifest + the strict compat gate.
//!
//! A snapshot bundle is `{memory, root delta}` artifacts plus this JSON
//! manifest. Restoring refuses (with a precise reason) when the snapshot
//! was taken for a different machine: wrong CPU arch, wrong VMM
//! name/version, wrong kernel, or a device-layout the host no longer
//! understands. This is the local-first half of the S3-plan compat gate
//! (plan §7: chunked content-addressed artifacts arrive later; the gate
//! shape is fixed now so manifests stay forward-compatible).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Filename of the ENGINE sidecar inside a snapshot bundle dir.
/// MUST differ from libkrun's own `manifest.json` (VMM-private: memory
/// layout, device state), which lives in the same dir.
///
/// Coexistence contract:
///
/// - `<bundle>/manifest.json` — written/read ONLY by libkrun (`SNAPSHOT`
///   and `krun_set_snapshot`). The engine never opens it; it only passes
///   the bundle dir through to the worker spec.
///
/// - `<bundle>/ahvm-manifest.json` — written by the engine after `SNAPSHOT`
///   succeeds, `check_compat`-gated by the engine BEFORE spawning a restore
///   worker. The worker stays dumb (no manifest logic, no VMM coupling).
///
/// A bundle missing the sidecar is treated as foreign/unmanaged and
/// refused; a bundle missing libkrun's manifest fails inside libkrun
/// at restore.
pub const SIDECAR_NAME: &str = "ahvm-manifest.json";

/// Manifest schema version written by this crate.
pub const MANIFEST_VER: u32 = 2;
/// Current virtual device-layout version. Bump when the VMM's device set
/// changes incompatibly; old bundles then fail the gate instead of
/// resuming into a half-wired VM.
pub const DEVICE_LAYOUT_VER: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmmId {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compat {
    /// `std::env::consts::ARCH` string (`"aarch64"`, `"x86_64"`).
    pub arch: String,
    pub vmm: VmmId,
    /// Digest of the kernel image the snapshot booted with.
    pub kernel_digest: String,
    pub mem_mib: u32,
    pub vcpus: u8,
    /// [`DEVICE_LAYOUT_VER`] at snapshot time.
    pub device_layout_ver: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifacts {
    pub memory_bytes: u64,
    pub root_delta_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub manifest_ver: u32,
    pub snapshot_id: String,
    pub created_at: i64,
    pub compat: Compat,
    pub artifacts: Artifacts,
}

/// Host capabilities a manifest is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostCaps {
    pub arch: String,
    pub vmm: VmmId,
    pub kernel_digest: String,
    pub device_layout_ver: u32,
}

/// Build [`HostCaps`] for this machine: native arch + current device
/// layout, caller-supplied VMM identity and kernel digest.
pub fn host_caps(vmm_name: &str, vmm_version: &str, kernel_digest: &str) -> HostCaps {
    HostCaps {
        arch: std::env::consts::ARCH.to_string(),
        vmm: VmmId {
            name: vmm_name.to_string(),
            version: vmm_version.to_string(),
        },
        kernel_digest: kernel_digest.to_string(),
        device_layout_ver: DEVICE_LAYOUT_VER,
    }
}

pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl SnapshotManifest {
    pub fn new(snapshot_id: impl Into<String>, compat: Compat, artifacts: Artifacts) -> Self {
        Self {
            manifest_ver: MANIFEST_VER,
            snapshot_id: snapshot_id.into(),
            created_at: unix_now(),
            compat,
            artifacts,
        }
    }

    /// Strict compat gate: every mismatch is refused with the exact
    /// dimension that diverged. Memory/vCPU sizes are intentionally *not*
    /// gated — a host may restore a small snapshot with room to spare.
    pub fn check_compat(&self, host: &HostCaps) -> crate::Result<()> {
        let id = &self.snapshot_id;
        if self.manifest_ver != MANIFEST_VER {
            return Err(crate::Error::Incompatible(format!(
                "snapshot {id}: manifest version {} != host {MANIFEST_VER}",
                self.manifest_ver
            )));
        }
        if self.compat.arch != host.arch {
            return Err(crate::Error::Incompatible(format!(
                "snapshot {id}: arch mismatch: snapshot is {} but host is {}",
                self.compat.arch, host.arch
            )));
        }
        if self.compat.vmm.name != host.vmm.name {
            return Err(crate::Error::Incompatible(format!(
                "snapshot {id}: vmm mismatch: snapshot needs {} but host runs {}",
                self.compat.vmm.name, host.vmm.name
            )));
        }
        if self.compat.vmm.version != host.vmm.version {
            return Err(crate::Error::Incompatible(format!(
                "snapshot {id}: vmm version mismatch: snapshot needs {} {} but host runs {}",
                self.compat.vmm.name, self.compat.vmm.version, host.vmm.version
            )));
        }
        if self.compat.kernel_digest != host.kernel_digest {
            return Err(crate::Error::Incompatible(format!(
                "snapshot {id}: kernel mismatch: snapshot kernel digest {} != host {}",
                self.compat.kernel_digest, host.kernel_digest
            )));
        }
        if self.compat.device_layout_ver != host.device_layout_ver {
            return Err(crate::Error::Incompatible(format!(
                "snapshot {id}: device layout mismatch: snapshot layout version {} != host {}",
                self.compat.device_layout_ver, host.device_layout_ver
            )));
        }
        Ok(())
    }

    /// Persist as `<dir>/ahvm-manifest.json` (see [`SIDECAR_NAME`] for why
    /// NOT `manifest.json`), atomically (tmp + rename).
    pub fn write_to(&self, dir: &Path) -> crate::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(SIDECAR_NAME);
        let tmp = dir.join("ahvm-manifest.json.tmp");
        let raw = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Read back a sidecar written by [`SnapshotManifest::write_to`].
    /// A missing sidecar means a foreign/unmanaged bundle: refused as
    /// incompatible (never silently restored).
    pub fn read_from(dir: &Path) -> crate::Result<Self> {
        let raw = std::fs::read(dir.join(SIDECAR_NAME))?;
        Ok(serde_json::from_slice(&raw)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    fn fixture() -> (SnapshotManifest, HostCaps) {
        let compat = Compat {
            arch: std::env::consts::ARCH.to_string(),
            vmm: VmmId {
                name: "krucible".to_string(),
                version: "0.1.0".to_string(),
            },
            kernel_digest: "sha256:abc".to_string(),
            mem_mib: 512,
            vcpus: 2,
            device_layout_ver: DEVICE_LAYOUT_VER,
        };
        let manifest = SnapshotManifest::new(
            "snap-1",
            compat,
            Artifacts {
                memory_bytes: 512 * 1024 * 1024,
                root_delta_bytes: 0,
            },
        );
        let host = HostCaps {
            arch: std::env::consts::ARCH.to_string(),
            vmm: VmmId {
                name: "krucible".to_string(),
                version: "0.1.0".to_string(),
            },
            kernel_digest: "sha256:abc".to_string(),
            device_layout_ver: DEVICE_LAYOUT_VER,
        };
        (manifest, host)
    }

    #[test]
    fn compat_gate_accepts_matching_host() {
        let (m, h) = fixture();
        m.check_compat(&h).unwrap();
        assert_eq!(m.manifest_ver, MANIFEST_VER);
    }

    #[test]
    fn compat_gate_refuses_each_dimension() {
        let (m, h) = fixture();

        let mut arch = m.clone();
        arch.compat.arch = "mips".to_string();
        let err = arch.check_compat(&h).unwrap_err();
        assert!(matches!(err, Error::Incompatible(_)), "unexpected {err}");
        assert!(err.to_string().contains("arch"), "unexpected {err}");

        let mut vmm = m.clone();
        vmm.compat.vmm.name = "firecracker".to_string();
        let err = vmm.check_compat(&h).unwrap_err();
        assert!(err.to_string().contains("vmm"), "unexpected {err}");

        let mut ver = m.clone();
        ver.compat.vmm.version = "9.9.9".to_string();
        let err = ver.check_compat(&h).unwrap_err();
        assert!(err.to_string().contains("vmm version"), "unexpected {err}");

        let mut kernel = m.clone();
        kernel.compat.kernel_digest = "sha256:other".to_string();
        let err = kernel.check_compat(&h).unwrap_err();
        assert!(err.to_string().contains("kernel"), "unexpected {err}");

        let mut layout = m.clone();
        layout.compat.device_layout_ver = DEVICE_LAYOUT_VER + 1;
        let err = layout.check_compat(&h).unwrap_err();
        assert!(
            err.to_string().contains("device layout"),
            "unexpected {err}"
        );

        let mut old = m.clone();
        old.manifest_ver = MANIFEST_VER - 1;
        let err = old.check_compat(&h).unwrap_err();
        assert!(
            err.to_string().contains("manifest version"),
            "unexpected {err}"
        );
    }

    #[test]
    fn manifest_file_roundtrip() {
        let dir = crate::test_scratch("manifest-roundtrip").join("snap-1");
        let (m, _) = fixture();
        let path = m.write_to(&dir).unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), SIDECAR_NAME);
        assert_eq!(SnapshotManifest::read_from(&dir).unwrap(), m);
    }

    #[test]
    fn sidecar_coexists_with_vmm_manifest() {
        // The exact collision this exists to prevent: libkrun's own
        // `manifest.json` (opaque bytes here) and our sidecar share a
        // bundle dir. Writing/reading either must leave the other intact.
        let dir = crate::test_scratch("manifest-coexist").join("snap-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"\x00vmm-private").unwrap();
        let (m, host) = fixture();
        m.write_to(&dir).unwrap();
        assert_eq!(
            std::fs::read(dir.join("manifest.json")).unwrap(),
            b"\x00vmm-private"
        );
        let back = SnapshotManifest::read_from(&dir).unwrap();
        assert_eq!(back, m);
        back.check_compat(&host).unwrap();
    }

    #[test]
    fn missing_sidecar_is_refused() {
        // Foreign bundle (libkrun manifest only, no sidecar): read_from
        // errors, so restore can never silently proceed.
        let dir = crate::test_scratch("manifest-missing").join("snap-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"\x00vmm-private").unwrap();
        assert!(SnapshotManifest::read_from(&dir).is_err());
    }

    #[test]
    fn host_caps_helper_matches_native_arch() {
        let host = host_caps("krucible", "0.1.0", "sha256:abc");
        assert_eq!(host.arch, std::env::consts::ARCH);
        assert_eq!(host.device_layout_ver, DEVICE_LAYOUT_VER);
    }
}
