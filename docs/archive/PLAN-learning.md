# Learning Plan: Understanding What You Built + Rewriting the Docs

Two goals, one process. You learn the systems concepts behind your code,
and you rewrite the documentation in your own voice as you go. Every doc
you rewrite is proof — to yourself and anyone reading — that you understand
the system. Machine-written docs explain correctly but generically. Your
docs will explain why things are the way they are, what surprised you, and
what you'd do differently.

**The method for every topic:**
1. Read your code for that area (specific files listed below)
2. Read the current doc section for it
3. Study the concept using the linked resource
4. Note where the doc is wrong, stale, or machine-sounding
5. Rewrite that doc section in your words

---

## Documentation Staleness Audit

Before starting, here's what's wrong with the docs RIGHT NOW. The code
has evolved significantly since these were written. Some are cosmetic,
some are outright incorrect.

### Critical (docs describe behavior the code no longer has)

| Doc | What it says | What the code actually does |
|-----|-------------|---------------------------|
| `thermal-management.md` §Diff Snapshots | Diff snapshots are used for warm→cold, `track_dirty_pages: true` | **Diff snapshots are completely disabled.** All snapshots are Full. `track_dirty_pages: false`. The rory incident killed this. (`engine.go:508-510`) |
| `thermal-management.md` §Why TCP | "Cold boot uses vsock (slightly faster)" | **All connections use TCP, including cold boot.** `Create()` calls `agent.NewTCPClientWithAuth()` directly (`engine.go:589`). Vsock is still configured for FC but the agent client never uses it. |
| `networking.md` entire doc | Describes a single shared bridge `brbhatti0` on `192.168.137.0/24` | **Per-user bridges** with `subnetFromIndex()`. Each user gets their own bridge and /24 subnet. `userNetworks map[string]*UserNetwork` (`engine.go:70,228-246`) |
| `architecture.md` §Project Structure | Lists `docker/docker.go` | **Docker engine is gone.** The `pkg/engine/docker/` directory doesn't exist. |
| `architecture.md` §Project Structure | Missing files | Missing: `jail.go`, `ringbuffer.go`, `public_proxy.go`, `pkg/backup/*`, `pkg/oci/*` |
| `architecture.md` diagram | Shows single bridge `brbhatti0` | Per-user bridges (same as networking.md) |

### Moderate (docs are incomplete, missing new features)

| Doc | What's missing |
|-----|---------------|
| `architecture.md` | No mention of jailer integration (chroot, UID drop, cgroups) |
| `architecture.md` | No mention of balloon device / memory reclaim on warm VMs |
| `architecture.md` | No mention of OCI image import (`pkg/oci/`) |
| `architecture.md` | No mention of S3 backup (`pkg/backup/`) |
| `architecture.md` | No mention of public proxy / preview URLs architecture |
| `thermal-management.md` | No mention of balloon inflation on warm VMs |
| `thermal-management.md` | No mention of force-pause after 10 consecutive activity failures |
| `thermal-management.md` | No mention of thermal failure counters or circuit breaker |
| `guest-agent.md` | Boot args still reference `console=ttyS0` — serial console is now disabled via `8250.nr_uarts=0` |
| `wire-protocol.md` | Opening paragraph mentions vsock for cold boot — now always TCP |
| `decisions.md` | No entry for: disabling diff snapshots, jailer, balloon, per-user bridges |
| `README.md` §Architecture | Diagram shows single bridge, no jailer, no balloon |

### Tone/Voice (machine artifacts)

These aren't wrong, but they read like an LLM wrote them:

- Every section follows the exact same structure (Context → Alternatives →
  Decision → Tradeoff). Real docs vary in structure based on what's important.
- Excessive use of "This is" and "Here's what" as openers.
- Every detail is explained to the same depth — important things and trivial
  things get equal weight.
