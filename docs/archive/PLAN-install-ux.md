# Install & Update UX Overhaul

The install script (`scripts/install.sh`) is the primary entry point for
every bhatti user — CLI users, server operators, CI pipelines. It works,
but has accumulated UX issues as the project grew from one tier to four,
and from "install once" to "install and update."

---

## Current State

The script handles four flows:

1. **macOS** → install/update CLI binary
2. **Linux fresh** → prompt CLI vs server, prompt tier, install
3. **Linux CLI update** → update CLI binary
4. **Linux server update** → update all server components

Configuration is via `BHATTI_` env vars only (`BHATTI_MODE`, `BHATTI_TIER`,
`BHATTI_TIERS`), designed for the piped `curl | bash` case. No `--flag`
parsing for direct execution.

`bhatti update` is a CLI command that shells out to `curl | bash` with
`BHATTI_MODE=cli` hardcoded — it only ever updates the CLI binary.

`bhatti version` checks CLI vs server version via the `X-Bhatti-Version`
response header. It shows an "Update available" notice when the CLI is
behind the server. But it never checks whether the server itself is
behind the latest GitHub release.

---

## Problems

### 1. `bhatti update` silently does the wrong thing on servers

A server admin running `bhatti update` gets only a CLI binary update.
Firecracker, lohar, kernel, and rootfs are silently skipped because the
command hardcodes `BHATTI_MODE=cli`. The admin thinks they're up to date.
This is the biggest footgun.

### 2. No visibility into available releases

`bhatti version` compares CLI vs server, but never checks GitHub releases.
A server operator running v1.6.3 with CLI v1.6.3 sees matching versions
and thinks everything is current, even if v1.6.5 is out. There's no
mechanism to surface available updates for the server itself.

| Scenario | What it checks | What it misses |
|---|---|---|
| Remote CLI behind server | ✅ CLI vs server header | — |
| Remote CLI ahead of server | Shows both, no warning | — |
| Server admin, CLI+server in sync | Shows matching versions | ❌ No check against latest release |
| Server admin, CLI+server both stale | Shows matching versions | ❌ Looks "up to date" |

### 3. Root check comes after interactive prompts

On a fresh Linux install, the user answers two prompts (CLI vs server,
which tier) before `do_server_install()` checks `$(id -u) -eq 0`. They
waste time, then get rejected. The root check should happen the moment
the user chooses "server."

### 4. No `--flag` parsing

The script only accepts `BHATTI_` env vars. Running it directly looks like:

```bash
sudo BHATTI_TIERS=all ./scripts/install.sh
```

Every other install script in the ecosystem (rustup, Homebrew, Deno, nvm)
accepts `--flags` when run directly:

```bash
sudo ./scripts/install.sh --tiers all
```

Env vars should remain as the CI/piped fallback. Flags override env vars.

### 5. No way to discover or add tiers post-install

After a fresh server install with `minimal`, the user isn't told other
tiers exist. `BHATTI_TIERS` was added but is undocumented outside of
`docs/tiers.md`. A server admin who wants to add the `computer` tier has
to know the exact env var name.

### 6. Unclear update story

The README has one sentence: "Re-running updates an existing installation."
There's no `--help` on the install script, no version comparison output,
and `bhatti update` doesn't mention it's CLI-only. Server operators have
no documented update path beyond "run the install command again."

### 7. Non-systemd daemon not detected on update

`do_server_update` checks `systemctl is-active bhatti` to decide whether
to stop/restart the daemon. If someone runs `bhatti serve` in a foreground
terminal (no systemd), the script replaces the binary on disk but prints a
generic "Restart the daemon to use the new version" without checking
whether the daemon is actually running. It should detect the running
process and warn with a PID.

Note: `kill`-ing a manual `bhatti serve` is safe — it handles SIGINT and
SIGTERM identically to `systemctl stop` (drains connections, snapshots all
VMs, cleans up). But we avoid auto-killing because the user would lose
their terminal session and the new process would need to be started in
the same context. Detect and warn is the right call.

### 8. `bhatti update` doesn't forward flags to install script

