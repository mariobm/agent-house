# ⚒ Bhatti

Open-source Firecracker microVM orchestrator. Each sandbox is a real Linux VM with its own kernel, filesystem, and process isolation — created in seconds, paused for free, resumed in microseconds.

Built for running AI coding agents in isolated environments. A paused sandbox resumes and executes a command in **under 3ms**.

```
bhatti create --name dev --cpus 2 --memory 1024
bhatti exec dev -- npm install
bhatti shell dev                          # Ctrl+\ to detach
bhatti destroy dev
```

## Install

```bash
curl -fsSL bhatti.sh/install | bash
```

On macOS, installs the CLI (~11MB binary). On Linux, asks whether you want the CLI or a full self-hosted server.

Self-hosting? Same command with `sudo` — it downloads pre-built binaries, a kernel, and an Ubuntu 24.04 rootfs:

```bash
curl -fsSL bhatti.sh/install | sudo bash
```

<details>
<summary>Fallback if bhatti.sh is unreachable</summary>

```bash
curl -fsSL https://raw.githubusercontent.com/sahil-shubham/bhatti/main/scripts/install.sh | bash
```
</details>

See [Quickstart](docs/quickstart.md) for full setup details.

## Updating

```bash
bhatti update                   # CLI: updates the binary
sudo bhatti update              # Server: updates all components
sudo bhatti update --tiers all  # Server: also pull additional tiers
```

> **Note:** `bhatti update` updating all server components requires v1.7.3+. On older versions, re-run the install command: `curl -fsSL bhatti.sh/install | sudo bash`

## Rootfs Tiers

The server install prompts you to pick a rootfs tier. Each tier is a pre-built Ubuntu 24.04 image:

| Tier | What's in it | Size |
|------|-------------|------|
| `minimal` | Bare Ubuntu + curl + fuse3 | ~200MB |
| `browser` | + Chromium, Playwright, Node 22 | ~600MB |
| `docker` | + Docker Engine | ~550MB |
| `computer` | + Full desktop: XFCE, KasmVNC, Chromium | ~1.5GB |

Use `--image` to create sandboxes from non-default tiers:

```bash
# Run browser automation
bhatti create --name scraper --image browser
bhatti exec scraper -- npx playwright test

# Run a desktop environment (VNC on port 6901)
bhatti create --name desktop --image computer
bhatti publish desktop -p 6901

# Run Docker-in-VM
bhatti create --name ci --image docker
bhatti exec ci -- docker run hello-world
```

The server auto-discovers tiers from `/var/lib/bhatti/images/`. Install more with `sudo bhatti update --tiers all`. See [Tiers](docs/tiers.md) for details on adding custom tiers.

## CLI Commands

### Core

| Command | Description |
|---------|-------------|
| `create` | Create a new sandbox VM |
| `list` | List sandboxes |
| `inspect` | Show sandbox details (state, IP, resources) |
| `exec` | Execute a command in a sandbox |
| `shell` | Open an interactive shell (Ctrl+\\ to detach) |
| `ps` | List active sessions in a sandbox |
| `stop` | Snapshot and stop a sandbox |
| `start` | Resume a stopped sandbox |
| `destroy` | Destroy a sandbox |

### Files & Data

| Command | Description |
|---------|-------------|
| `file read` | Read a file from a sandbox |
| `file write` | Write stdin to a file in a sandbox |
| `file ls` | List files in a sandbox directory |
| `volume create` | Create a persistent volume |
| `volume list` | List volumes |
| `volume delete` | Delete a volume |
| `secret set` | Create or update an encrypted secret |
| `secret list` | List secrets |

### Images & Snapshots

| Command | Description |
|---------|-------------|
| `image list` | List available rootfs images |
| `image pull` | Pull an OCI/Docker image from a public registry |
| `image import` | Import a local Docker image as a bhatti rootfs |
| `image save` | Save a sandbox's rootfs as a reusable image |
| `snapshot create` | Checkpoint a running sandbox |
| `snapshot resume` | Resume from a named snapshot |

### Networking

| Command | Description |
|---------|-------------|
| `publish` | Publish a sandbox port with a public URL |
| `unpublish` | Remove a published port |
| `share` | Generate a shareable web shell URL |

### Admin (server operators)

| Command | Description |
|---------|-------------|
| `serve` | Start the bhatti daemon |
| `user create` | Create a user with API key and resource limits |
| `user list` | List users |
| `user rotate-key` | Rotate a user's API key |
| `admin status` | System overview (sandboxes, memory, disk) |
| `admin events` | Query the event log |
| `admin metrics` | Query metrics snapshots |

### Setup

| Command | Description |
|---------|-------------|
| `setup` | Interactive CLI configuration (endpoint + API key) |
| `update` | Update bhatti to the latest version |
| `version` | Print version and check for updates |
| `completion` | Generate shell completions (bash/zsh/fish) |

