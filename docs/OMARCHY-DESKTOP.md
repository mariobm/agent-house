# Omarchy desktop preview

Linux x86_64 hosts with an accessible Intel render device can run the adapted
Omarchy/Hyprland desktop. The Mac remains a client. Ubuntu remains the default.
GPU rendering is a preview for trusted workloads: VirGL executes in the worker
process and accesses the host GPU. It does not provide a separate renderer sandbox.

```bash
ahvm host upgrade home
ahvm image pull omarchy-desktop
ahvm create omarchy --image omarchy-desktop
ahvm desktop omarchy
```

Creation defaults to 4 CPUs and 8192 MiB RAM and enables desktop mode automatically.
The image has a sparse 40 GiB virtual disk. A saved SSH host can install the image
on the first create automatically, so the explicit pull is optional. For direct
API connections, install the image on the server first. Omarchy cannot be made
the headless default image.

The server bundle includes a separate `ahvm-vmm-gpu` worker, pinned VirGL 1.2.0,
and Mesa EGL/DRI libraries. Normal VMs continue using the standard worker.
Fresh installation and host upgrades add the service account to the existing
render-node group when one is present. A host without a usable render device
gets an error. Hardware beyond the tested Intel host is not qualified yet.

The image boots systemd as PID 1. System/user services manage Hyprland, Forge,
WayVNC and the private desktop relay. The desktop uses the `desktop` user.
Closing the viewer disconnects the display; idle policy still applies. Stop/start
preserves disk data but starts a new desktop session. GPU RAM snapshots are not supported.

On macOS, Command maps to Super. A four-modifier Hyper chord also maps to Super.
Linux clients preserve their original modifiers. Global host shortcuts can still
intercept keys. Audio, desktop lock/login, clipboard and upstream Omarchy upgrades
remain outside this preview's qualification.

```bash
ahvm delete omarchy
```

## Building

See `images/omarchy-desktop/README.md` for the pinned upstream source and image
recipe. Release builds use Ubuntu 22.04 (glibc 2.35), with meson, ninja-build,
python3-yaml, libdrm-dev, libepoxy-dev, libegl1-mesa-dev, libgbm-dev and libgl1-mesa-dri.
`package-rust.sh` builds the pinned VirGL source and packages graphics dependencies
and their notices. Image downloads use the existing signed catalog, SHA-256
verification and bounded sparse extraction.

## Qualification

On `agent_house` (Intel UHD Graphics 770), the packaged worker ran as the
`ahvm-rust` service account, with the bundled graphics libraries. One 4-CPU,
8-GiB Omarchy VM booted with systemd, healthy desktop services and working HTTPS.
Hyprland reported VirGL/Intel hardware rendering. A 20-frame GPU readback probe
verified pixel values. VNC delivered a nonblank 1280×720 desktop and keyboard
input was confirmed in the guest. Stop/start restored the desktop from disk.