After Fix 1, `bhatti update` on a server triggers `do_server_update()`.
But if someone runs `sudo bhatti update --tiers all`, the `--tiers` flag
is not forwarded to the install script. The `updateCmd` doesn't accept
or pass through any flags.

### 9. No rollback or partial update recovery

`do_server_update` replaces components sequentially: Firecracker → bhatti
→ lohar → kernel → rootfs. If the script dies at step 4 (OOM, network
drop, disk full, `kill -9`), you have a new Firecracker + new bhatti
binary + new lohar but an old kernel and old rootfs. On restart, there's
no guarantee these versions are compatible.

There is no:
- Backup of previous binaries before overwriting
- Post-download verification that the new binary executes
- Any recovery path for the user

### 10. Binary replacement not atomic across filesystems

`install_bhatti_binary` downloads to `/tmp/bhatti.tmp` then
`mv`s to `/usr/local/bin/bhatti`. If `/tmp` and `/usr/local/bin` are on
different filesystems (extremely common — `/tmp` is often tmpfs), `mv`
falls back to copy + delete, which is not atomic. A power failure
mid-copy = corrupted binary.

### 11. macOS quarantine attribute blocks first run

On macOS, `curl`-downloaded binaries get the `com.apple.quarantine`
extended attribute. First execution shows a scary "cannot be opened
because it is from an unidentified developer" dialog. The install script
doesn't strip it.

### 12. Shell completions not mentioned on install

`bhatti setup` suggests shell completions, but the install script doesn't.
Tab-completing sandbox names is a high-value first-use experience that
most users never discover.

---

## Fixes

### Fix 1: Make `bhatti update` server-aware

`bhatti update` currently hardcodes `BHATTI_MODE=cli`. Instead, it should
let the install script auto-detect:

```go
// Current:
install.Env = append(os.Environ(), "BHATTI_MODE=cli")

// Fixed — let install.sh detect server vs CLI:
install.Env = os.Environ()
```

On a server (config file exists), the install script runs
`do_server_update()`. On a CLI-only machine, it runs `do_cli_install()`.
On macOS, the install script always runs `do_cli_install()` regardless —
macOS is unaffected by this change.

If on a server without root, it fails with the existing "server update
requires root" message and suggests `sudo` — better than silently doing
a partial update. But the error currently comes from inside the install
script, after curl has already downloaded and started executing. Add an
upfront root check in the Go command before shelling out:

```go
// Before shelling out to install.sh:
if !cliOnly && isServer() && os.Getuid() != 0 {
    return fmt.Errorf("server update requires root\n  Re-run with: sudo bhatti update")
}
```

This avoids downloading the install script just to fail immediately.

Add a `--cli-only` flag for the rare case where someone explicitly wants
just the binary on a server host.

Update help text — currently says "Update bhatti CLI to the latest version",
should reflect the new behavior:

```go
var updateCmd = &cobra.Command{
    Use:   "update",
    Short: "Update bhatti to the latest version",
    Long: `Update bhatti to the latest release. On a server, updates all
components (bhatti, Firecracker, lohar, kernel, rootfs). On a CLI-only
machine, updates just the binary.

Use --cli-only to update only the binary on a server.
Use --tiers to install additional rootfs tiers during the update.`,
    Example: `  bhatti update                   # auto-detect CLI vs server
  sudo bhatti update               # server update (requires root)
  sudo bhatti update --tiers all   # server update + pull all tiers
  bhatti update --cli-only         # binary only, even on a server`,
}
```

Accept and forward `--tiers` and `--cli-only` to the install script:

```go
RunE: func(cmd *cobra.Command, args []string) error {
    cliOnly, _ := cmd.Flags().GetBool("cli-only")
    tiers, _ := cmd.Flags().GetString("tiers")

    // ...shell out to install.sh...

    env := os.Environ()
    if cliOnly {
        env = append(env, "BHATTI_MODE=cli")
    }
    if tiers != "" {
        env = append(env, "BHATTI_TIERS="+tiers)
    }
    install.Env = env
    // ...
}
```

#### Version coordination

`bhatti update` (and the install script) always pulls the **latest
GitHub release** via `resolve_latest_version()`. There is no version
pinning — you can't say "update to v1.6.3 specifically."

This is fine for now because:
- The CLI and server are the same binary (`bhatti`). On a server,
  `do_server_update` updates the binary that `bhatti serve` runs,
  so CLI and server stay in sync after a restart.
- On a remote CLI machine, the CLI may be newer than the server.
  This is guarded by `X-Bhatti-Min-CLI` (server rejects too-old CLIs)
  and is generally safe because the API is backward-compatible within
  a major version.
- `do_server_update` already has a major version crossing guard that
  prompts for confirmation before upgrading across major versions.

Output for each case:

```
# CLI-only machine:
Updating bhatti CLI
  v1.6.4 → v1.6.5
  ✓ bhatti v1.6.5 → /usr/local/bin/bhatti

