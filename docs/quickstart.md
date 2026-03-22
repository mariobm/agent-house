# Quickstart

Get Bhatti running on a Linux host with KVM in under 10 minutes.

## Install

```bash
# On a Linux host with KVM (Raspberry Pi 5, AWS Graviton, Hetzner bare metal)
git clone https://github.com/sahil-shubham/bhatti.git
cd bhatti
sudo ./scripts/install.sh
```

The install script builds everything from source — Go, Firecracker, the host daemon, the guest agent, and a base Ubuntu rootfs:

```
==> Installing bhatti on myhost (aarch64)
==> Installing Go 1.25.6...
==> Installing Firecracker 1.6.0...
==> Building bhatti and lohar from source...
==> Downloading kernel...
==> Building rootfs (this takes ~10 minutes on first install)...
==> Generating config...

============================================
  bhatti installed on myhost (aarch64)

  To start the daemon:
    cd /var/lib/bhatti && sudo bhatti serve

  Then in another terminal:
    bhatti create --name hello
    bhatti exec hello -- echo 'it works'
    bhatti shell hello
    bhatti destroy hello
============================================
```

For a systemd service that starts on boot:

```bash
sudo ./scripts/install.sh --systemd
```

## Create a Sandbox

```bash
bhatti create --name dev --cpus 2 --memory 1024
# → a1b2c3d4e5f6  dev  192.168.137.2
```

This boots a Firecracker microVM with 2 vCPUs, 1GB RAM, and a full Ubuntu 24.04 userland. The VM is ready when the command returns (~3.5 seconds).

## Run Commands

```bash
bhatti exec dev -- uname -a
# → Linux dev 6.1.90 ... aarch64 GNU/Linux

bhatti exec dev -- node --version
# → v22.16.0

bhatti exec dev -- which rg fd
# → /usr/bin/rg
# → /usr/bin/fd
```

## Interactive Shell

```bash
bhatti shell dev
```

You're now inside the VM. Full zsh with syntax highlighting, autosuggestions, and starship prompt. Press `Ctrl+\` to detach — the shell keeps running inside the VM.

## File Operations

```bash
# Write a file into the VM
echo 'console.log("hello from bhatti")' | bhatti file write dev /workspace/app.js

# Read it back
bhatti file read dev /workspace/app.js

# List a directory
bhatti file ls dev /workspace/
```

## Secrets

```bash
bhatti secret set API_KEY sk-abc123
bhatti secret list
bhatti secret delete API_KEY
```

## Destroy

```bash
bhatti destroy dev
```

VM is gone. TAP device cleaned up, IP returned to the pool, rootfs deleted.

## What Just Happened

Behind the scenes:

1. `create` copied the base rootfs (CoW clone if filesystem supports it), created a config drive with hostname/DNS/auth token, allocated a TAP device and IP on the bridge network, started a Firecracker process, configured it over the Unix socket API, booted the kernel, and waited for lohar (the guest agent) to respond on TCP port 1024.

2. `exec` connected to lohar over TCP, sent an `EXEC_REQ` frame with the command, streamed `STDOUT`/`STDERR` frames back, and read the `EXIT` frame with the exit code.

3. `shell` opened a WebSocket to the daemon, which connected to lohar and allocated a PTY inside the VM. Terminal I/O was relayed bidirectionally via `STDIN`/`STDOUT` frames.

4. `destroy` killed the Firecracker process, removed the TAP device, released the IP, and deleted the sandbox directory.

If you'd left the sandbox idle for 30 seconds, it would have automatically transitioned to *warm* (vCPUs paused, ~400µs resume). After 30 minutes idle, it would have been snapshotted to disk and the Firecracker process killed (*cold*, ~50ms resume). The next `exec` would have transparently restored it. See [Thermal Management](thermal-management.md) for details.

## Environment Variables

```bash
export BHATTI_URL=http://localhost:8080   # API endpoint (default)
export BHATTI_TOKEN=your-auth-token       # Auth token (or from ~/.bhatti/config.yaml)
```

## Next Steps

- [Architecture](architecture.md) — how the system fits together
- [API Reference](api-reference.md) — REST and WebSocket endpoints
- [CLI Reference](cli-reference.md) — all commands
- [Design Decisions](decisions.md) — why things are the way they are
