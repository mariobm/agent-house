<p align="center">
  <a href="https://ahvm.app"><img src="https://ahvm.app/og-image-v1.png" alt="AHVM: Give your agents a machine of their own." width="1200"></a>
</p>

<h1 align="center">Agent House</h1>

<p align="center">Stateful Linux microVMs for coding agents, on the hardware you control.</p>

<p align="center">
  <a href="https://ahvm.app">Website</a> ·
  <a href="https://ahvm.app/docs/">Documentation</a> ·
  <a href="https://github.com/mariobm/agent-house/releases">Releases</a>
</p>

## What is AHVM?

AHVM gives your coding agents persistent Linux machines with interactive shells,
files, networking and snapshots. Install the client on your Mac or Linux machine;
your sandboxes run on a Linux server you control. No domain or public API port
is needed.

The default [Ubuntu image](docs/DEVELOPMENT-IMAGE.md) includes Node.js LTS, Bun,
Python, Git, build tools, Claude Code, Codex, OpenCode and Pi. Bring your own
provider credentials. Inside the guest, run `ahvm-dev` to use the developer account.

## 1. Install the client

```sh
curl -fsSL https://ahvm.app/install.sh | bash
export PATH="$HOME/.local/bin:$PATH"
```

Or use Homebrew:

```sh
brew install mariobm/ahvm/ahvm
```

Clients are available for Apple Silicon, Intel Macs and Linux x86_64. The curl
installer needs curl, gzip and Python 3. You can [inspect the script](https://ahvm.app/install.sh)
before running it.

## 2. Connect your Linux server

```sh
ahvm host add home --ssh root@YOUR_SERVER_IP --install
```

Existing SSH aliases work too: replace `root@YOUR_SERVER_IP` with your alias.
The server needs Linux x86_64, KVM, systemd, glibc 2.35+, Python 3, OpenSSL 3
and GNU tar. Use root or an account with passwordless sudo.

Installation downloads the runtime and Ubuntu image. Leave off `--install` to
connect an already installed server. The first host becomes your default;
`--host home` selects a host explicitly. SSH handles encryption and host-key
checks; the admin token is not saved on your laptop.

```sh
ahvm host list
ahvm host use home
```

## 3. Create a workspace

```sh
ahvm create dev --cpus 2 --memory 4096
ahvm exec dev -- bun --version
ahvm shell dev
```

Omit the name to let AHVM generate one: `ahvm create`.

Type `exit` to end Bash and return to your local terminal. Your VM and files
remain. Press `Ctrl-]` instead to detach while keeping the shell session alive.
Run `ahvm shell dev` to open a new shell, or `ahvm start dev` first if the VM
has stopped automatically while idle.

## 4. Files and checkpoints

```sh
printf 'Hello from AHVM\n' > hello.txt
ahvm files put dev ./hello.txt /workspace/hello.txt
ahvm files get dev /workspace/hello.txt ./download.txt
ahvm snapshot create dev before-change
ahvm stop dev
ahvm start dev
```

Uploads replace the guest file only when complete. Stop saves a local disk and
memory checkpoint; start resumes it. Idle-stop is automatic, and wake is explicit.
Crash recovery uses the latest checkpoint. Keep off-host backups for important data.

When you are finished, delete the VM and its working disk:

```sh
ahvm delete dev
```

## Cloud pilot

Invited accounts can connect the CLI with `ahvm login`, check their workspace with
`ahvm whoami`, and revoke access with `ahvm logout`. Hosted VM commands are coming
next. See [cloud login and headless setup](docs/CLOUD-LOGIN.md).

## Images

```sh
ahvm image available
ahvm image list
ahvm image pull ubuntu-dev
ahvm image default ubuntu-dev
ahvm create another-dev --image ubuntu-dev
```

Ubuntu is the automatic default. These are VM disks, not Docker images. Downloads
are verified against a signed catalog and cached on your server. Updating an
image affects future sandboxes; existing filesystems and snapshots stay unchanged.
See [building your own images](docs/IMAGE-PUBLISHING.md).

## Desktop

The desktop preview adds XFCE, Bash and Firefox to the Ubuntu development image.
It runs on the same Linux server without a GPU. With a desktop-capable release:

```bash
ahvm create dev-desktop --desktop
ahvm desktop dev-desktop
```

The first create downloads `ubuntu-desktop` to your saved SSH host. The Mac
client includes the native viewer. Closing its window leaves the VM running;
`ahvm stop dev-desktop` preserves files but discards the desktop session.
Remove it with `ahvm delete dev-desktop`. Desktop snapshots are not supported.
See the [desktop image guide](images/ubuntu-desktop/README.md).

## Upgrades

```sh
ahvm upgrade                 # Client installed with curl
brew upgrade ahvm            # Client installed with Homebrew
ahvm host upgrade home       # Server
```

Use the client upgrade command matching your installation. Server upgrades
preserve configuration and the selected image, check API health, and roll back
the runtime and database if the new version fails to start.

## More documentation

- [Remote hosts, images and upgrades](docs/REMOTE-HOSTS.md)
- [Server installation and maintenance](docs/RUST-INSTALL.md)
- [Files](docs/FILE-UPLOADS.md)
- [Preview ports and private network access](docs/NETWORK-ACCESS.md)
- [Recovery and rollback](docs/RUST-CUTOVER.md)

AHVM is an early-release product for evaluation; it is not yet qualified for
hostile multi-tenant workloads.

## Development

The runtime is Rust, using the pinned `libkrucible` fork of libkrun. Private
submodule access is required to build the VMM; see the [build guide](docs/RUST-INSTALL.md).

```sh
make build
make test
make check
```

Agent House originated as a fork of [Bhatti](https://github.com/sahil-shubham/bhatti).

## License

AHVM v0.1.0 and later editions designated under the [AHVM Community License](LICENSE)
are source-available. Community use is free while your company and its controlled
affiliates have total worldwide ARR of **USD 1 million or less**. Above that
threshold, a separate **paid commercial software license** is required; contact
[sales@ahvm.app](mailto:sales@ahvm.app). Hosting and support fees are separate.

This change is prospective. Existing Apache-2.0 grants and upstream/third-party
licenses remain valid. See [NOTICE](NOTICE) and the retained
[Apache-2.0 text](licenses/Apache-2.0.txt). See the
[licensing explanation](docs/LICENSING.md) for prior-release and third-party scope.

### Omarchy desktop preview

On a supported Linux GPU host:

```bash
ahvm create omarchy --image omarchy-desktop
ahvm desktop omarchy
```

The first create downloads the image on a saved SSH host. Defaults to 4 CPUs and
8 GiB RAM. See [setup, requirements and limitations](docs/OMARCHY-DESKTOP.md).
