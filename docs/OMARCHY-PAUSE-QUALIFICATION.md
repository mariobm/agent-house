# Omarchy boot measurement and resident pause

Qualification on `agent_house`, 2026-10-08: Linux x86_64/KVM, Intel UHD 770
render node, immutable Omarchy `4.0.4-20260922` image, 2 vCPUs and 8 GiB guest RAM.
The implementation enables resident control only for the Linux x86_64 GPU
desktop profile. Software Ubuntu/XFCE and macOS desktop pause remain disabled.
Other GPU hardware needs its own qualification; these samples are not a latency
guarantee. No Omarchy spare is created by this change.

## Boot boundaries and observed bottleneck

The reported released Cloud create was about **29.9 seconds**. A subsequent
released v0.4.0 cold-start request for the existing Cloud Omarchy VM took
**34.96 seconds**. Those are cold boots that recreate RAM and desktop processes.
They are different operations and disk histories from a clean disposable local
overlay; compare each boundary directly, not as interchangeable benchmarks.

The existing guest reported 12.922 seconds before systemd userspace and 16.291
seconds of userspace startup, 29.213 seconds in total. Its original `dmesg`
timestamps contain an **11.077-second interval** between encrypted-key
registration at 0.163669 seconds and ext4 reporting orphan-inode deletion at
11.240356 seconds. That interval brackets root-device/mount/recovery work; it
does not prove 11 seconds of kernel CPU execution or identify one responsible
I/O operation. `/init.krun` appeared at 11.330668 seconds and systemd's next
observed message at 14.130195 seconds. Kernel messages replayed into journald
later are unsuitable for timing that interval.

Forge's systemd unit spent another **7.199 seconds** waiting for the private
desktop-ready marker. That includes desktop/VNC startup. The current image
starts Forge only after the relay is ready, so Forge readiness cannot be used
as a separate measurement of pure agent startup. A usable framebuffer may also
arrive after that marker: a local cold restart's first full RFB response was
blank, then a later requested frame painted successfully.

This evidence points to the cold disk/guest startup path and desktop readiness
as the major measured intervals. It does not justify an image or kernel tuning
change yet. Resident pause avoids that cold path for short idle periods.

With `AHVM_TIMINGS=1`, the runtime now emits these bounded JSON timing stages;
command contents and credentials are excluded:

| Stage | Boundary |
| --- | --- |
| `daemon/create_request`, `daemon/start_request` | Node request lifecycle, including admission, scheduler and store mirror |
| `engine/overlay_prepare` | Local overlay creation |
| `engine/volume_prepare`, `engine/volume_attach` | Replicated preparation and disk attachment |
| `engine/worker_boot` | Worker-spec/cgroup/network preparation and isolated broker launch; excludes guest startup |
| `engine/guest_ready`, `engine/ready_probe` | Forge readiness wait and nested attempts |
| `engine/guest_network`, `engine/volume_bind` | Guest network setup and replicated worker/disk binding |

The qualifier independently timestamps the first valid RFB banner and completed
nonblank framebuffer, before PNG encoding. Short early-banner attempts avoid
counting a stalled pre-relay connection as ten seconds of guest startup. The
remaining polling, connection scheduling and framebuffer transfer costs are
part of the measured first-pixel boundary. API receipt and first paint are
reported separately. Nested stages overlap and must not be summed with their
parents. Public Cloud routing/client latency is outside node stage logs and
the host-loopback qualifier.

## Local qualification

Three preliminary raw control cycles passed before enabling automatic desktop
pause. Direct control resume to completed framebuffer measured 38–47 ms. After
enablement, ten consecutive authenticated WebSocket reconnects from observed
automatic pause measured **65–87 ms** to completed nonblank framebuffer. These
are host-side local samples, including node admission, VNC handshake and raw
framebuffer transfer, excluding Cloud routing and the desktop-viewer app.

Each repeated cycle retained the same worker PID/start time, guest boot ID,
Hyprland, WayVNC and Foot process identities. A shell variable set only in RAM
survived and was read through VNC keyboard input after every wake. Pointer input,
nonblank framebuffer and guest IPv4 HTTPS were verified on each cycle.

Targeted follow-ups completed the recovery/cold behavior checks. The initial
negative-resume qualifier expected HTTP 500; the product correctly returned
HTTP **502 / `backend_unavailable`** for its deliberately hidden control socket.
The corrected gate requires that error, verifies the paused state and worker
identity remain intact, restores the socket and resumes the same applications.
The initial cold framebuffer assertion was also changed to wait up to 20 seconds
for real nonblank paint while holding a connected viewer guard. Neither issue
required a product lifecycle change.

The complete local gate passed:

- Quiet authenticated viewer and shell connections each remained running for
  40 seconds, past the accelerated 5-second pause and 35-second stop thresholds.
- Daemon restart adopted a resident-paused desktop without waking or replacing it.
- Running and paused GPU workers refused `SNAPSHOT` without changing state or
  creating the requested directory.
- A five-second paused sample consumed **0 observed worker CPU ticks**. RAM and
  resource reservations remained charged; this does not promise zero host CPU.
- Long idle stopped the worker, wrote no RAM checkpoint, preserved a file, and
  cold-started with a new process/guest boot identity and working framebuffer.
