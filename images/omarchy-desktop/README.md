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
daemon. The public catalog does not contain an `omarchy-desktop` entry yet.

AHVM supplies PID 1, networking and the private VNC transport. Quickshell is
launched directly because this guest does not have a systemd user manager.
Power management, desktop login/lock, sound, Omarchy updates, hardware helpers,
clipboard integration and GPU isolation require further qualification. RAM
snapshots are unsupported. Keep the released Ubuntu image as the default.

## Initial qualification (2026-09-11)

On `agent_house`, one 4-CPU / 8-GiB VM booted the 40-GiB image with the
experimental GPU worker. The RFB probe received a nonblank 1280×720 WayVNC
framebuffer and typed a command; `/home/desktop/input-ok` contained
`AHVM-VNC-INPUT-OK`, also visible in the capture. Quickshell started, but the
captured desktop only showed the themed terminal; the bar/launcher and full
Omarchy appearance are not yet qualified. The VM was terminated after the test.
No production service or existing sandbox was changed. No image was published.