# Server (with root):
Updating bhatti server (browser tier)
  v1.6.4 → v1.6.5
  ✓ Firecracker 1.14.0 + jailer (up to date)
  ✓ bhatti v1.6.5
  ✓ lohar (4.1M)
  ✓ kernel (8.2M)
  ✓ rootfs browser (600M)

# Server (without root):
error: server update requires root
  Re-run with: sudo bhatti update

# Already up to date:
✓ bhatti v1.6.5 is already up to date
```

This output already comes from the install script — `bhatti update`
pipes stdout/stderr through, so no extra work needed there.

### Fix 2: Release check in `bhatti version`

`bhatti version` already makes a request to the server. Add a second
check: the CLI itself queries the GitHub releases API directly to find
the latest version. This is a lazy, on-demand check — no background
goroutines, no server-side changes, no caching infrastructure.

`bhatti version` is an explicit "tell me about versions" command, so an
extra ~200ms HTTP call is acceptable. This is what `npm outdated`,
`pip`, and `terraform version` do.

**Implementation:**

- `bhatti version` makes a non-blocking call to
  `api.github.com/repos/.../releases/latest` with a 2s timeout.
- If the call succeeds, compare against both the CLI version and the
  server version (from `X-Bhatti-Version`).
- If the call fails (air-gapped, GitHub down, timeout), skip silently.
  No errors, no noise.
- Cache the result to `~/.bhatti/.latest-version` with a timestamp.
  Reuse cached value if < 1 hour old to avoid redundant calls. This
  means the check fires at most once per hour regardless of how often
  `bhatti version` is run.

No server-side changes needed. No background goroutine. No new
response headers.

**Output:**

```
# Everything up to date:
bhatti v1.6.5
api: https://api.bhatti.sh
server: v1.6.5

# CLI or server behind:
bhatti v1.6.3
api: https://api.bhatti.sh
server: v1.6.3

Update available: v1.6.3 → v1.6.5 (bhatti update)
```

Also surface in `bhatti admin status`. Air-gapped / GitHub down = no
notice, no error. Update notices only appear on `bhatti version` and
`bhatti admin status` — never on regular commands.

### Fix 3: Root check before prompts

Move the root check to fire immediately after the user selects "server,"
before the tier prompt:

```bash
case "$mode" in
    server)
        # Check root BEFORE asking for tier
        [ "$(id -u)" -eq 0 ] || die "server installation requires root" \
                                    "Re-run with:" \
                                    "  curl -fsSL bhatti.sh/install | sudo bash"
        # ... then prompt for tier ...
```

### Fix 4: Add `--flag` parsing to install script

Parse flags before `main()`. Flags override env vars. Only the flags
that already exist as env vars:

```bash
while [ $# -gt 0 ]; do
    case "$1" in
        --tier)    BHATTI_TIER="$2"; shift 2 ;;
        --tier=*)  BHATTI_TIER="${1#--tier=}"; shift ;;
        --tiers)   BHATTI_TIERS="$2"; shift 2 ;;
        --tiers=*) BHATTI_TIERS="${1#--tiers=}"; shift ;;
        --mode)    BHATTI_MODE="$2"; shift 2 ;;
        --mode=*)  BHATTI_MODE="${1#--mode=}"; shift ;;
        --force)   BHATTI_FORCE=1; shift ;;
        --quiet)   QUIET=1; shift ;;
        --verbose) VERBOSE=1; set -x; shift ;;
        --help|-h) usage; exit 0 ;;
        *) die "unknown flag: $1" ;;
    esac
