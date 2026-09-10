# Ubuntu development image

The optional `ubuntu-dev` image adds a working development environment to the
minimal BusyBox image. Linux x86_64/KVM is the qualified target. Start with 2 CPUs
and 4 GiB RAM; larger projects and multiple agents may need more.

Includes Ubuntu 24.04 LTS, Node.js 24 LTS with npm, Bun, Python 3 with pip/venv,
Git, SSH client, CA certificates, GCC/G++/make, pkg-config, ripgrep, fd, jq,
SQLite, rsync, curl, wget, unzip, tmux, nano and Vim. AI commands are `claude`,
`codex`, `opencode`, and `pi` (the Pi agent from pi.dev).

## Build

Use a Linux x86_64 build host with root, curl, Python 3, binutils, util-linux,
tar/xz and e2fsprogs. No Docker is needed. Supply the static forge from the same
runtime revision you intend to package:

```sh
sudo FORGE_BIN=/opt/ahvm-rust/bin/ahvm-forge \
  scripts/ubuntu-dev-rootfs.sh /var/tmp/ubuntu-dev.ext4
```

The builder verifies the Ubuntu and Node archive checksums. Versions resolved
from official sources are pinned in `images/ubuntu-dev/versions.env`; refreshing
them is an explicit change. Ubuntu security updates are applied at build time,
so builds are **not byte-for-byte reproducible**. The exact Ubuntu package list,
tool versions and forge checksum are recorded under `/usr/local/share/ahvm`;
the resolved npm dependency lock is `/opt/ahvm-tools/package-lock.json`.

Provisioning mounts are confined to a temporary mount/PID namespace. The host's
packages and services are not modified. Output is published only after success;
existing output is refused. The image has a 16 GiB sparse filesystem (override
with `IMG_MB`). Logical capacity is not the compressed download size. Per-VM
disk copies and snapshots still consume host storage.

For a new native bundle, supply the resulting image to the normal packager:

```sh
AHVM_GUEST_IMAGE=/var/tmp/ubuntu-dev.ext4 scripts/package-rust.sh /tmp/ahvm-dev-bundle
```

Without `AHVM_GUEST_IMAGE`, packaging keeps the minimal image. The public
early-access installer still selects its published minimal bundle until a
development bundle is explicitly published.

## Use

Configure `AHVM_BASE_IMAGE` in the daemon's environment to select the image for
new sandboxes. Keep the old image and use a fresh sandbox; changing the base does
not upgrade existing filesystems or snapshots.

```sh
ahvm create dev --cpus 2 --memory 4096
ahvm shell dev
# Inside the guest:
ahvm-dev
node --version
bun --version
python --version
claude
# Or: codex / opencode / pi
```

`ahvm-dev` enters the `developer` account with `/workspace` as its directory.
For automation, use `ahvm exec dev -- ahvm-dev node --version`. The current API
still executes as guest root by default. The developer has passwordless sudo
**inside the VM**; it is a convenience account, not a tenant security boundary.
Forge remains PID 1; systemd and an SSH server are not running in the guest.

No keys, logins or paid model access are included. Authenticate using each
tool's own workflow inside your sandbox. For remote browser authentication,
use the tool's device-code flow where available; a localhost callback refers
to the guest, not your laptop. User credentials persist in that sandbox and
its snapshots; do not redistribute a snapshot after signing in.

Use `python -m venv .venv` for Python dependencies rather than changing Ubuntu's
system Python. Tools are installed under `/opt/ahvm-tools`; their exact versions
can be refreshed by rebuilding the image. No update daemon is installed.

## Qualification

On `agent_house` (2026-09-10), `scripts/test-dev-image.py` passed in **19.50s**
with one 2-vCPU/4-GiB sandbox: all four AI CLI version commands as `developer`,
Node/Bun/Python, apt/npm/pip installs, HTTPS, C compilation, a PTY, and
stop/start with installed packages and files intact. The sandbox was deleted
afterward. The image allocated approximately **2.3 GiB** on the build host.
Actual authenticated AI workloads are the next user acceptance step; this gate
does not send model requests or verify provider logins.

Run against an empty disposable daemon configured with this image:

```sh
AHVM_ENDPOINT=http://127.0.0.1:18880 \
AHVM_TOKEN_FILE=/etc/ahvm-test-dev/admin.token \
  python3 scripts/test-dev-image.py /opt/ahvm-test-dev/bin/ahvm
```