- `decisions.md` explains things the reader already knows ("HTTP is a text
  protocol — headers are human-readable strings").
- Comments in code sometimes explain Go language features instead of domain
  decisions.

When you rewrite, focus on: What would confuse YOU if you came back in 6
months? What would a contributor need to know? What can be cut?

---

## Week 1-2: Processes and the Guest Agent

### What to learn

Everything lohar does is about processes. Fork, exec, signals, process
groups, PID 1 responsibilities — this is the foundation.

**Read your code:**
- `cmd/lohar/main.go` (310 lines) — the boot sequence, mounts, listeners
- `cmd/lohar/exec.go` — piped exec with process group kill
- `cmd/lohar/session.go` — session registry, ring buffer, idle timers
- `cmd/lohar/handler.go` (147 lines) — protocol dispatch

**Study:**
- OSTEP Chapter 5: "Process API" — fork, exec, wait, signals
  https://pages.cs.wisc.edu/~remzi/OSTEP/cpu-api.pdf (free, 15 pages)
- OSTEP Chapter 26: "Concurrency and Threads" — for understanding the
  goroutines in your exec fan-out
  https://pages.cs.wisc.edu/~remzi/OSTEP/threads-intro.pdf
- `man 7 signal` on your Linux server — the signal reference

**Concepts to nail down:**
- fork() creates a copy of the current process. execve() replaces that
  copy with a new program. Go's `exec.Command.Start()` does both.
- `Setpgid: true` puts the child in its own process group. `Kill(-pid)`
  kills the entire group. This is why `npm install` and all its children
  die when you cancel an exec.
- PID 1 never exits. Orphaned processes become PID 1's children. Your code
  intentionally skips zombie reaping because Go's runtime races with it.
- Signals: SIGKILL (instant death, can't be caught), SIGTERM (graceful,
  can be caught), SIGHUP (terminal disconnected), SIGCHLD (child exited).

**Questions to answer yourself:**
1. Why does `cmd/lohar/exec.go` use `Kill(-pid)` instead of `Kill(pid)`?
2. Why does `cmd/lohar/main.go` end with `select {}`?
3. What happens if a process inside your VM forks and the parent dies?
4. Why does piped exec use SIGKILL but TTY sessions use SIGTERM?

### Docs to rewrite: `guest-agent.md`

Current problems:
- Boot args example still has `console=ttyS0` — serial is disabled now
- Doesn't mention lohar injection on every create (`injectLoharIntoRootfs`)
- The PTY section is thorough but reads like a textbook, not docs. Focus
  on what a contributor needs to know, not explaining ioctls from scratch.
- Missing: what happens when the agent's auth token doesn't match
- Missing: connection limits, session limits

Rewrite goals:
- Update boot args to match actual `engine.go:496-498`
- Add a section on lohar injection and why it exists (protocol drift)
- Trim the PTY internals to "lohar opens /dev/ptmx, unlocks it, starts
  the shell on the slave side" — link to a resource for the deep dive
- Add a "what changed" section at the top noting diff snapshots disabled,
  serial console disabled, always-TCP

---

## Week 3-4: Networking

### What to learn

Your networking has evolved significantly — per-user bridges, iptables
isolation, the kernel ip= trick. The docs describe the old single-bridge
model.

**Read your code:**
- `pkg/engine/firecracker/network.go` (346 lines) — ALL the networking
- Pay attention to: `subnetFromIndex`, `UserNetwork`, `ensureUserBridge`,
  `setupGlobalFirewall`, `createTapDevice`, `destroyTapDevice`

**Study:**
- Julia Evans: networking zine / blog posts — approachable, visual
  https://jvns.ca/blog/2024/10/01/some-notes-on-nix-networking/
- Red Hat: virtual networking intro — bridges and TAP
  https://developers.redhat.com/blog/2018/10/22/introduction-to-linux-interfaces-for-virtual-networking
- Arch Wiki: iptables — packet flow diagram, NAT tables
  https://wiki.archlinux.org/title/iptables

**Concepts to nail down:**
- A TAP device is a virtual network cable. One end in the VM, one on host.
- A bridge connects TAPs like a switch. Your per-user bridges isolate
  users at L2 — VMs on different bridges can't even see each other.
- NAT/MASQUERADE rewrites source IPs so VMs can reach the internet.
  One masquerade rule per bridge, not per VM.
- Kernel `ip=` configures the guest NIC before init runs. Solves the
  chicken-and-egg problem (host needs to talk to agent, agent needs
  network, host needs to tell agent its IP).
- iptables FORWARD chain: packets being routed through the host (not
  TO the host). Your VMs' internet traffic goes through FORWARD.

**Questions to answer yourself:**
1. What is `subnetFromIndex` doing? How does user 1 get a different
   subnet than user 2?
2. Why does `setupGlobalFirewall` use `-I FORWARD 1` (insert at position 1)?
3. What would break if you destroyed the TAP device during `Stop()`
   instead of `Destroy()`?
4. How does per-user bridge isolation work at the packet level? Can VM A
   (user 1) send a packet to VM B (user 2)?

### Docs to rewrite: `networking.md`

This is the most stale doc. Almost everything needs updating.

Current problems:
- Entire doc describes single bridge `brbhatti0` on `192.168.137.0/24`
- Code now has per-user bridges with dynamic subnets
- No mention of `setupGlobalFirewall()` or what the 6 global rules are
- No mention of per-user bridge creation/cleanup lifecycle
- IP pool section describes 253 addresses — now it's 253 per user
- The "Bridge Architecture" ASCII diagram is wrong
- Post-snapshot networking section is good but should mention that
  ALL connections now use TCP (not just post-restore)

Rewrite goals:
- New diagram showing per-user bridges
- Explain `subnetFromIndex` — how subnets are allocated
- Document the 6 global firewall rules and why each exists
- Document bridge lifecycle: created on first sandbox, destroyed on last
- Update IP pool to reflect per-user pools
- Keep the kernel `ip=` section — it's correct and well-written

---

## Week 5: Virtualization and Snapshots

### What to learn

This is the core of bhatti — what Firecracker does, how snapshots work,
and why you disabled diff snapshots.

**Read your code:**
- `pkg/engine/firecracker/engine.go` — `Create()` (lines 278-610) and
  `Stop()` (lines 652-770) are the two most important functions
- `pkg/engine/firecracker/snapshot.go` — Checkpoint and ResumeSnapshot
- `pkg/engine/firecracker/jail.go` (40 lines) — path resolution

**Study:**
- Firecracker design doc (15 min):
  https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md
- Firecracker snapshot docs:
  https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md
- Julia Evans on Firecracker:
  https://jvns.ca/blog/2021/01/23/firecracker--start-a-vm-in-less-than-a-second/

**Concepts to nail down:**
- Firecracker uses KVM (/dev/kvm). The CPU runs guest code directly on
  hardware. Privileged instructions trap to Firecracker, which handles them.
- Virtio: standard for virtual devices. virtio-blk = virtual disk (your
  rootfs.ext4 files), virtio-net = virtual NIC (your TAP device).
- Snapshot = vm.snap (device state, JSON or binary) + mem.snap (full RAM
  dump). Restore loads both into a new FC process.
- Diff snapshots only write dirty pages. You disabled them after the rory
  incident — dirty page tracking missed some host-side virtio ring writes,
  causing silent corruption. All snapshots are now Full.
- Vsock: Firecracker's host↔guest communication channel. Breaks after
  snapshot/restore because FC's proxy state is lost. You work around this
  with TCP over virtio-net (the TAP device).
- Jailer: chroot + UID drop + cgroups. FC runs as non-root in a restricted
  filesystem view. Your jail.go handles path resolution between host and
  chroot paths.

**Questions to answer yourself:**
1. What are the 8 Firecracker API endpoints your code calls? (Trace through
   Create — each `fcPut` is one endpoint)
2. Why does `Stop()` pause BEFORE creating a snapshot?
3. Why does `Stop()` NOT destroy the TAP device?
4. What does `injectLoharIntoRootfs` do and why is it necessary?
5. What does the `restoreFailed` circuit breaker protect against?
6. In `Start()`, why create a new `AgentClient` with TCP instead of
   reusing the old one?

### Docs to rewrite: `architecture.md` and `thermal-management.md`

**architecture.md** problems:
- Diagram shows Docker engine (removed), single bridge (now per-user)
- Project structure is stale — missing jail.go, ringbuffer.go, backup/,
  oci/, public_proxy.go
- No mention of jailer, balloon, OCI import, backup, public proxy
- Recovery section doesn't mention circuit breaker or force-start

**thermal-management.md** problems:
- Diff Snapshots section describes a feature that's been disabled
- "Why TCP" section says cold boot uses vsock — code always uses TCP
- No mention of balloon inflation on warm VMs
- No mention of force-pause after maxThermalFailures (10)
- No mention of thermal failure counters or circuit breaker
- No mention of snapshot sanity checks

Rewrite goals for `thermal-management.md`:
- Update to reflect: all snapshots are Full, dirty page tracking off
- Add a short section on why diff was disabled (the rory incident, link
  to PLAN-reliability.md)
- Document balloon: inflated to 50% of VM memory on warm, deflated on
  resume (deflate_on_oom)
- Document force-pause: 10 consecutive activity query failures → pause
- Document the circuit breaker on restore failures
- Remove or clearly mark the vsock-on-cold-boot text as historical
- Add snapshot sanity checks (verifySnapshotArtifacts)

---

## Week 6: The Wire Protocol and PTYs

### What to learn

Your wire protocol is simple and clever. The PTY handling is the most
"systems-y" part of lohar. Understanding both deeply makes you fluent
in the whole host↔guest communication path.

**Read your code:**
- `pkg/agent/proto/frame.go` (127 lines) — the ENTIRE framing layer
- `pkg/agent/proto/constants.go` — frame type definitions
- `pkg/agent/client.go` (740 lines) — host-side client
- `cmd/lohar/tty.go` — PTY allocation and session handling

**Study:**
- HTTP/2 framing (RFC 7540 section 4.1) — same concept as yours, more
  features. Seeing the parallel validates your design:
  https://httpwg.org/specs/rfc7540.html#FrameHeader
- Terminal/PTY explainer:
  https://yakout.io/blog/terminal-under-the-hood/

**Concepts to nail down:**
- Binary framing: 4 bytes length + 1 byte type + N bytes payload.
  Length includes the type byte. Max 1MB per frame.
- Atomic writes: entire frame assembled into one buffer, one Write() call.
  Without this, concurrent goroutines (stdout + stderr) would interleave
  bytes on the wire, producing corrupt frames.
- PTY = master + slave pair. Lohar holds the master. The child process
  (shell/command) uses the slave. The kernel's PTY layer between them
  handles echo, line editing, Ctrl+C, etc.
- Sessions survive disconnects because lohar keeps the master fd open.
  The 64KB ring buffer captures output while no one is attached.

**Questions to answer yourself:**
1. What happens if you write length, type, and payload in three separate
   Write() calls while another goroutine is also writing?
2. Why does the forward protocol (port 1025) abandon framing after the
   handshake?
3. What does `ReadFrame` do when it encounters an unknown frame type?
4. In your session model, what prevents SIGHUP from killing the process
   when the host disconnects?

### Docs to rewrite: `wire-protocol.md`

Current problems:
- Opening paragraph says "vsock (cold boot)" — should say TCP always
- Otherwise this doc is mostly accurate and well-structured
- Could use a "Protocol Evolution" section — what can change without
  breaking existing guests?

Rewrite goals:
- Fix the vsock mention in the opening
- Add a note about protocol versioning / forward compatibility
- Trim any over-explanation (the file read/write sections are thorough
  but could be tighter)
- Add your voice — what surprised you about this protocol? What would
  you change?

---

## Week 7: Storage, SQLite, and Encryption

### What to learn

Atomic writes, WAL mode, ext4 images, age encryption — the data layer.

**Read your code:**
- `cmd/lohar/files.go` — atomic file writes inside the VM
- `pkg/store/store.go` (1545 lines) — SQLite store
- `pkg/secrets/age.go` — encryption at rest
- `pkg/engine/firecracker/configdrive.go` — config drive creation

**Study:**
- OSTEP Chapter 42: "Crash Consistency" — why fsync+rename works
  https://pages.cs.wisc.edu/~remzi/OSTEP/file-journaling.pdf (free)
- SQLite WAL docs — 15 min read
  https://www.sqlite.org/wal.html

**Concepts to nail down:**
- write() goes to page cache (RAM), not disk. fsync() forces to disk.
  Without fsync before rename, the renamed file could be empty after crash.
- rename() is atomic on POSIX. Reader sees old file or new file, never half.
- SQLite WAL: writes go to a log. Readers see a consistent snapshot without
  blocking writers. Critical for bhatti because thermal manager writes while
  API reads.
- Config drive: a 1MB ext4 image with a JSON config file. Mounted read-only
  in the VM. Contains hostname, token, env vars, files, volume mounts, init
  script. Everything the VM needs to configure itself before the agent starts.

**Questions to answer yourself:**
1. What if you skip fsync and just do write + rename? When does data loss
   happen?
2. Why WAL mode specifically? What would happen with the default journal
   mode when the thermal manager writes while the API reads?
3. What does the config drive contain that the agent can't get via exec?
   (Answer: everything — the agent doesn't exist yet when the config
   drive is read during boot)

### Docs to rewrite: `decisions.md` (add new entries)

Current problems:
- Missing decisions for: disabling diff snapshots, jailer integration,
  balloon device, per-user bridges, OCI import, S3 backup
- The existing entries are well-structured but all follow the same
  template. Vary the format based on importance.
- Entry 2 (No FC SDK) could be shorter — the rationale is clear
- Entry 11 (Bridge networking) describes the OLD single-bridge model

Add new entries:
- **Disabling diff snapshots** — the rory incident, what went wrong,
  why Full is safer even if slower
- **Jailer integration** — what it provides (chroot, UID drop, cgroups),
  what you dropped (PID namespace — breaks host socket access), phased
  rollout decision
- **Per-user bridges** — why one bridge per user instead of one shared
  bridge. L2 isolation, what attacks it prevents.
- **Balloon device** — reclaiming memory from warm VMs, deflate_on_oom

Update entry 11 to reflect per-user bridges.

---

## Week 8: State Machine, Concurrency, Full Picture

### What to learn

How the thermal state machine works, the concurrency model, and how
everything connects end-to-end.

**Read your code:**
- `pkg/server/server.go` — `runThermalCycle`, `SnapshotAll`, `ensureHot`
- `pkg/engine/firecracker/engine.go` — `EnsureHot`, `Pause`, `Resume`
- `cmd/bhatti/main.go` — recovery, graceful shutdown

**Study:**
- Go Memory Model (1 page):
  https://go.dev/ref/mem
  Key rule: mutex unlock happens-before the next lock. This is why
  capture-and-release is safe.

**Concepts to nail down:**
- The thermal state machine: hot ↔ warm ↔ cold. Any API request calls
  `ensureHot()` which transparently restores. Consumer never sees states.
- Capture-and-release: hold lock, read the Agent pointer, release lock,
  use the Agent. Safe because the pointer is only replaced during Start(),
  which also holds the lock.
- Activity cache: `sync.Map` of engineID → last API timestamp. Avoids
  TCP connection to every VM every 10 seconds. Only idle VMs get queried.
- SnapshotAll: parallel bounded (10 concurrent), retry once on failure,
  leave VM running if both attempts fail (live > dead).
- Recovery: on startup, read all sandboxes from SQLite, restore to
  engine's in-memory map. Check snapshot files exist. Mark unrecoverable
  ones as "unknown".

**Questions to answer yourself:**
1. Draw the complete state machine including error states. What state is
   a VM in after a failed restore? After SnapshotAll fails?
2. What happens if Destroy() is called while Exec() is using a captured
   Agent reference?
3. Why does SnapshotAll leave the VM running on double failure instead of
   killing it?
4. What's the difference between the host-side activity cache and the
   guest-side activity tracking? When does each matter?

### Docs to rewrite: `architecture.md` (the big one)

This is the capstone rewrite. By now you understand every piece.

Current problems (summary):
- Diagram shows Docker engine (gone) and single bridge
- Project structure is stale
- Missing: jailer, balloon, OCI, backup, public proxy, rate limiters,
  snapshot sanity checks, circuit breaker, force-pause
- Recovery section doesn't mention restoreFailed / circuit breaker
- No mention of SnapshotAll parallel bounded retry behavior

Rewrite goals:
- New ASCII diagram reflecting: per-user bridges, jailer (optional),
  balloon device, no Docker engine
- Updated project structure matching actual files
- Updated "Recovery" section with circuit breaker, force-start
- New section: "Jailer" — what it does, bare vs jailed mode
- New section: "Backup" — S3-compatible, scheduled
- Updated concurrency section with force-pause behavior
- The data flow diagrams (Exec, Streaming Exec) are still correct — keep

---

## Documentation Style Guide (for your rewrites)

When you rewrite, keep these principles:

**1. Lead with what a reader needs, not what you know.**
Don't explain what a bridge is from scratch. Say "each user gets their own
Linux bridge — VMs on different bridges are isolated at L2" and link to a
resource for people who don't know what a bridge is.

**2. Vary structure by importance.**
Critical things (snapshot corruption, security model) get detailed
treatment. Operational details (MAC address generation) get one paragraph.
The current docs treat everything equally.

**3. Say what surprised you.**
"We expected vsock to work after snapshot restore. It doesn't." is more
useful than a formal Context → Alternatives → Decision structure. Some
decisions are straightforward and don't need three paragraphs.

**4. Mark what's intentionally missing.**
"We don't use PID namespaces because they break host socket access.
Revisiting when we add per-VM network namespaces." is better than silence.

**5. Date your docs.**
At the top of each doc: "Last updated: [date]. Reflects code as of [commit]."
This tells future-you (and contributors) whether to trust it.

**6. Cut aggressively.**
The current `decisions.md` is 4800 words. It could be 2500 and be clearer.
If a section makes one point, make it in one paragraph, not three.

---

## The Schedule

| Week | Learn | Rewrite |
|------|-------|---------|
| 1-2 | Processes, signals, PID 1, mounts | `guest-agent.md` |
| 3-4 | Networking, bridges, TAP, iptables, NAT | `networking.md` |
| 5 | Firecracker, KVM, virtio, snapshots, jailer | `thermal-management.md` |
| 6 | Wire protocol, PTYs | `wire-protocol.md` |
| 7 | Filesystems, fsync, SQLite WAL | `decisions.md` (add entries) |
| 8 | State machine, concurrency, full picture | `architecture.md` |

After week 8, update `README.md` — it references the old architecture
diagram and doesn't mention jailer, balloon, per-user bridges, or backup.

---

## How You Know It Worked

You'll know you own this when:

1. Someone reads your rewritten docs and they sound like a person wrote them
2. You can trace `bhatti exec dev -- echo hello` from keystroke to output
   and explain every step without looking at code
3. You can look at a bug report and immediately know which file to open
4. You can explain the rory incident — what happened, why, and how you
   fixed it — in a conversation, not by reciting a doc
5. Your git log for the doc rewrites tells its own story — each commit
   message reflects something you understood for the first time