done
```

Gate output functions on `QUIET`:

```bash
info()    { [ "${QUIET:-}" = "1" ] && return; printf "  ${DIM}%s${RESET}\n" "$*"; }
heading() { [ "${QUIET:-}" = "1" ] && return; printf "\n${BOLD}==> %s${RESET}\n" "$*"; }
success() { [ "${QUIET:-}" = "1" ] && return; printf "  ${GREEN}✓${RESET} %s\n" "$*"; }
```

`--quiet` suppresses all output (exit code only — for CI pipelines).
`--verbose` enables `set -x` (shows curl commands, paths, checksums —
for debugging failed installs).

Add a `usage()` function with `--help`:

```bash
usage() {
    cat <<EOF
Usage: install.sh [flags]

Flags:
  --tier <name>       Tier for fresh install (minimal, browser, docker, computer)
  --tiers <list|all>  Additional tiers to install on update (comma-separated or "all")
  --mode <cli|server> Skip install type prompt
  --force             Skip major version upgrade confirmation
  --quiet             Suppress output (exit code only, for CI)
  --verbose           Enable debug output (set -x)
  -h, --help          Show this help

Environment variables (equivalent, for piped installs):
  BHATTI_TIER, BHATTI_TIERS, BHATTI_MODE, BHATTI_FORCE=1

Examples:
  curl -fsSL bhatti.sh/install | bash                             # CLI (auto-detected)
  curl -fsSL bhatti.sh/install | sudo bash                        # server (prompted)
  curl -fsSL bhatti.sh/install | sudo bash -s -- --tiers all      # flags via pipe
  sudo ./scripts/install.sh --tier computer                       # server, computer tier
  sudo ./scripts/install.sh --tiers all                           # update + pull all tiers
EOF
}
```

### Fix 5: Detect non-systemd daemon on update

Add a `pgrep -x bhatti` fallback in `do_server_update` when systemd
isn't managing the service. If a manual `bhatti serve` is running,
print its PID and tell the user to restart it. Don't auto-kill — the
user would lose their terminal session.

### Fix 6: Post-install tier hint

After a fresh server install, list the other available tiers and show
`sudo bhatti update --tiers all`. After a server update, mention any
new tiers that exist upstream but aren't installed locally.

### Fix 7: Update error messages and docs

**Install script error messages** — `do_server_update` die message
currently only suggests `curl -fsSL bhatti.sh/install | sudo bash`.
After Fix 1, users will also reach this via `bhatti update`. Update to
suggest both:

```bash
die "server update requires root" \
    "Re-run with:" \
    "  sudo bhatti update" \
    "  curl -fsSL bhatti.sh/install | sudo bash"
```

Same for the root check in `do_server_install` (Fix 3).

**README.md** — The update story is a single sentence ("Re-running
updates an existing installation"). Expand to mention `bhatti update`
as the primary update path:

```markdown
## Updating

```bash
bhatti update                   # CLI: updates the binary
sudo bhatti update              # Server: updates all components
sudo bhatti update --tiers all  # Server: also pull additional tiers
```

Or re-run the install command directly:

