# Slicer Architecture Learnings

Research notes from hands-on testing of [SlicerVM](https://docs.slicervm.com/)
(v0.1.108) on Raspberry Pi 5. Slicer is a production Firecracker orchestrator
by OpenFaaS Ltd. These notes inform bhatti's architecture decisions.

Tested: 2026-03-15

---

## How Slicer's guest agent works

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

## Pause/Resume vs Snapshot/Restore

- **Pause/Resume** (vCPU only): works reliably. FC process stays alive,
  memory stays allocated, vsock UDS stays intact. Exec works immediately
  after resume.
- **Suspend/Restore** (snapshot to disk): CLI commands exist (`slicer vm
  suspend`, `slicer vm restore`) but return 404 on v0.1.108. Not yet
  shipped. Likely hitting the same vsock-after-snapshot issue we found.

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

Slicer demonstrates several standard techniques well. These are not Slicer
inventions — they are documented Linux, Firecracker, and cloud-init patterns.
Listed here as a reminder of which to adopt for bhatti, with their actual origins.

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
