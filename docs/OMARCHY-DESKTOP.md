# Omarchy desktop preview

Linux x86_64 hosts with an accessible Intel render device can run the adapted
Omarchy/Hyprland desktop. The Mac remains a client. Ubuntu remains the default.
GPU rendering is a preview for trusted workloads: VirGL executes in the worker
process and accesses the host GPU. It does not provide a separate renderer sandbox.

```bash
ahvm host upgrade home
ahvm image pull omarchy-desktop
ahvm create omarchy --image omarchy-desktop --no-shell
ahvm desktop omarchy
```

Self-hosted creation defaults to 4 CPUs and 8192 MiB RAM and enables desktop mode automatically.
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
Desktop connections default to **1280×720**. Use `ahvm desktop omarchy --resolution 1080p`
for **1920×1080**, or `--resolution 720p` to switch back. Hyprland stays at
60 Hz and scale 1; the viewer fits the desktop to its window. Resolution is
shared by viewers of the same VM. Switching sizes briefly reconnects the VNC
transport but keeps desktop applications running. Cold boots use the image's
720p default until the next desktop connection applies its requested size.

For network diagnostics, use `curl -4 -I https://example.com` for HTTPS and
`ping -4 -c 3 1.1.1.1` for ICMP echo. Public IPv4 ping requires a host gateway
v0.3.10 or newer; released v0.3.9 and older gateways only support TCP and DNS, so a
ping timeout on those versions does not mean Internet access is broken. See
[networking support and limits](STATUS-networking.md#isolation-and-lifecycle).
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

## AHVM Cloud preview

Cloud desktop access is enabled by the operator per workspace after host
qualification. The Cloud profile is fixed at **2 vCPUs / 8192 MiB RAM**, with the
image's **40 GiB logical disk** and replicated storage. Workspace and host quotas
still apply. It does not consume an Ubuntu spare, and normal Ubuntu Cloud VMs
keep their existing profile.

After the Cloud rollout and desktop entitlement are enabled:

```bash
ahvm use cloud
ahvm create omarchy --image omarchy-desktop --no-shell
ahvm desktop omarchy
```

The authenticated desktop connection wakes a stopped Cloud VM. Use the viewer
v0.3.9 or newer for Cloud TLS and cold-wake retry support. Earlier viewers
either lack a TLS crypto provider or time out after 10 seconds. Disk writes replicate asynchronously. A cold restart preserves
replicated files and opens a new desktop session, without restoring applications
from RAM. The connected viewer holds an activity guard; closing it allows the
normal idle-stop policy to apply. The current viewer connection is bounded to
one hour and can be reopened. Explicit stop/delete still takes precedence.

```bash
ahvm stop omarchy
ahvm desktop omarchy      # Cold wake and a fresh desktop session
ahvm delete omarchy
```

On 2026-09-22, one 2-vCPU / 8-GiB replicated Omarchy VM on `agent_house`
booted in 26.93 seconds after shared-base preparation. Stop took 1.58 seconds,
the explicit storage sync reached zero pending bytes, and cold start after
observed local eviction took 23.82 seconds. Two saved files survived with a new
kernel boot ID. The authenticated node WebSocket delivered a nonblank 1280×720
Hyprland desktop before and after recovery; VNC keyboard input was verified in
the guest and HTTPS succeeded. These are single-run node measurements, not
Cloud edge latency percentiles. The initial shared-base import took about ten
minutes including the first failed GPU-permission check; operators must prewarm
it before enabling user creates.

The Cloud edge routing, ownership, image pinning, quotas and wake forwarding are
covered by integration tests. Production Cloud deployment and the signed viewer
update follow the paired runtime and control-plane changes; this qualification
does not claim a production Cloud login-to-desktop test yet.