```bash
curl -fsSL bhatti.sh/install | bash         # CLI
curl -fsSL bhatti.sh/install | sudo bash    # server
```
```

**docs/quickstart.md** — Same treatment. Add a section after the
install steps explaining how to update.

**Fallback install URL** — `bhatti.sh/install` is a redirect, and a
single point of failure. Document the raw GitHub URL as a fallback in
the README:

```bash
# If bhatti.sh is unreachable:
curl -fsSL https://raw.githubusercontent.com/sahil-shubham/bhatti/main/scripts/install.sh | bash
```

### Fix 8: Staged downloads with rollback

Address Problems 9 and 10. Download everything to a staging area first,
verify, then swap. If anything fails before the swap, the running
system is untouched.

**Binary staging on same filesystem (atomic rename):**

```bash
install_bhatti_binary() {
    local binary="bhatti-${OS}-${ARCH}"
    local dest="/usr/local/bin/bhatti"
    local tmp="${dest}.tmp.$$"

    # Stage to same filesystem as destination — mv is atomic rename
    # (cross-filesystem mv falls back to copy+delete, not atomic)
    download "${RELEASE_URL}/${binary}" "$tmp"
    chmod +x "$tmp"

    # Verify the binary actually executes (catches HTML error pages,
    # wrong-arch binaries, truncated downloads)
    if ! "$tmp" version >/dev/null 2>&1; then
        rm -f "$tmp"
        die "downloaded binary failed to execute" \
            "This usually means the download was corrupted or" \
            "the wrong platform binary was downloaded." \
            "Expected: ${OS}/${ARCH}"
    fi

    # macOS: remove quarantine attribute (Problem 11)
    if [ "$OS" = "darwin" ]; then
        xattr -d com.apple.quarantine "$tmp" 2>/dev/null || true
    fi

    # Backup previous binary for manual rollback
    if [ -f "$dest" ]; then
        cp "$dest" "${dest}.old" 2>/dev/null || true
    fi

    mv "$tmp" "$dest"
}
```

**Cleanup on failure:**

Extend the existing `_cleanup` trap to remove staged files:

```bash
_cleanup() {
    rm -f /tmp/bhatti.tmp
    rm -f /usr/local/bin/bhatti.tmp.$$
    rm -f "$DATA_DIR/lohar.tmp.$$" 2>/dev/null || true
}
```

If the script dies mid-update, `.tmp.*` files are cleaned up and the
previous binary is intact. The `.old` backup provides a manual
rollback path:

```bash
# If the new version is broken:
sudo mv /usr/local/bin/bhatti.old /usr/local/bin/bhatti
sudo systemctl restart bhatti
```

Mention this in the update completion output:

```
  Previous version saved to /usr/local/bin/bhatti.old
  Rollback: sudo mv /usr/local/bin/bhatti.old /usr/local/bin/bhatti
