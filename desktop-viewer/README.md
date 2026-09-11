# Optional AHVM desktop viewer

Experimental native window for `ahvm desktop dev`. This separate Cargo project
uses the operating system WebView and embeds noVNC. The ordinary CLI has no GUI
or browser dependencies. The viewer is not yet included in public installers.

## Build and use on macOS

With Rust and Node.js/npm installed, run from the repository root:

```bash
bash desktop-viewer/build.sh
cargo build --manifest-path rust/Cargo.toml --release -p ahvm-cli
```

Configure a host with the [desktop image](../images/arch-desktop/README.md), then:

```bash
ahvm create dev --desktop
ahvm desktop dev --viewer "$PWD/desktop-viewer/dist/AHVM Desktop.app/Contents/MacOS/ahvm-desktop"
```

The flag defaults to 2 CPUs and 4096 MiB. Explicit `--cpus` and `--memory` still
work. Without `--desktop`, existing image and resource defaults are unchanged.
The host must advertise desktop support; an older daemon cannot silently create
a regular VM instead. Desktop images are configured by the host, so omit `--image`.

For a shorter command, install `dist/ahvm-desktop` next to the CLI or on PATH.
Then `ahvm desktop dev` finds it automatically. The `.app` is currently unsigned;
release signing, notarization and automated optional installation remain pending.
Only macOS arm64 has been tested. The Linux WebKitGTK build is not qualified.

Closing the window leaves the VM running. A connected viewer holds an activity
guard, preventing idle suspension, for at most one hour. Reopen the viewer after
that limit. Close it before stopping or deleting the VM:

```bash
ahvm stop dev      # sync disk and stop; desktop/RAM state is discarded
ahvm start dev     # boot the same disk again
ahvm desktop dev
ahvm delete dev    # permanently remove the VM and its disk
```

Desktop snapshots/forks and preview ports are not supported in this prototype.
Normal VM snapshots, previews and shell behavior are unchanged.

## Measured size

macOS arm64 release builds on this branch: the CLI increased from 7,618,512
to 7,653,136 bytes, a 34,624-byte increase (33.8 KiB, 0.45%). The separate
viewer executable is 2,432,016 bytes (2.32 MiB), including the JavaScript
bundle. This excludes the desktop guest image and uses the system WebView;
other platforms and signed distribution sizes will differ.

## Transport

The existing CLI host selection, SSH tunnel and API authentication are reused.
The CLI hands connection settings to the helper through a private stdin pipe.
Credentials are not placed in argv, page URLs, JavaScript or generated assets.
The helper strips inherited AHVM token variables, binds a random loopback port,
uses an unpredictable page/stream capability and checks the WebSocket Origin.
The daemon checks token authentication and sandbox ownership before bridging
VNC over its private Unix socket and guest vsock. There is no public VNC port.

noVNC (MPL-2.0), its authors and pako license notices accompany the optional
bundle. Build outputs, node_modules, screenshots and logs are ignored by Git.

## Qualification, 2026-09-11

One 2-CPU / 4-GiB VM on `agent_house`, using an isolated daemon, was accessed
from the macOS arm64 native viewer. The window rendered Hyprland, typed a command
that created a guest file, disconnected without stopping the VM, and reconnected.
Disk markers survived stop/start. GPU acceleration and raw RFB pointer input were
also verified by the preceding [GPU experiment](../experiments/gpu/desktop/README.md).

Shortcut qualification is incomplete: automated Mac events delivered modifier
flags without separate modifier key events, so Ctrl+C/shifted input was not a
valid end-to-end physical-keyboard test. Check a real keyboard, alternate layouts,
clipboard, sustained desktop latency and reconnect UX before promoting this
prototype. No keyboard-event logging is included in the viewer.

The renderer runs in the VMM process. This is a trusted-workload experiment;
untrusted desktop isolation, Omarchy customization and release packaging remain
follow-up work. GPU snapshots are intentionally outside this milestone.
