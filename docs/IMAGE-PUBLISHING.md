# Build and publish a guest image

Images are x86_64 ext4 disks containing Linux userspace plus the static AHVM
forge guest agent and `/init.krun` entrypoint. The VM supplies its kernel;
a distro's normal bootloader/kernel installation is not used. Guest ABI 1
requires the current forge protocol, working mounts, PTY support, networking
setup and the entrypoint. Public images must contain no credentials, SSH host
keys, provider logins, build tokens, shell histories or machine-specific state.

## Build

Use `scripts/ubuntu-dev-rootfs.sh` on a Linux x86_64 build machine as root:

```sh
FORGE_BIN=/path/to/qualified/static/ahvm-forge \
  scripts/ubuntu-dev-rootfs.sh /tmp/ubuntu-dev.ext4
gzip -c /tmp/ubuntu-dev.ext4 > /tmp/ubuntu-dev.ext4.gz
```

The Ubuntu recipe lives in `images/ubuntu-dev/`: `versions.env` pins downloads,
`provision.sh` installs programs, and `init.krun` initializes the guest. For a
different program selection, create a separate profile and builder from this
recipe rather than overwriting the existing one. For Debian, Alpine, etc.,
replace the base filesystem and package provisioning, adapt init/mount setup,
and retain the static guest agent. Include Bash for `ahvm shell`; minimal
images can still launch another shell through `ahvm session create`.

Test the image locally on KVM before publishing: boot, exec, Bash/PTY, files,
networking, stop/start, snapshot/restore and worker recovery. Images must be
built from clean distro inputs, not exported from an authenticated developer
sandbox. The compressed and expanded sizes below are exact bytes, not `du`.

## Publish (maintainer only)

Use a new versioned key for every generation. Upload credentials stay in
`~/.config/ahvm-release/r2-publisher.json`; the signing key is separate at
`~/.config/ahvm-release/signing-key.pem`. Neither belongs in the image or repo.
The R2 token grants object read/write only in `ahvm-images`. Downloads are
public. The embedded public key verifies catalogs but cannot authorize uploads
or create signatures. Other Cloudflare account administrators may also have
write access; protect their credentials too.

```sh
python3 scripts/r2-publish.py /tmp/ubuntu-dev.ext4.gz \
  ubuntu-dev/NEW_VERSION/ubuntu-dev-amd64.ext4.gz --immutable
curl -fsSL https://images.ahvm.app/catalog.json -o /tmp/catalog-current.json
# Verify the existing signature and preserve its CLI/server entries.
python3 scripts/assemble-catalog.py /tmp/catalog-current.json /tmp/catalog-payload.json
```

Edit the payload's `images` map to add/update your profile, preserving all other
entries. Use a record like this, replacing every placeholder with actual data:

```json
{
  "version": "NEW_VERSION",
  "url": "https://images.ahvm.app/ubuntu-dev/NEW_VERSION/ubuntu-dev-amd64.ext4.gz",
  "sha256": "SHA256_OF_COMPRESSED_FILE",
  "size": 123,
  "unpacked_size": 17179869184,
  "guest_abi": 1
}
```

Sign, publish to a qualification channel, and test a fresh pull/create through
that channel before promoting the same signed catalog:

```sh
python3 scripts/sign-catalog.py /tmp/catalog-payload.json \
  ~/.config/ahvm-release/signing-key.pem /tmp/catalog-next.json
python3 scripts/r2-publish.py /tmp/catalog-next.json qualification.json
AHVM_CATALOG_URL=https://images.ahvm.app/qualification.json ahvm image pull PROFILE
ahvm create image-check --image PROFILE --cpus 2 --memory 4096
# Run the guest checks, then delete image-check.
# Re-fetch/check the current catalog before promotion if another publisher ran.
python3 scripts/r2-publish.py /tmp/catalog-next.json catalog.json
```

Coordinate catalog publication with other maintainers: it replaces the catalog
as a whole. Refresh its 90-day expiry even without new artifacts. After
promotion users run `ahvm image pull PROFILE`; `ahvm image default PROFILE`
selects it for future unnamed creates. Existing sandboxes keep their disk.