```

### Fix 9: `detect_tier` glob fallback

The glob fallback in `detect_tier` returns the first match
alphabetically, not the configured tier. If both `browser` and
`minimal` rootfs files exist, it returns `browser`. Fix to prefer
`minimal` as the safest default.

### Fix 10: `download()` HTTP code edge case

`download()` uses `curl -fsSL -w '%{http_code}'`, but `-f` (fail fast)
causes curl to exit non-zero on HTTP errors *before* writing the `-w`
output, so `$http_code` may be empty in the error path. Drop `-f` and
check the HTTP code manually.

---

## Implementation Order

Tests first, then the most impactful fix, then build outward.

### Phase 1: Script tests (bats-core)

Safety net before changing anything. Validates existing behavior so
we catch regressions as we refactor.

1. Add `BHATTI_TEST` guard to `install.sh` so functions can be sourced
2. Add `scripts/install_test.bats` with tier consistency, `version_gt`,
   `detect_tier`, and flag parsing tests
3. Add bats-core to CI alongside Go tests

### Phase 2: `bhatti update` server-awareness

The most impactful fix — prevents silent partial updates on servers.

1. Remove `BHATTI_MODE=cli` hardcoding in `updateCmd`
2. Add `--cli-only` and `--tiers` flags to `bhatti update`, forward as
   env vars to the install script
3. Update `bhatti update` Short, Long, and Example text
4. Update error messages in install script to suggest `sudo bhatti update`

### Phase 3: Install script hardening

Binary staging, verification, atomic replacement, rollback. Fixes 8–10.

1. Rewrite `install_bhatti_binary` with same-filesystem staging,
   binary verification, `.old` backup, and macOS quarantine stripping
2. Fix `detect_tier` glob fallback to prefer `minimal`
3. Fix `download()` to not rely on `-f` for HTTP error detection
4. Add config preservation invariant comment to `do_server_update`
   (it correctly never overwrites `/etc/bhatti/config.yaml` — document
   this as a rule so future changes don't break it)

### Phase 4: Install script flag parsing + root check

Two changes to the install script, low risk.

1. Move root check before tier prompt
2. Add flag parsing block before `main()` (`--quiet`, `--verbose`)
3. Add `usage()` function with `--help`

### Phase 5: Daemon detection + post-install hints

Cosmetic/safety improvements, low risk.

1. Add `pgrep -x` detection for non-systemd `bhatti serve` in
   `do_server_update`
2. Add tier hint after fresh server install
3. Update README.md and docs/quickstart.md with update instructions
4. Document fallback install URL

### Phase 6: Release check in `bhatti version`

Least urgent — lazy CLI-side GitHub check, no server changes.

1. Add GitHub releases API check to `bhatti version` (2s timeout)
2. Add `~/.bhatti/.latest-version` cache (1h TTL)
3. Display update notices for CLI and server
4. Surface in `bhatti admin status`

---

## What's Not in This Plan

**Version pinning** (`bhatti update --version v1.6.3`). Not needed yet —
single release stream, backward-compatible within major versions.

**Auto-updating / background update daemon.** `bhatti update` is explicit.
No surprises.

**Auto-restarting non-systemd daemon.** Detecting and warning is safe.
Auto-killing a foreground process loses the user's terminal context.

**Channels (stable/beta/nightly).** One release stream. Defer until
there's a reason to split.

**Passive update nagging on regular commands.** Update notices only appear
on `bhatti version` and `bhatti admin status`. Regular commands (`exec`,
`list`, `shell`) never show update notices. The only exception is the
existing `X-Bhatti-Min-CLI` hard-warning for critically outdated CLIs.

**Concurrent install locking.** Unlikely for a single-operator tool.

**GitHub API rate limit mitigation.** Install script runs infrequently
per host; `bhatti version` cache is 1h. Add `GITHUB_TOKEN` passthrough
if CI pipelines start hitting the 60 req/hr limit.

**Proxy / corporate firewall docs.** `curl` respects `HTTPS_PROXY`
natively. Document when someone asks.

---

## Install Script Quality-of-Life

These are UX gaps compared to best-in-class CLI installers (rustup,
tailscale, homebrew, deno). Each is small but they compound.
Independent of the phased fixes above — can ship in any order.

### 5.1 Install systemd service directly

The install script writes the unit file to `/var/lib/bhatti/bhatti.service`
and tells the user to `cp` it to `/etc/systemd/system/`. This is an
unnecessary manual step — the script already runs as root.

Write directly to `/etc/systemd/system/bhatti.service` in both
`do_server_install` and `do_server_update`. Then offer to start:

```bash
cp "$DATA_DIR/bhatti.service" /etc/systemd/system/bhatti.service
systemctl daemon-reload

# Only prompt in interactive mode — skip in CI/piped installs
if command -v systemctl >/dev/null 2>&1 && [ -t 0 ] && [ "${BHATTI_NO_PROMPT:-}" != "1" ]; then
    echo ""
    printf "  Start bhatti now? [Y/n]: "
    read -r start_choice < /dev/tty 2>/dev/null || start_choice="y"
    case "${start_choice:-y}" in
        n|N|no|NO) echo "  Skipped. Start later with: sudo systemctl enable --now bhatti" ;;
        *) systemctl enable --now bhatti; success "bhatti service started" ;;
    esac
else
    info "Start with: sudo systemctl enable --now bhatti"
fi
```

This eliminates the most common point where new users get stuck.
Non-interactive installs (CI, piped) skip the prompt and print the
manual start command.

### 5.2 Download progress for large files

The rootfs downloads are 200MB–1.5GB. `curl -fsSL` is fully silent — the
user sees nothing during a multi-minute download.

For direct downloads (binaries, kernel), drop the `-s` flag and use
curl's built-in progress bar:

```bash
# Small files (binaries < 50MB) — silent
download() {
    curl -fsSL -o "$dest" "$url"
}

