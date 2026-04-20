# Rootfs Tiers

Tiers are pre-built ext4 root filesystem images that ship as base environments
for sandboxes. Each tier builds on `minimal` and adds specific tooling.

| Tier | Description | Approx Size |
|------|-------------|-------------|
| `minimal` | Bare Ubuntu 24.04 | ~200MB |
| `browser` | + Chromium/Playwright | ~600MB |
| `docker` | + Docker Engine | ~550MB |
| `computer` | + Full desktop (KasmVNC + XFCE + Chromium) | ~1.5GB |

## How tiers are discovered

The server **auto-discovers** tiers at startup by globbing for
`rootfs-*-{arch}.ext4` in the images directory (`/var/lib/bhatti/images/`).
Any file matching the pattern is registered as a built-in admin image.
There is no hardcoded tier list in the server — drop a new rootfs file and
it appears in `bhatti image list` on next restart.

## Installing additional tiers on an existing server

By default, the install script only downloads the single tier configured in
`/etc/bhatti/config.yaml`. Use `BHATTI_TIERS` to pull additional tiers:

```bash
# Install all available tiers
curl -fsSL bhatti.sh/install | sudo BHATTI_TIERS=all bash

# Install specific tiers
curl -fsSL bhatti.sh/install | sudo BHATTI_TIERS=computer,browser bash
```

The server discovers the new rootfs files on restart and registers them
automatically. No config changes needed — the config only controls which
tier is the default for `bhatti create` when no `--image` is specified.

## Adding a new tier

### 1. Create the tier script

Add `scripts/tiers/<name>.sh`. This runs inside a chroot during
`build-tier.sh`. It receives these env vars:

- `$MOUNT` — chroot mount point
- `$ARCH` / `$DEB_ARCH` — target architecture
- `$AGENT` — path to lohar binary
- `$SCRIPT_DIR` — path to `scripts/`

Most tiers source minimal first:

```bash
#!/bin/bash
set -euo pipefail
"$SCRIPT_DIR/tiers/minimal.sh"
# ... install your packages ...
```

### 2. Register in `scripts/build-tier.sh`

Add a size default to the `case` statement:

```bash
case "$TIER" in
    minimal)  SIZE_MB="${SIZE_MB:-512}" ;;
    browser)  SIZE_MB="${SIZE_MB:-2048}" ;;
    docker)   SIZE_MB="${SIZE_MB:-2048}" ;;
    computer) SIZE_MB="${SIZE_MB:-4096}" ;;
    new-tier) SIZE_MB="${SIZE_MB:-1024}" ;;  # ← add this
    *) echo "unknown tier: $TIER" >&2; exit 1 ;;
esac
```

### 3. Add to CI release matrix

In `.github/workflows/release.yml`, add the tier name to the rootfs job matrix:

```yaml
tier: [minimal, browser, docker, computer, new-tier]
```

Bump `timeout-minutes` if the tier has a heavy build (desktop packages, etc.).

### 4. Add to the install script menu

In `scripts/install.sh`, update the interactive tier selection prompt and its
`case` mapping so users can pick it during `install.sh`:

```bash
echo "    5) new-tier — description (~size)"
# ...
case "${tier_choice:-1}" in
    5) tier="new-tier" ;;
esac
```

Also update the `BHATTI_TIER` env var comment at the top of the file.

### 5. That's it

The server picks up the new rootfs automatically — no Go code changes needed.

## Checklist

```
[ ] scripts/tiers/<name>.sh          — tier build script
[ ] scripts/build-tier.sh            — SIZE_MB default in case statement
[ ] .github/workflows/release.yml    — add to matrix.tier
[ ] scripts/install.sh               — interactive menu + BHATTI_TIER comment
[ ] scripts/install.sh               — add to ALL_KNOWN_TIERS in do_server_update()
```