- A deliberately killed paused worker was identified through its exact cgroup,
  start time and pidfd, surfaced Failed, and recovered only through explicit
  cold start with the retained disk.
- The ordinary 30-second policy paused after **31.83 seconds** in one observation,
  then resumed on desktop reconnect.

In the complete local run, create API receipt was 6.714 seconds, the first RFB
banner 7.198 seconds and the first nonblank frame 7.388 seconds. Accelerated idle
cold start took 6.594 seconds at the API; explicit recovery after worker death
took 6.286 seconds. These clean local-disk observations do not explain or replace
the existing replicated Cloud cold-start sample.

## Replicated qualification

The same complete gate passed for one disposable replicated desktop, including
ten automatic pause/wake cycles. Minimum/median/maximum authenticated node
WebSocket wake to nonblank framebuffer were **63.85 / 74.21 / 142.92 ms**. The
worker, graphical processes and RAM variable remained unchanged on all ten
wakes. Both quiet connection guards, paused daemon adoption, GPU snapshot
refusal, failed-control recovery, disk-only cold stop and killed-worker explicit
recovery passed. The five-second paused worker CPU sample recorded zero ticks.
The ordinary 30-second policy paused after **31.15 seconds** in one observation.

The initial create API receipt took **17.010 seconds**, the first RFB banner
17.694 seconds and the first nonblank frame **17.719 seconds**. Later idle cold
start took **36.067 seconds** at the API; explicit recovery after killing a paused
worker took **21.810 seconds**. Builds/clippy finished before this replicated
series. Shared immutable base and verified metadata caches were reused; the
image was not reimported. These are loopback fixture measurements, including
the test admission gate, not public Cloud timings.

| Non-overlapping node stage | Create | Idle cold start | Failed-worker cold start |
| --- | ---: | ---: | ---: |
| Volume preparation | 0.993 s | Already prepared | Already prepared |
| Disk attach | 0.792 s | 0.817 s | 0.021 s |
| Worker launch | 0.092 s | 0.122 s | 0.108 s |
| Forge/guest readiness | **14.836 s** | **34.971 s** | **21.548 s** |
| Guest networking | 0.044 s | 0.088 s | 0.021 s |
| Disk/worker bind | 0.042 s | 0.032 s | 0.034 s |
| Node request total | 17.008 s | 36.065 s | 21.808 s |

The cold-start sample reproduces the long wait while attachment and worker
launch remain below a second. The dominating stage is guest readiness, which
includes disk-demand I/O, guest startup and the image's desktop marker wait.
This does not isolate one kernel service or prove that CPU execution is the
cause. Resident resume avoids recreating those guest processes.

Native retirement/reclamation completed before the exact test prefix was
cleaned and verified empty; one final remote object was removed. NBD3 was then
size zero with no client PID and root ownership, the fixture loop was unmounted
and detached, and no fixture cgroups or builder bind mounts remained. The fresh
2-GiB backing file, copied fixture runtimes/data and test auth databases/env files
were removed after those checks. Seven small fixture directories retain results,
logs, frames and ownership metadata (about 8 MiB total). Their UID/GID reservations
remain permanently reserved. The production daemon stayed active with the same
PID; production services/configuration/quota roots and user VM data were unchanged
by the fixture runs.

## Reproduction and ownership

`scripts/qualify-desktop-pause.py` runs only with explicit `--execute`, root,
a fresh `/var/tmp/ahvm-omarchy-pause-*` directory and a new reserved UID/GID
range. Use the host's reachable IPv4 DNS resolver; UDP DNS to 1.1.1.1 was
unreachable during this qualification, while the configured host resolver worked.
The script never edits production services, user VM disks or immutable images.

The fixture uses its own hardened worker broker, private worker jails, transient
units and delegated cgroups. Worker identities are distinct from the daemon and
have no effective capabilities; Landlock and no-new-privileges remain enforced.
The durable identity-range reservation is retained after cleanup. Only the exact
owned units/cgroups are stopped. Cleanup failure makes the qualification fail.

For replicated mode, use an unused NBD device outside the production pool, a
fresh 2-GiB regular backing file formatted as XFS and its newly allocated loop
device. A separate bounded volume supervisor reuses immutable shared base data
and read-only copied verified caches. Its root-private admission proxy fsyncs
the native random volume ID on the initial resources request, verifies that
exact remote prefix is empty, and only then forwards preparation. Private test
objects are confined to that recorded volume ID. Native retirement/reclamation
must complete before remote cleanup; shared bases and other volume IDs are never
deleted. A failed ownership check retains evidence for safe operator recovery.

Source tests passed on macOS and Linux; Linux ran 78 daemon, 70 engine, 5 protocol
and 2 GPU-worker unit tests. App clippy with warnings denied and formatting
passed. The fork Makefile BLK/NET/GPU/INPUT build and default clippy passed. Its
required SEV and TDX checks fail in unchanged `virtio/persist.rs` because TEE
builds lack `Rng`, `RngState` and `TYPE_RNG`; baseline HEAD reproduces those
errors on baseline **`79cdb626fff10c16731c6f4bec690dacd7feec59`**. The full device
clippy check additionally reproduces an existing redundant
`&buffer` at `virtio/input/worker.rs:243` on Rust 1.98. The actual app GPU build
and clippy (GPU/blk/net without input) passed. These unrelated fork matrix issues
are not fixed in this change.
