# GPU acceleration spike

Experimental, opt-in Linux VirGL rendering through the existing libkrucible
fork. This is a standalone VMM probe, not desktop support in the daemon or CLI.
No production image, installed runtime, service permissions or submodule pin
is changed by this branch. The default build still excludes GPU support.

## Result on agent_house

One standalone 2-vCPU / 4-GiB VM at a time, on an Intel i5-12500 with UHD 770:

- Host render node: `/dev/dri/renderD128`; virglrenderer 1.2.0, Mesa 26.0.8.
- Guest: a disposable copy of Ubuntu 24.04 dev, Mesa 25.2.8, bundled libkrunfw 5.
- Renderer: `virgl (Mesa Intel(R) UHD Graphics 770 (ADL-S GT1))`.
- OpenGL ES 3.2 and EGL 1.5; `/dev/dri/card0` and `renderD128` appear in the guest.
- Twenty alternating framebuffer clears/readbacks validate their pixel values.
  The first corrected run took 9.07 ms total. Three further boots took
  9.07, 7.97 and 9.04 ms. These are tiny 32×32 offscreen operations, not desktop frame rates.
- The first EGL/surfaceless-only configuration waited about 15 seconds for a
  fence. The GPU worker does not poll VirGL fences; enabling THREAD_SYNC and
  ASYNC_FENCE_CB removes that delay. No fork patch was needed.
- GPU-enabled Linux build and clippy pass; the default build still checks.
- GPU snapshot/restore and control sockets are rejected before boot. Renderer
  state has not been qualified for checkpointing, so the usual lifecycle must
  not silently claim support.

The probe rejects software renderer names, verifies pixels and bounds total
readback time to two seconds, catching the initial fence stall. Console output
is tagged `ERROR init_or_kernel` by the existing firmware console logger even
for ordinary successful stdout; use the probe markers and exit status.

## Reproduce

Use a Linux KVM host with its Intel Mesa driver and development packages for
virglrenderer, libepoxy, libdrm, EGL/GBM and libclang. Build from the repo root:

```sh
LIBRARY_PATH=/opt/ahvm-rust/lib cargo build --manifest-path rust/Cargo.toml \
  --release --locked -p ahvm-vmm --features gpu
```

As root, prepare a new disposable disk from the existing Ubuntu dev image.
The script uses a private mount namespace and never changes the source disk.
Do not run it on an existing sandbox disk.

```sh
# Optional: a faster Ubuntu mirror, still verified with Ubuntu archive keys.
export AHVM_GPU_APT_MIRROR=https://mirror.hetzner.com/ubuntu/packages
experiments/gpu/prepare-image.sh \
  /var/lib/ahvm-images/default.ext4 /tmp/ahvm-gpu-test.ext4
AHVM_VMM_BIN="$PWD/rust/target/release/ahvm-vmm" \
  experiments/gpu/run.sh /tmp/ahvm-gpu-test.ext4
```

The runner boots once, checks `GPU_RENDER_OK` and `GPU_PROBE_EXIT=0`, and has a
60-second timeout. The prepared disk remains for repeat runs. JSON specs and
console logs are temporary. The bundled kernel starts `/init.krun`, which the
probe image replaces with its test entrypoint.

## Boundaries and next steps

The follow-up [Hyprland desktop experiment](desktop/README.md) now verifies
hardware-rendered Wayland, VNC capture, keyboard and pointer input. GPU mode
now adds a virtual scanout to enable KMS and disables the legacy TSI INET
fallback. Omarchy, clipboard, audio, Vulkan, desktop performance and the
packaged viewer remain unqualified.

GPU rendering currently runs in the VMM process, with access to the host render
node. Production support needs an explicit isolation/permission model and
hardware/driver qualification. Do not grant the installed daemon render access
or expose this experiment to untrusted guest workloads as part of this spike.
The native test binary links host graphics libraries; it is not a portable
release artifact. GPU checkpoints remain disabled until a supported design
and recovery tests exist.
