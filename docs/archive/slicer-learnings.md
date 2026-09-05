# SlicerVM — research notes on a peer Firecracker orchestrator

Research notes from hands-on testing of [SlicerVM](https://docs.slicervm.com/)
(v0.1.108) on Raspberry Pi 5. SlicerVM is a production Firecracker orchestrator
by OpenFaaS Ltd, evaluated alongside [Sprites](./sprites-learnings.md) and
other projects in the Firecracker-orchestrator space.

Tested: 2026-03-15

## What this file is and isn't

This is a record of *what SlicerVM does as a product* — not a list of techniques
sourced from it. SlicerVM is closed-source; everything noted below was observed
from its public documentation, its CLI, and runtime behavior of the binaries
shipped in its OCI images.

Every technique referenced in this file has a documented upstream origin in
Linux, Firecracker, or cloud-init. The "[Standard Linux/Firecracker techniques
worth using](#standard-linuxfirecracker-techniques-worth-using)" section at the
bottom enumerates each one with its actual source. Bhatti's design follows those
upstream sources directly; SlicerVM appears in this file because it's a useful
concrete example to compare against, and because its public release notes
(e.g. the v0.1.108 unship of suspend/restore) corroborate upstream Firecracker
behaviors we hit independently.

---

## What SlicerVM's guest agent exposes (observed)

- **`slicer-agent`** (6.7MB Go binary) runs as a systemd service inside the VM
- Listens on **vsock port 514** (shell) and **vsock port 516** (RPC/exec)
- Separate **`vmmeter`** binary (6.4MB) reports metrics on vsock port 515
- Agent handles: exec, file copy, shell, secret sync, shutdown
- Auth: `/runner/agent_token` on a second drive for host↔agent authentication

## Networking

- **Kernel `ip=` cmdline** configures guest eth0 before init runs:
  `ip=192.168.137.2::192.168.137.1:255.255.255.0::eth0:off:8.8.8.8:1.1.1.1:`
- No chicken-and-egg: network is up before systemd/agent starts
- **Bridge networking**: single Linux bridge (`brvm0`) shared by all VMs in a
  host group. Simpler than per-VM TAP + iptables NAT. VMs can talk to each
  other and are directly routable from the host.
- DNS configured via `/runner/configure-dns.sh` (reads from config drive)

## Config injection via second drive

- 1MB ext4 image mounted as `/dev/vdb` → `/runner/`
- Contains: `job.conf` (env vars), `configure-dns.sh`, `agent_token`, `ssh_keys`, `userdata.sh`
- Available before systemd starts → init scripts can read it
- Much cleaner than exec-after-boot for config injection

## Image distribution

- OCI images via containerd: `ghcr.io/openfaasltd/slicer-systemd-arm64:6.1.90-aarch64-latest`
- Kernel baked into the image (extracted at launch)
- Rootfs created via overlayfs snapshotter from containerd
- Supports ZFS and devmapper for instant clones
- Image tags encode kernel version + arch

## Pause/Resume vs Snapshot/Restore in SlicerVM

What ships in v0.1.108:

- **Pause/Resume** (vCPU only): works reliably. FC process stays alive,
  memory stays allocated, vsock UDS stays intact. Exec works immediately
  after resume.
- **Suspend/Restore** (snapshot to disk): CLI commands exist (`slicer vm
  suspend`, `slicer vm restore`) but return 404 on v0.1.108. Not yet
  shipped. Public corroboration of the upstream Firecracker vsock-after-restore
  issue documented in the next section.

## Key finding: vsock is broken after Firecracker snapshot/restore

Confirmed independently:
- Slicer's 6.1.90 kernel exhibits the same behavior as the 5.10 pre-built kernel
- After snapshot → kill FC → new FC → load snapshot: vsock connections from
  host complete the CONNECT handshake (Firecracker side) but the guest agent
  never receives them. Ping over virtio-net works perfectly.
- This is a Firecracker/virtio_vsock interaction bug, not a kernel version issue
- Firecracker PR #5688 ("minimize local port collisions after snapshot restore")
  is in v1.15.0 but doesn't fully solve it

## Shell and port forwarding UX

- Shell: `slicer vm shell vm-1` — direct vsock connection, bypasses SSH.
  Supports `--uid`, `--gid`, `--cwd`, `--shell`, `--bootstrap` (run command on connect)
- Port forward: SSH-style `-L` syntax including Unix socket forwarding:
  `slicer vm forward vm-1 -L 2375:/var/run/docker.sock`
- Exec: `slicer vm exec vm-1 -- cmd` with `--uid`, `--cwd`, `--shell` options

## Standard Linux/Firecracker techniques worth using

SlicerVM is a useful concrete example of several standard techniques. To be
explicit: **none of these are SlicerVM inventions.** They are documented Linux,
Firecracker, and cloud-init patterns, and bhatti adopts them from those upstream
sources directly. Listed here with their actual origins so the attribution is
unambiguous.

1. **Kernel `ip=` cmdline** for guest network config before init runs.
   Documented in linux `Documentation/admin-guide/kernel-parameters.txt`.
   Solves the chicken-and-egg of "agent needs network, network needs agent."

2. **TCP-over-virtio-net for post-snapshot communication.** vsock breaks after
   Firecracker snapshot/restore (upstream Firecracker behavior, see FC issue
   tracker). virtio-net survives, so use TCP listeners on the guest as the
   post-restore channel.

3. **Config drive (second virtio-blk for per-VM config).** Standard cloud-init
   pattern (NoCloud datasource). Available before init scripts run.

4. **Bridge networking over per-VM NAT.** Standard Linux bridge networking,
   simpler than per-VM TAP+iptables for multi-sandbox setups.

5. **Agent auth token** to prevent unauthorized exec access. Standard
   bearer-token auth pattern.

6. **Shell UX flags** (`--uid`, `--cwd`, `--bootstrap`). Common Unix conventions
   from `su`, `sudo`, `chroot`, etc.
