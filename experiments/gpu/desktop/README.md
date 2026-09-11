# Accelerated Hyprland desktop experiment

One standalone 2-vCPU / 4-GiB VM, separate from the installed AHVM daemon.
This is an Arch + Hyprland proof, not an Omarchy image or `ahvm desktop` command.
GPU snapshots remain disabled.

## Verified on agent_house

- Arch bootstrap 2026-09-01; Hyprland 0.56.2, Aquamarine 0.15.0,
  Mesa 26.2.2, WayVNC 0.10.1, Foot 1.28.0.
- Hyprland reports `virgl (Mesa Intel(R) UHD Graphics 770 (ADL-S GT1))`.
- A 1280×720 headless Wayland desktop runs a Bash terminal as an unprivileged user.
- A host-side RFB client receives pixels through a private Unix socket → vsock
  bridge → guest WayVNC Unix socket. It types a command into the terminal;
  the guest verifies the resulting file. The client captures the changed screen.
- Pointer coordinates are checked through Hyprland, allowing the observed
  one-pixel rounding in absolute input conversion.
- Two repeat boots and a separate image rebuilt from the recipe pass the full
  gate. The original framebuffer probe still passes (20 frames in 8.92 ms).
- No public VNC port, installed service changes, or additional sandboxes.

This is a capture/input correctness test, not a desktop FPS, WAN latency or
hardware video-encoding benchmark. The raw RFB client is deliberately a small
bounded test client, not the proposed packaged viewer.

## Reproduce

Build the experimental GPU VMM as described in the parent README. As root on
Linux, prepare a new disk (about 6 GiB logical; packages require network access):

```sh
experiments/gpu/desktop/prepare-image.sh /tmp/ahvm-desktop.ext4
AHVM_VMM_BIN="$PWD/rust/target/release/ahvm-vmm" \
  experiments/gpu/desktop/run.sh /tmp/ahvm-desktop.ext4 /tmp/ahvm-desktop-result
```

The bootstrap is pinned by SHA-256; pacman verifies package signatures, but
package versions roll with Arch. The guest has no user credentials or network
configuration. The runner owns one VM, checks capture, keyboard, pointer and
renderer, and stops it within 45 seconds. The prepared disk and output directory
remain for inspection. Do not run two instances against the same disk.

The result directory contains `vnc-before.png`, `vnc-after.png` and console logs.
Keep these artifacts outside the repository. No screenshot or large test output
is committed.

## What the experiment needed

1. Disable the legacy TSI INET fallback for GPU guests. The bundled kernel faults
   in `tsi_dgram_setsockopt` during udev startup with that fallback enabled.
2. Expose a virtual scanout to enable KMS capabilities. Aquamarine still checks
   DRM CRTC features even with `AQ_NO_KMS_REQUIREMENT=1`. The guest disables this
   physical-style output and creates a headless output for capture; the VMM's
   no-op host display backend cannot present scanouts.
3. Mount `/dev/shm` and grant the guest desktop user access to its render node.
4. Implement the fork's missing `transfer_read` command by delegating to
   Rutabaga ([fork PR #5](https://github.com/mariobm/libkrucible/pull/5)).
   Previously, WayVNC capture panicked the GPU worker.
5. Allow WayVNC's initial placeholder frame before checking actual pixels.

Mesa still emits a failed optional RESOURCE_MAP_BLOB attempt (0x208) and falls
back to readback. The tested path works, but zero-copy capture is not qualified.
The renderer is in the VMM process: this remains a trusted-workload experiment,
not an isolation qualification for untrusted desktop guests.

The next integration is now available as an experimental
[managed image](../../../images/arch-desktop/README.md) and
[optional native viewer](../../../desktop-viewer/README.md). Omarchy customization
and release qualification remain pending.

## Build checks

AHVM GPU build/clippy, default VMM check, formatting, shell/Python syntax,
the fork GPU build and its existing GPU unit test pass. Broader fork checks
also ran: default clippy passes; SEV/TDX fail on existing Rng feature-gating
errors in `virtio/persist.rs`, and the input-enabled clippy combination fails
on an existing redundant borrow in `input/worker.rs`. Those unrelated errors
are not changed or suppressed by this experiment.
