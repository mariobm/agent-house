# Optional AHVM desktop viewer

Experimental native window for `ahvm desktop dev`. This separate Cargo project
uses the operating system WebView and embeds noVNC. The ordinary CLI has no GUI
or browser dependencies. The macOS client release archive now includes it alongside the CLI. Focused keyboard forwarding is included from v0.2.3.

## Build and use on macOS

With Rust and Node.js/npm installed, run from the repository root:

```bash
bash desktop-viewer/build.sh
cargo build --manifest-path rust/Cargo.toml --release -p ahvm-cli
```

Configure a host with the [desktop image](../images/ubuntu-desktop/README.md), then:

```bash
ahvm create dev --desktop
ahvm desktop dev --viewer "$PWD/desktop-viewer/dist/AHVM Desktop.app/Contents/MacOS/ahvm-desktop"
```

The flag defaults to 2 CPUs and 4096 MiB. Explicit `--cpus` and `--memory` still
work. Without `--desktop`, existing image and resource defaults are unchanged.
The host must advertise desktop support; an older daemon cannot silently create
a regular VM instead. The default desktop image is `ubuntu-desktop`; saved SSH hosts pull it on first
use. A host may override the image for private qualification.

The curl installer, Homebrew formula and `ahvm upgrade` install the bundled
viewer next to the CLI on macOS, so `ahvm desktop dev` needs no separate install.
For source builds, copy `dist/ahvm-desktop` next to the CLI or use the explicit
path above. Linux server/client artifacts remain CLI-only until Linux viewer
dependencies are qualified. The `.app` is currently unsigned;
release signing and notarization remain pending.
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
to 7,660,224 bytes, a 41,712-byte increase (40.7 KiB, 0.55%). The separate
viewer executable is 2,432,016 bytes (2.32 MiB), including the JavaScript
bundle. This excludes the desktop guest image and uses the system WebView;
the combined client archive is about 4.04 MiB compressed. Other platforms
and signed distribution sizes will differ.

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

Ubuntu/XFCE qualification now also covers uppercase/punctuation and Ctrl+C in
the native window. A modifier-event repair handles event sources that provide
flags without separate modifier key events; physical events are left to noVNC.
International layouts, host clipboard integration and sustained frame-rate
benchmarks remain follow-up work. No keyboard-event logging is included.

The default Ubuntu/XFCE desktop uses software rendering. The separate
Hyprland experiment runs its GPU renderer in the VMM process and remains for
trusted workloads. Omarchy customization and GPU snapshots are outside this
milestone.

## Hosting VMs on this computer

The viewer is a client, not a virtualization backend. The current server
installer requires Linux x86_64, KVM and systemd. macOS can run the CLI and
viewer against a remote Linux host; Mac-local provisioning is not supported by
this release. A future `ahvm host add local --local --install` interface would
need a qualified macOS runtime, matching architecture images and local service
management. It is not necessary to SSH into a Mac to use the client.

### Focused keyboard forwarding (preview)

The desktop automatically captures delivered keyboard events when connected and
focused. There is no toggle. Both Mac Command keys map to Linux Super; the full
Control+Option+Shift+Command combination (Raycast Hyper/Caps Lock) is collapsed
to a single Super modifier. Hyper letter/digit shortcuts use the physical key
rather than the Shift/Option-generated glyph. Partial modifier release does not
leak Control/Alt/Shift into the guest. This conversion applies only on macOS.
Linux forwards its original modifiers, including Super and AltGr, without Hyper conversion.

Click outside the viewer to release. Ctrl+Option+Esc also releases until the
next focus or desktop click (Ctrl+Alt+Esc on Linux). Disconnect/close release all held keys. The header
shows focus/capture state.

macOS symbolic-hotkey suppression still requires Accessibility permission for
AHVM Desktop. Third-party global shortcuts such as Raycast may intercept keys
before they reach the viewer even with this permission. We do not change Raycast
bindings or install a global event tap. Keyboard layout translation remains
separate from shortcut capture. Full macOS/global-shortcut behavior and the
user's real Raycast Hyper remapper need manual qualification.
