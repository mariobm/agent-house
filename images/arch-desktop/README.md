# Managed Arch desktop image (experimental)

This adds Forge control and private VNC transport to the Arch/Hyprland GPU
experiment. It is not the default Ubuntu developer image and is not published.
The desktop runs as the `desktop` user with a Bash terminal. Omarchy is not yet
installed. Guest applications communicate with the host through explicit vsock;
VNC is a private Unix socket, never a TCP listener.

On an isolated Linux x86_64 build host, as root:

```bash
cargo build --manifest-path rust/Cargo.toml --release -p ahvm-forge
images/arch-desktop/prepare-image.sh /absolute/new-desktop.ext4 \
  "$PWD/rust/target/release/ahvm-forge"
```

The recipe reuses the pinned bootstrap and signed rolling packages from
`experiments/gpu/desktop/prepare-image.sh`. Supply a new destination; it refuses
to overwrite a disk. Do not mount or modify a backing image while a VM uses it.

Build the VMM with the branch's optional GPU feature and readback-fix fork pin,
as described in [the GPU experiment](../../experiments/gpu/README.md). Set
`AHVM_DESKTOP_IMAGE=/absolute/new-desktop.ext4` in the isolated daemon's environment,
and point `AHVM_VMM_BIN` at that GPU-enabled worker. The host needs the tested
VirGL/Mesa runtime and access to its render device. The default installed VMM
and daemon are not changed by building this image.

Use `ahvm create dev --desktop` and the [optional viewer](../../desktop-viewer/README.md).
Disk-only stop/start works; RAM snapshots, desktop fork and preview ports do not.
The integrated native-viewer test did not qualify guest internet access. This
image is for the desktop prototype, not general production use.