# Large files (rootfs, kernel) — progress bar
download_large() {
    curl -fSL --progress-bar -o "$dest" "$url"
}
```

Note: `download_pipe` (used for rootfs) pipes curl into `zstd`, so
curl's progress bar won't work there — stdout is consumed by the
decompression pipeline. For piped downloads, print the expected size
before starting and let the step timing indicate progress.

### 5.3 Checksum verification

The release workflow generates `checksums-sha256.txt` but the install
script doesn't verify against it. Every downloaded binary and rootfs
should be verified.

Download the checksums file once per install/update run and reuse it:

```bash
# At the top of do_server_install / do_server_update:
CHECKSUMS=$(curl -fsSL "${RELEASE_URL}/checksums-sha256.txt" 2>/dev/null || true)

verify_checksum() {
    local file="$1" expected_name="$2"
    [ -n "$CHECKSUMS" ] || { info "checksums not available, skipping verification"; return 0; }
    local expected
    expected=$(echo "$CHECKSUMS" | grep "$expected_name" | awk '{print $1}')
    [ -n "$expected" ] || { info "checksum not found for $expected_name, skipping"; return 0; }
    local actual
    actual=$(sha256sum "$file" | awk '{print $1}')
    [ "$actual" = "$expected" ] || die "checksum mismatch for $expected_name" \
        "expected: $expected" \
        "got:      $actual" \
        "The download may be corrupt. Try again."
}
```

Fail hard on mismatch. Skip gracefully if checksums file is unavailable
(older releases, air-gapped). This matters especially for the server
install where we're placing binaries at `/usr/local/bin/` as root.

### 5.4 "All tiers" interactive option

Add "5) all" to the interactive tier prompt. Show download sizes next
to each option. On update, mention any tiers that exist upstream but
aren't installed locally.

### 5.5 Uninstall discoverability

Add uninstall instructions to the install completion output. Also fix
the stale `sudo $0 --systemd` reference in `scripts/uninstall.sh`.

### 5.6 Disk space pre-check

Before downloading a 1.5GB rootfs, check if there's enough disk space.
Discovering you're out of space 10 minutes into a download is one of
the worst install experiences.

```bash
check_disk_space() {
    local required_mb="$1" path="$2"
    local available_mb
    available_mb=$(df -BM "$path" 2>/dev/null | tail -1 | awk '{print $4}' | tr -d 'M')
    if [ -n "$available_mb" ] && [ "$available_mb" -lt "$required_mb" ]; then
        die "insufficient disk space" \
            "Required: ${required_mb}MB" \
            "Available: ${available_mb}MB on $(df "$path" | tail -1 | awk '{print $6}')" \
            "Free up space and try again."
    fi
}
```

Call before `install_rootfs`. Tier sizes are already known from the
prompt text (`~200MB`, `~600MB`, etc.) — use those plus a 20% margin.
Also check before `install_kernel` and `install_lohar`.

### 5.7 Elapsed time per step and total

The single biggest "this feels professional" signal in a CLI installer.
rustup, Homebrew, and Tailscale all do this.

```
==> Installing bhatti v1.6.5 (server, browser tier) on myhost (aarch64)
  ✓ Firecracker 1.14.0 + jailer (up to date)
  ✓ bhatti v1.6.5 (2.1s)
  ✓ lohar (4.1M, 0.8s)
  ✓ kernel (8.2M, 1.2s)
  ✓ rootfs browser (612M, 48.3s)
  ✓ systemd service installed

  Done in 52.4s
