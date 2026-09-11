# Ubuntu desktop

A separate image derived from `ubuntu-dev`: the same development tools and AI
CLIs, with XFCE, TigerVNC, a Bash terminal and Firefox added. It uses software
rendering and the ordinary GPU-free worker. The default image stays `ubuntu-dev`.

With a compatible daemon and a saved SSH host:

```bash
ahvm create dev --desktop
ahvm desktop dev
```

The first command installs the signed `ubuntu-desktop` image on that host if it
is missing. Later creates reuse the cached image. Defaults are 2 CPUs and 4096
MiB; the image has a 16-GiB logical disk with sparse/compressed distribution.
For a direct API connection, install it on the server first with
`ahvm image pull ubuntu-desktop`. `--image ubuntu-desktop` may accompany
`--desktop`; it does not change the default image for ordinary creates.

The desktop runs as `developer`, opens a Bash terminal in `/workspace`, and
inherits the existing development image's sudo policy. VNC and X11 expose no TCP
listener. The private VNC Unix socket is bridged over vsock and the authenticated
AHVM API; closing the native viewer leaves the VM running.

```bash
ahvm stop dev    # sync disk, discard RAM and desktop session
ahvm start dev   # boot the same disk
ahvm desktop dev
ahvm delete dev  # remove the VM and its disk
```

Snapshots/fork and preview ports remain unsupported for desktop VMs in this
milestone. Browser networking uses the existing managed gateway policy. This is
not a GPU desktop; the [Hyprland experiment](../arch-desktop/README.md) remains
separate.

## Build

Run as root on Linux x86_64:

```bash
FORGE_BIN=/path/to/static/ahvm-forge \
  scripts/ubuntu-desktop-rootfs.sh /new/path/ubuntu-desktop.ext4
```

This first builds `ubuntu-dev`. To extend an existing **unbooted, trusted template**
without rebuilding its tools, also set `UBUNTU_DEV_IMAGE=/path/ubuntu-dev.ext4`.
The builder copies it and mounts only the copy in a private namespace; never
supply a user's working VM disk as a release template. Existing outputs are
refused. Ubuntu packages are signed, and Firefox comes from the
[official Mozilla APT repository](https://support.mozilla.org/en-US/kb/install-firefox-linux)
with a checked signing-key fingerprint. Resolved packages are recorded inside
the image. Package versions can change when rebuilding; qualify each new image.

For private qualification, `AHVM_DESKTOP_IMAGE=/absolute/image.ext4` overrides
the cache lookup. Ordinary Ubuntu desktop needs no other daemon GPU setting.

## Qualification (2026-09-11)

The signed `2026.09.11` image is published on `images.ahvm.app`: 1,284,249,206
compressed bytes (about 1.20 GiB), with a 16-GiB logical disk. Validation used
one 2-CPU / 4-GiB VM at a time on `agent_house`, with the standard GPU-free
v0.2.1 worker and a separate daemon running this branch.

Verified: first-use pull through a saved SSH host, valid create JSON, immediate
native Mac viewer connection, uppercase/punctuation and Ctrl+C, Firefox loading
HTTPS through the managed gateway, private VNC mode 0600 with no VNC/X11 TCP
listener, and files surviving disk-only stop/start. Firefox plus XFCE reported
about 750 MiB used guest RAM in the smoke test; this is not a workload benchmark.
The normal `ubuntu-dev` default was unchanged. Test VMs and the separate daemon
were cleaned up; the downloaded desktop image remains cached for future use.

The CLI/daemon feature is still on PR #32 until its release is published.
