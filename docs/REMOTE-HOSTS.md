# Remote hosts, images and upgrades

The standalone client runs on macOS (Apple Silicon or Intel) and Linux x86_64.
The VM server requires Linux x86_64, KVM and systemd. The server API stays on
loopback; normal client commands open an authenticated OpenSSH tunnel.

```sh
curl -fsSL https://ahvm.app/install.sh | bash
# Or: brew install mariobm/ahvm/ahvm
ahvm host add home --ssh root@192.168.1.50 --install
ahvm create dev --cpus 2 --memory 4096
ahvm exec dev -- bun --version
ahvm shell dev
```

The curl installer uses `~/.local/bin` (override `AHVM_BIN_DIR`) and prints the
PATH change if needed. It requires curl, gzip and Python 3. Host provisioning
requires root or passwordless sudo, Python 3, OpenSSL 3 and GNU tar on the
server. Existing SSH aliases, keys, agents and host-key verification apply.
AHVM does not disable host-key checking or copy your private SSH keys.

The first saved host is the default. `ahvm host use home` selects another;
`--host home` overrides it for a command. `ahvm host list` shows destinations,
and `ahvm host remove home` removes only the client entry, never server data.
Host config is stored in `~/.config/ahvm/hosts.json` (`AHVM_CONFIG_DIR` override).
Tokens stay in the server's protected `/etc/ahvm-rust/admin.token`; the client
reads one through SSH for each command and does not save it locally.

Sandbox names are scoped to the selected host. `ahvm create` generates a
`vm-` name; `ahvm create dev` uses `dev` as its name and ID. Explicit
`--endpoint` / `AHVM_ENDPOINT` selects the direct API instead of a saved default;
combining an explicit endpoint with `--host` is rejected.

## Images

```sh
ahvm image available             # Published catalog
ahvm image list                  # Cached on the selected host
ahvm image pull ubuntu-dev       # Download/update the local alias
ahvm image default ubuntu-dev
ahvm create dev --image ubuntu-dev
```

These are AHVM ext4 guest disks, not Docker/OCI images. Dockerfile import and
Docker-in-guest qualification are separate future work. The image cache is
`/var/lib/ahvm-images`; only the host administrator writes it. A server without
a saved host runs image commands locally. Provisioning fetches Ubuntu once.
The first pulled image becomes the default. Pulling a new generation updates
its named alias; `image default` selects that generation for unnamed creates.
Existing VMs and snapshots are never rewritten by an image update.

The R2 catalog at `https://images.ahvm.app/catalog.json` is Ed25519-signed and
expires. The embedded public key verifies metadata before use. Downloads
must match both signed SHA-256 and byte counts; expanded disks are size-bounded
and sparse. Incomplete files are temporary and never become a cache entry.
Image records declare the guest-agent ABI, which the CLI and daemon check.
The current guest ABI is 1; this is a publisher-maintained compatibility
contract, not a general guarantee for arbitrary imported disks.

## Upgrades

```sh
ahvm upgrade                    # Standalone client
brew upgrade ahvm               # Homebrew client
ahvm host upgrade home          # Explicit server upgrade
```

Standalone upgrades verify the signed catalog and retain `ahvm.previous`
next to the executable. Homebrew installations are never overwritten by the
self-updater. A CLI installed as part of the server is upgraded with its host.

The host updater supports the standard `/opt`, `/etc` and `/var/lib/ahvm-rust`
installation. It verifies the bundle, preserves configuration and the guest
image, restarts the daemon, and checks authenticated API health. Workers
survive the daemon restart. A failed candidate restores the prior runtime and
SQLite files; the previous runtime remains under `/opt/ahvm-rust.previous`.
Custom paths/listen addresses require manual upgrades. State-format or VMM
snapshot compatibility changes need an explicit migration before release.

Interactive commands show cached update notices on stderr. Checks run at most
once a day in a separate process and never block the foreground command. JSON
and redirected stderr suppress notices. Set `AHVM_NO_UPDATE_CHECK=1` to opt out.
Initial curl installation trusts the HTTPS bootstrap and its download digest;
subsequent updates enforce the public key embedded in the installed client.

## Publishing a release

Build assets through the manual `Build release assets` workflow, or run
`scripts/package-distribution.py` on each qualified platform. It emits separate
client downloads, a server-only archive, and per-platform metadata. The Linux
packager refuses dependencies requiring glibc newer than 2.35. Guest images are
built and qualified separately, then published under immutable R2 keys.

After merging and qualification, download the three workflow artifacts into one
directory and run:

```sh
scripts/publish-release.py /path/to/dist 0.2.1 FULL_QUALIFIED_COMMIT_SHA
```

The publisher checks artifact hashes, creates the GitHub release, signs and
publishes the update catalog, and updates the Homebrew tap through a PR. It uses
local GitHub authentication, `~/.config/ahvm-release/signing-key.pem`, and the
bucket-scoped `r2-publisher.json` in the same private directory. Never commit
those files. Back up the signing key securely; the embedded public key is in
`packaging/keys/releases.pem`. Merely creating a release through GitHub's UI
does not promote it into the signed catalog or update Homebrew.

`AHVM_CATALOG_URL` selects a signed qualification channel without weakening
signature verification. Normal installations use the production catalog.
Refresh/re-sign the catalog before its 90-day expiry even if no new release is
planned. A new guest or state ABI requires explicit compatibility qualification;
automatic migration is not inferred from a version number.
