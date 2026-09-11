# Omarchy desktop experiment

Opt-in development experiment, separate from the released Ubuntu/XFCE desktop.
Uses the existing AHVM Arch GPU image, with Omarchy's Hyprland configuration,
Quickshell, theme, Firefox and terminal. It does not run Omarchy's disk installer
or replace the host OS.

Upstream source tested: `omacom/omarchy` commit
`b5589faaf80c6f87c07d4560fca37c4a81722f28` (`4.0.0.alpha`).
This is an adapted desktop, not a fully qualified Omarchy distribution.

On the Linux builder, export that upstream commit into a source directory, then:

```sh
sudo images/omarchy-desktop/prepare-image.sh \
  /path/to/arch-desktop.ext4 /path/to/new-omarchy.ext4 /path/to/omarchy-source
```

The output must be new. The builder copies the base and grows the copy to 40 GiB;
it leaves the base unchanged. Arch packages remain signature checked. Package
versions roll, so the source pin alone does not make the image reproducible.

For testing use one 4-vCPU / 8-GiB VM, the experimental GPU-enabled worker and
`AHVM_DESKTOP_IMAGE=/path/to/new-omarchy.ext4`, `AHVM_DESKTOP_GPU=1` on an isolated
daemon. See [normal installation](../../docs/OMARCHY-DESKTOP.md) for the packaged path.

The initialization wrapper execs systemd as PID 1. System services supervise
Forge and the PAM-backed desktop session. `user@1000.service` supplies the user
manager and D-Bus session; user services supervise WayVNC and the private vsock
relay. Forge waits for the relay before advertising readiness. The normal Ubuntu
image and host services are unchanged.

The bundled 6.12.44 kernel supports this boot and cgroup v2. The image overrides
Arch's PID maximum with the supported 32768 limit. AHVM owns virtual networking,
so networkd/resolved are disabled in this image; its first-run Wi-Fi prompt is
skipped when no wireless device exists. Journal storage is bounded to 64 MiB.

Power management, desktop login/lock, sound, Omarchy updates, hardware helpers,
clipboard/international keyboard integration and GPU isolation remain outside
this qualification. RAM snapshots are unsupported. Keep Ubuntu as the default.

## Qualification (2026-09-11)

On `agent_house`, one 4-CPU / 8-GiB VM booted the 40-GiB image using the
experimental GPU worker. Confirmed systemd is PID 1, cgroup v2 is mounted,
Forge/desktop/user manager/VNC/relay are active, and no systemd units failed.
Native macOS viewer displays Omarchy's bar, notifications and menu. The RFB
probe typed a command and the guest marker confirmed delivery. HTTPS returned
200 through the AHVM gateway. Stop/start preserved a disk marker and all services
came back without failed units. A fresh build from the committed recipe also
booted with healthy services and working HTTPS.

The owner completed visual testing and approved merging the preview. No Omarchy
image has been published and no production service or existing sandbox was changed.

Keybindings menu follow-up: explicitly install Perl and Lua, and adapt the pinned
upstream scanner's monitor mock for qconsole. The four-modifier Hyper+K sequence
was verified in the native viewer to reopen the Keybindings menu after Escape.
The user's physical Raycast Caps Lock remapper remains a manual input check.
