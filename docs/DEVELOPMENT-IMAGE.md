# Ubuntu development image

The default `ubuntu-dev` image provides a working development environment. Linux x86_64/KVM is the qualified target. Start with 2 CPUs
and 4 GiB RAM; larger projects and multiple agents may need more.

Includes Ubuntu 24.04 LTS, Node.js 24 LTS with npm, Bun, Python 3 with pip/venv,
Git, SSH client, CA certificates, GCC/G++/make, pkg-config, ripgrep, fd, jq,
SQLite, rsync, curl, wget, unzip, tmux, nano and Vim. AI commands are `claude`,
`codex`, `opencode`, and `pi` (the Pi agent from pi.dev).

The current recipe pins **released OpenCode 2.0.18** from official npm package
**`@opencode/cli`**, replacing the older `opencode-ai` 1.18.30 package. Other
tool and OS pins are unchanged. The CLI and its native binary remain under
root-owned `/opt/ahvm-tools`; no server, provider login, password or conversation
state is started or saved during image provisioning. Existing VM disks are not
upgraded by publishing a new image.

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
Package installation runs as guest `ahvm`, then the installed tool tree becomes
root-owned. npm may warn about install scripts; the native platform optional
package still supplies the executable. No new broad script allowance is added,
and the gate verifies the installed executable rather than assuming a
postinstall script ran.

Provisioning mounts are confined to a temporary mount/PID namespace. The host's
packages and services are not modified. Output is published only after success;
existing output is refused. The image has a 16 GiB sparse filesystem (override
with `IMG_MB`). Logical capacity is not the compressed download size. Per-VM
disk copies and snapshots still consume host storage.

For a new native bundle, supply the resulting image to the normal packager:

```sh
AHVM_GUEST_IMAGE=/var/tmp/ubuntu-dev.ext4 scripts/package-rust.sh /tmp/ahvm-dev-bundle
```

Without `AHVM_GUEST_IMAGE`, packaging builds Ubuntu automatically (requires root
or sudo). `AHVM_GUEST_PROFILE=minimal` explicitly selects BusyBox for lightweight
tests. The public installer selects the Ubuntu development bundle.

## Use

Fresh installations configure the bundled Ubuntu image automatically: `ahvm create`
uses it without an image flag or provisioning step. Existing installations can
select it through `AHVM_BASE_IMAGE`. Changing the base does not upgrade existing
filesystems or snapshots.

```sh
ahvm create dev --cpus 2 --memory 4096
# Inside the automatically opened guest shell:
ahvm-dev
node --version
bun --version
python --version
claude
# Or: codex / opencode / pi
```

Newly built images provide an `ahvm` user. With a matching CLI, `ahvm shell`
and interactive `ahvm create` open Bash as that user in `/workspace`, with a
colored prompt. `sudo -i` opens a root shell when needed. Existing images are
not modified; older images without the shell entry point retain their previous
Bash behavior. Publishing the new image and CLI is required for rollout.

`ahvm-dev` also enters the `ahvm` account with `/workspace` as its directory.
For automation, use `ahvm exec dev -- ahvm-dev node --version`. The current API
still executes as guest root by default. The ahvm user has passwordless sudo
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

On **2026-09-26**, a clean image build with released **OpenCode 2.0.18** passed
the complete KVM gate in **14.40s**, using one disposable **2-vCPU/4-GiB**
sandbox. The run covered all existing development-tool, apt/npm/pip, HTTPS,
C compilation, PTY and stop/start persistence checks, plus the authenticated
loopback released-2 API checks below. The test daemon used the production DNS
resolver setting `127.0.0.53`. It made **zero model calls**; other tool/OS pins
were unchanged. The signed download passed the same full gate in **14.60s**;
its expanded SHA-256 matched the original build. Both test VMs were deleted.

The public signed catalog now advertises Ubuntu image generation **2026-09-26**
(about **1.10 GiB** compressed, **16 GiB** logical disk). The image is built
from clean Ubuntu inputs, not exported from an authenticated VM. Publishing
it does not change installed tools inside existing VM disks.

Production Cloud qualification also passed through the normal CLI with a
1-vCPU/2-GiB replicated VM: create (**6.44s**), authenticated OpenCode 2 API
as `ahvm`, stop/start (**5.59s**) and persistent files. The disposable Cloud VM
was deleted. These are individual image-rollout checks, not latency benchmarks.

The current `scripts/test-dev-image.py` additionally checks the exact OpenCode
pin against both the repository and guest `image-versions.env`, verifies the
installed package is `@opencode/cli`, and checks root ownership of the tool.
It starts a bounded foreground server as guest `ahvm` using temporary state and
a privately generated environment password, then verifies exact version at
`/api/info`, JSON `/openapi.json` released session operations, loopback-only
listening, and HTTP 401 for missing/wrong credentials. The temporary server
process group is terminated on success or failure. This adds **zero model
calls** and creates no provider login or agent conversation. The disposable
sandbox is deleted by the outer gate, including on failure.

Historical baseline: on `agent_house` (2026-09-10), `scripts/test-dev-image.py` passed in **19.50s**
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