All commands support `--json` for machine-readable output. See [CLI Reference](docs/cli-reference.md) for full flag details.

## Performance

On a Raspberry Pi 5 (ARM64, NVMe):

```
                                p50       p95       p99
Exec command:                   1.26ms    1.88ms    2.93ms
1KB file read:                  433µs     733µs     1.08ms
1KB file write:                 754µs     1.17ms    1.82ms
Warm resume + exec:             2.70ms    7.36ms    7.36ms
Cold resume + exec:             40.8ms    42.3ms    42.3ms
10 concurrent execs:            12.8ms    22.3ms    22.3ms

VM boot (create + first exec):  8.0s      8.4s      8.4s
Full snapshot (512MB):           3.3s      3.6s      3.6s
Diff snapshot:                  20.7ms    35.9ms    35.9ms
Pause (hot→warm):               450µs     611µs     611µs
Resume (warm→hot):               462µs     565µs     565µs
```

## Architecture

```
bhatti (host daemon)                        lohar (guest agent, PID 1 in each VM)
  ├─ REST/WS API (:8080)                     ├─ TCP :1024 (exec, files, sessions)
  ├─ Per-user auth (API keys, SHA-256)        ├─ TCP :1025 (port forwarding)
  ├─ Firecracker engine                       ├─ PTY sessions + 64KB scrollback
  │  (create, exec, snapshot, diff snap)      ├─ Atomic file writes
  ├─ Thermal manager                          ├─ Process group kill
  │  (hot → warm → cold, auto)               ├─ Exec as uid 1000 (not root)
  ├─ Per-user bridge networks (isolated)      └─ Config drive (env, secrets)
  ├─ SQLite store + age encryption
  ├─ Rate limiting + exec timeouts
  └─ Reverse proxy (HTTP + WebSocket)
```

Idle sandbox → **warm** after 30s (vCPUs paused, ~400µs resume) → **cold** after 30min (snapshotted to disk, memory freed, ~50ms resume). Any API request transparently wakes it.

## Multi-Tenant Isolation

Each user gets their own API key, sandbox limits, and network:

```bash
sudo bhatti user create --name alice --max-sandboxes 5
# → API key: bht_...  (shown once)
```

- **API scoping** — users see only their own sandboxes and secrets
- **Network isolation** — per-user bridge + /24 subnet, cross-user traffic blocked at L2
- **Resource caps** — per-user limits on sandbox count, CPUs, and memory
- **Rate limiting** — per-user token buckets (10 creates/min, 120 execs/min)
- **Secrets** — encrypted at rest (age), scoped per user

## Key Features

- **Preview URLs** — `bhatti publish dev -p 3000` → `https://dev-k3m9x2.bhatti.sh`, auto-wake from sleep
- **Diff snapshots** — only dirty pages after the first snapshot (~52ms vs ~4.4s)
- **Session-aware exec** — TTY sessions survive disconnects, scrollback replayed on reattach
- **OCI image support** — `bhatti image pull python:3.12` → use as base for sandboxes
- **Persistent volumes** — survive sandbox destruction, mountable across sandboxes
- **Streaming exec** — real-time NDJSON output via `Accept: application/x-ndjson`
- **Guest hardening** — exec as uid 1000, config drive unmounted after boot, connection/session limits
- **Single binary** — `bhatti serve` = daemon, `bhatti create` = CLI, `bhatti user` = admin

## Documentation

| | |
|---|---|
| **[Quickstart](docs/quickstart.md)** | CLI install + server install, user management |
| **[Architecture](docs/architecture.md)** | System design, data flow, concurrency model |
| **[Tiers](docs/tiers.md)** | Rootfs tiers, adding custom tiers |
| **[Wire Protocol](docs/wire-protocol.md)** | Binary framing, connection lifecycle, auth |
| **[Guest Agent](docs/guest-agent.md)** | PID 1 init, PTY, sessions, process management |
| **[Thermal Management](docs/thermal-management.md)** | Hot/warm/cold, diff snapshots, activity caching |
| **[Networking](docs/networking.md)** | Per-user bridges, iptables isolation, kernel ip= |
| **[API Reference](docs/api-reference.md)** | REST/WebSocket endpoints |
| **[CLI Reference](docs/cli-reference.md)** | All commands and flags |
| **[Testing](docs/testing.md)** | 11K lines of tests, zero mocks for VM tests |
| **[Design Decisions](docs/decisions.md)** | Why TCP over vsock, why no FC SDK, why PID 1, ... |

## Requirements

**Server:** Linux (aarch64 or x86_64) with KVM (`/dev/kvm`) and root access.

**CLI:** macOS or Linux. No special requirements.

## License

[Apache 2.0](LICENSE).