```

Implementation is trivial — capture `$SECONDS` at the start of each
step:

```bash
step_start() { _step_start=$SECONDS; }
step_elapsed() { echo "$(( SECONDS - _step_start ))s"; }
```

Capture total at the top of `main()` and print in the completion block.

### 5.8 Shell completions in install output

Detect `$SHELL` and suggest the completion command in install output.
`bhatti setup` already does this — mirror it in the install script.

### Implementation order

5.1 (systemd direct install) and 5.2 (download progress) are the
highest impact. 5.3 (checksums) and 5.7 (disk space) are reliability.
5.8 (elapsed time) is cheap polish. The rest is cosmetic.

All independent of Phases 1–6 and can ship in any order.

---

## Script Tests (bats-core)

The install script has zero tests. Every bug fixed in the computer tier
session (missing tier in menu, missing from CI matrix, missing from
server registration) would have been caught by a single consistency
test. Add tests using [bats-core](https://github.com/bats-core/bats-core)
— the standard framework for shell script testing (used by Homebrew,
nvm, rbenv). Fast, no network, no root.

Only test things that catch real bugs. No tests for trivial one-liners
or things already validated by CI (rootfs builds, downloads).

**This ships as Phase 1 in the implementation order** — tests first,
then changes. The tests validate existing behavior so we have a
regression safety net before refactoring anything.

### 7.1 Tier consistency

The class of bug from the computer tier incident: a tier exists as a
script in `scripts/tiers/` but isn't registered in one or more of the
other places that need to know about it.

One test that verifies every `scripts/tiers/*.sh` file appears in:
- `scripts/build-tier.sh` case statement (SIZE_MB default)
- `.github/workflows/release.yml` matrix
- `scripts/install.sh` interactive menu
- `scripts/install.sh` `ALL_KNOWN_TIERS` in `do_server_update`

This is a CI gate that prevents the exact class of bug that caused
this entire plan to exist.

### 7.2 `version_gt`

Gates major version upgrade prompts and "already up to date" detection.
If wrong, users either get blocked from updating or silently skip
safety prompts. Edge cases matter:

- `v1.6.3 > v1.6.2` → true
- `v2.0.0 > v1.99.99` → true
- `v1.6.3 > v1.6.3` → false (equal)
- `1.6.3 > 1.6.2` → true (no v-prefix)
- `v1.0 > v0.9` → true (missing patch)

### 7.3 `detect_tier`

Extracts the tier name from `config.yaml`'s `firecracker_rootfs` path.
If it parses wrong, `do_server_update` downloads the wrong rootfs.
Test against real config formats:

- `firecracker_rootfs: /var/lib/bhatti/images/rootfs-browser-arm64.ext4` → `browser`
- `firecracker_rootfs: /var/lib/bhatti/images/rootfs-computer-amd64.ext4` → `computer`
- Quoted paths (`"..."`, `'...'`) → still parses correctly
- Missing config → falls back to glob, then to `minimal`

Uses temp directories with mock config files — no real `/etc/bhatti`
needed.

### 7.4 Flag parsing

After Fix 4, flags drive the entire install flow. Test that:

- `--tier browser` sets `BHATTI_TIER=browser`
- `--tiers all` sets `BHATTI_TIERS=all`
- `--tiers computer,browser` sets `BHATTI_TIERS=computer,browser`
- `--force` sets `BHATTI_FORCE=1`
- `--help` prints usage and exits 0
- Unknown flag `--bogus` exits non-zero with error
- Flags override env vars (`BHATTI_TIER=minimal --tier browser` → browser wins)
- `--tier=browser` (equals syntax) works same as `--tier browser`

### Implementation

Add `bats-core` as a git submodule or download in CI. Test file at
`scripts/install_test.bats`. The install script needs a `--test` flag
(or similar) that sources functions without running `main()`:

```bash
# At the bottom of install.sh, replace bare `main` call:
if [ "${BHATTI_TEST:-}" != "1" ]; then
    main
fi
```

Then tests source the script and call functions directly:

```bash
setup() {
    export BHATTI_TEST=1
    source scripts/install.sh
}

@test "version_gt: v1.6.3 > v1.6.2" {
    run version_gt v1.6.3 v1.6.2
    [ "$status" -eq 0 ]
}

@test "all tiers in scripts/tiers/ are in install menu" {
    local disk=$(ls scripts/tiers/*.sh | xargs -I{} basename {} .sh | sort)
    local menu=$(grep -oP 'tier="\K[a-z]+' scripts/install.sh | sort -u)
    [ "$disk" = "$menu" ]
}
```

Run in CI alongside Go tests. No root, no network, sub-second.
