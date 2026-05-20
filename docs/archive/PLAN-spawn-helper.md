# Fix the cgroup-placement race with a `lohar spawn` helper

Status: shipped in v1.11.10 (commits 623e8ad...e60035a).
Target release: `v1.11.10` (patch — bug fix, no API/UX changes).
Touches: `cmd/lohar/main.go` (~5 lines for argv dispatch) + `cmd/lohar/spawn.go` (new, ~60 lines) + `cmd/lohar/systemctl.go` (refactor `startDaemon`'s spawn call, ~10 lines net) + `cmd/lohar/spawn_test.go` (new, ~120 lines) + `cmd/lohar/cgroup_test.go` (one new assertion).

---

## Why

`v1.11.9` shipped the computer-tier conversion: four shim-managed units replacing `init.sh`. First boot works end-to-end (KasmVNC up on :6080, XFCE renders, `screenshot` works, password generation runs once). **Restarting `kasmvnc.service` fails.** The unit transitions to `failed (Result: exit-code, code=1)` and stays there; the journal shows:

```
_XSERVTransSocketUNIXCreateListener: ...SocketCreateListener() failed
_XSERVTransMakeAllCOTSServerListeners: server already running
Fatal server error:
Cannot establish any listening sockets - Make sure an X server isn't already running
```

Source of the conflict: the previous `Xkasmvnc` process is **still alive after stop**, still holding the abstract Unix socket `@/tmp/.X11-unix/X99`. No file-system clean-up frees an abstract socket — only killing the holder does.

`systemctl stop kasmvnc` *should* have killed it. The shim does `cgroup.kill` on the unit's cgroup, which the kernel translates into SIGKILL for every PID listed in `cgroup.procs`. Why didn't `Xkasmvnc` go down with it?

```
$ sudo cat /proc/$(pgrep -x Xkasmvnc)/cgroup
0::/

$ sudo cat /sys/fs/cgroup/system.slice/kasmvnc.service/cgroup.procs
(empty)
```

`Xkasmvnc` is in the **root cgroup**, not the unit's. `cgroup.kill` on the unit's cgroup has nothing to kill.

The shim's `startDaemon` (`cmd/lohar/systemctl.go`) acknowledges this exact failure mode:

```go
// Move the process into the unit's cgroup. The window between
// cmd.Start() and this write is brief but real — the daemon runs
// briefly in the parent's cgroup. systemd avoids this with
// CLONE_INTO_CGROUP via clone3, which Go's os/exec doesn't expose.
// For our use case this is acceptable: the daemon is doing exec
// setup (loading shared libs, parsing config) during this window,
// not consuming bulk resources.
```

The unstated assumption — *the daemon is a single process by the time we write to `cgroup.procs`* — is true for `dockerd`, `containerd`, anything written as a foreground daemon. It is **false for X servers**, which `daemon(3)` on startup (fork, parent exits, child detaches). When the fork happens during the race window, the child inherits whatever cgroup the parent was in — `lohar`'s cgroup, i.e. `0::/`. That child is the long-running `Xkasmvnc` we then can't stop.

The same shape applies to anything that double-forks during startup: classic Apache, `dbus-daemon` in default mode, `nginx` without `daemon off;`. This isn't an Xkasmvnc bug — it's a contract bug. Right now the shim's stop semantics depend on every daemon author cooperating with our race window, and we have no way to enforce that without auditing every tier on every contribution. So we'd be filing it as a footgun for every future tier author. We don't want to.

The fix is to **remove the race entirely** by placing the cgroup before any fork happens.

---

## Scope

### In

- A new `lohar spawn` subcommand whose entire job is "enter this cgroup, then `execve` into this argv". One file, no business logic.
- `cmd/lohar/main.go`: extend the argv-based dispatch (already used for `systemctl`/`journalctl`) to recognise `spawn` as a verb.
- `cmd/lohar/systemctl.go`: replace the `exec.Command("/bin/sh", "-c", "exec "+execStart)` + post-spawn `cgroup.procs` write with a single `exec.Command("/proc/self/exe", "spawn", "--cgroup", path, "--", "/bin/sh", "-c", "exec "+execStart)`. The post-spawn `PlaceInCgroup` call is removed outright (not left as a "defensive copy") — two writers to the same kernel file means two places that can drift and a fuzzier contract for `lohar spawn`. The new integration test (below) is the safety net.
- Tests: a `cmd/lohar/spawn_test.go` covering the happy path, the cgroup-write failure path, and the argv-parse edge cases; one new assertion in `cgroup_test.go` (or a new `TestStartDaemonPlacesIntoUnitCgroup`) that reads `/proc/<pid>/cgroup` after a fresh start and confirms it ends in `<unit>.service`, not `0::/`.

### Out (deliberately deferred)

- **`clone3(CLONE_INTO_CGROUP)`.** What real systemd does. Atomic, kernel-side, no helper process. Implementation cost — raw `unix.Syscall6` with a `clone_args` struct, reproducing what `syscall.ForkExec` does post-fork (close-on-exec, signal handler reset, credentials, working dir, the lot), all while holding `syscall.ForkLock` to keep other goroutines safe — is real surgery on a piece of code where bugs are very expensive. We pick the spawn helper because it gives us the same correctness guarantee with plain Go and `~60 LOC` we can read in one sitting. `clone3` stays available as a future migration if a benchmark ever shows the extra `execve` matters.
- **Migration of the `Type=forking` path.** `startForking` (`systemctl.go:881`) currently calls neither `CreateCgroup` nor `PlaceInCgroup` — Type=forking daemons get **no** cgroup placement at all today, not a racy one. None of the four built-in tiers uses `Type=forking`. When the first one does, that's the moment to bring forking under the spawn helper (the mechanism generalises directly; it just isn't wired). Tracked here as an explicit deferral rather than left as an unstated gap — so a future contributor reading "Type=forking is unaffected by this race" doesn't assume it's also placed correctly.
- **Recursive descendant kill on stop.** Would be a workaround for the same problem from the other end — fragile, doesn't fix the resource-accounting half of the bug. The spawn helper supersedes it.
- **User=/Group= drop semantics.** Today the shim doesn't honour `User=`/`Group=` at all — `startDaemon` only sets `Setsid: true` on `cmd.SysProcAttr` (line 692); no `Credential` is set, and the directive isn't parsed. The spawn helper doesn't change that. If a future tier needs uid drop, the right home is **inside `lohar spawn`**, between the `cgroup.procs` write and the `syscall.Exec`: the helper writes its PID while still root (cgroup.procs is `0644`, root-owned, so non-root processes cannot write it), then drops credentials via `setresgid`/`setresuid`, then execs. Doing it on the outer `exec.Command` instead would mean the helper inherits the dropped uid and can no longer write `cgroup.procs`, which would break the whole mechanism. One subtlety: the cgroup directory must already exist when `spawn` runs. `CreateCgroup` is called before `exec.Command` today, so this is satisfied.

---

## Design

### The mechanism

```
                  ┌──── lohar (PID 1, supervisor) ───────────────┐
                  │                                              │
                  │   CreateCgroup(/sys/fs/cgroup/.../X.service) │
                  │                                              │
                  │   cmd := exec.Command("/proc/self/exe",       │
                  │       "spawn", "--cgroup", <path>, "--",     │
                  │       "/bin/sh", "-c", "exec "+execStart)    │
                  │   cmd.Start()  ─────────────────────────┐    │
                  └─────────────────────────────────────────┼────┘
                                                            │
                  ┌── new process, lohar binary ────────────▼────┐
                  │   argv[0] = "lohar"                          │
                  │   argv[1] = "spawn"                          │
                  │                                              │
                  │   1. write own PID to <cgroup>/cgroup.procs  │
                  │       ┌─────────────────────────────────┐    │
                  │       │ kernel: PID is now in <unit>    │    │
                  │       │ cgroup. ANY future fork() from  │    │
                  │       │ this PID — by any descendant —  │    │
                  │       │ inherits this cgroup.           │    │
                  │       └─────────────────────────────────┘    │
                  │                                              │
                  │   2. syscall.Exec("/bin/sh",                 │
                  │         []string{"/bin/sh", "-c",            │
                  │             "exec "+execStart},              │
                  │         os.Environ())                        │
                  │                                              │
                  │      PID preserved across execve. Becomes:   │
                  │      /bin/sh which exec's into the daemon.   │
                  │      Daemon's PID == cmd.Process.Pid above.  │
                  └──────────────────────────────────────────────┘
                                       │
                                       ▼
                            (daemon may fork; children
                             inherit the unit's cgroup)
```

### Why this works

Two invariants do all the work:

1. **`cgroup.procs` write is observed by the kernel before any descendant of the writer is born.** The race in the current shim is that we write `cgroup.procs` *after* `cmd.Start()`, which means the freshly-forked child can fork its own children in a window where it's still in the parent's cgroup. With the spawn helper, the write happens at the very top of `lohar spawn`'s main, before any `fork(2)` call from this process. The only thing that could create a descendant in the wrong cgroup is `lohar spawn` itself forking before writing — and `lohar spawn` only writes-then-exec's, with no `os/exec` calls in between.

2. **`execve` does not change cgroup membership.** Cgroup membership is a property of the process, not of its loaded image. So after `lohar spawn` `syscall.Exec`s into `/bin/sh`, then `/bin/sh` `exec`s into the daemon (via the `exec` keyword in `sh -c "exec ..."`), the daemon — same PID throughout — is still in the unit's cgroup. Any subsequent `fork()` it does (X-server detach, dbus pre-forking workers, whatever) inherits the unit's cgroup. Stop-time `cgroup.kill` catches all of them.

**Counting execves honestly.** Three execves run on the spawn path, all PID-preserving and cgroup-preserving:

  - `#1`: `cmd.Start()` does fork + execve. The child execve's into `lohar` (now in spawn mode via the argv[1] verb).
  - `#2`: `lohar spawn` does `syscall.Exec("/bin/sh", ...)`. Execve into `/bin/sh`.
  - `#3`: `/bin/sh -c "exec <execStart>"` does execve into the daemon binary (the `exec` builtin replaces the shell, no extra fork).

Cgroup.procs is written between #1 and #2 — the only point where it can be done correctly. The supervisor's `cmd.Process.Pid` is the PID throughout all three execves and is what ends up in the unit's pidfile.

### Argv-dispatch axis (it's not a busybox symlink)

`lohar spawn` is dispatched by **argv[1] verb**, not by argv[0] basename. This is different from `systemctl` and `journalctl`, which are dispatched by the busybox/symlink pattern at `main.go:29` (`switch filepath.Base(os.Args[0])`). The dispatch site for `spawn` is added separately, before the PID-1 guard, so it runs as a transient helper process and never enters `runAgent`.

We deliberately do **not** symlink `/proc/self/exe` to `/usr/bin/spawn` or any other PATH entry. `lohar spawn` is a private supervisor primitive; making it look like a user-facing verb would invite people to wrap it themselves and pin its argument shape as a public contract. The argv[1]-verb form keeps it discoverable only via `grep` of the source, which is what we want for an internal mechanism.

### Implementation note: keep the race window between cgroup write and exec tidy

The correctness argument relies on "`lohar spawn` only writes-then-exec's, with no `os/exec` calls in between" — the implementation should keep that span as small as the Go runtime allows:

- No `defer` between the `os.WriteFile` and the `syscall.Exec`. Defers introduce a function-call boundary and a deferred-stack push.
- No heap allocations in that span (no `fmt.Errorf`, no `strings.Join`, no log lines on success).
- No opening or closing other file descriptors.

In practice the helper is: parse argv into local slices, `os.WriteFile`, check err, `syscall.Exec`. The window is one syscall wide on the happy path.

### Failure modes

| Failure | Behaviour |
|---|---|
| Cgroup directory doesn't exist (lohar's `CreateCgroup` didn't run or failed silently) | `os.WriteFile` returns `ENOENT`. `lohar spawn` prints `cgroup.procs: <err>` to stderr and `os.Exit(1)`. No daemon is started — clean failure visible in `journalctl -u X` as start error. |
| Permission denied on cgroup write | Same as above; `EACCES`. Caller sees non-zero exit code from `cmd.Wait()`. |
| Argv after `--` is malformed | `lohar spawn` exits 2 with usage. Hard error before any state change. |
| `syscall.Exec` fails (binary missing, permission) | Process dies; supervisor observes non-zero wait status. Same as today's "ExecStart pointed at a non-existent binary" case. |
| `lohar spawn` is killed between cgroup write and exec | Daemon never starts. Pidfile points at the dead lohar-spawn PID; `IsRunning` returns false on next poll; watcher's Restart policy kicks in (which re-spawns; same race we want to avoid is gone because each spawn is its own atomic placement). |

The interesting case is "killed between cgroup write and exec." This window is microseconds — write to a kernel pseudofile, then `execve`. We accept it for the same reason real systemd accepts the window between `clone3` and the kernel-side cgroup placement: it exists in theory but doesn't lose data in practice.

### Subcommand contract

```
lohar spawn --cgroup <path> -- <argv...>
```

- `--cgroup <path>`: absolute path to the cgroup directory. We write `<path>/cgroup.procs`.
- `--`: end-of-flags marker. Required, to avoid ambiguity with flags in the daemon's argv.
- `<argv...>`: argv[0] is the binary to execve. Anything from argv[1] onward is passed to it.

No other flags. No environment passthrough magic — environment is inherited from the parent `exec.Command` as today.

### Backward compatibility

- Every existing unit file works unchanged. The shim wraps `ExecStart` the same way it does today, just via one extra exec-chain link.
- `systemctl status`, `systemctl is-active`, `journalctl -u` all keep working the same way (they don't care about the spawn chain; they care about the pidfile, which still points at the daemon's PID).
- Pidfile contents are byte-identical to today: `cmd.Process.Pid` is the PID that survives both execve's, so the recorded PID matches the daemon's PID at every point.
- `KillMode=control-group` (the default) now works for every daemon, including forking ones. `KillMode=process` is unaffected.
- Existing snapshots / running sandboxes are unaffected — the change is in how *new* spawns are scheduled, not in any on-disk state.

### Alternatives considered (one-paragraph each)

**A. `clone3(CLONE_INTO_CGROUP)`.** Atomically place the forked child in the cgroup, like real systemd. Eliminates the helper layer. Costs: a few hundred lines of raw `unix.Syscall6` carefully wrapping Go's `syscall.ForkExec` semantics; runtime-lock ordering bugs are silent and only show up under load; depends on a Go version with `unix.SysClone3` (currently in `golang.org/x/sys` but the constants we'd need are still drifting). The spawn helper gives us the same correctness with no surgery. We pick it knowing `clone3` is a viable future migration if `execve` latency ever shows up in a benchmark (it won't — we measured the docker tier's `services_started` at 1028 ms, dominated by `dockerd` itself).

**B. Walk `/proc` on stop and kill descendants of the pidfile PID.** Doesn't fix the resource-accounting half of the bug (the orphan was in the wrong cgroup *all along*, not just at stop-time). Misses double-fork: when X-server's detach forks and the parent exits, the child gets reparented to lohar (PID 1, subreaper) and the PPid chain is lost. Plus all the usual `/proc` walk pitfalls (PID reuse, races with new processes during the walk). Rejected.

**C. Per-tier flag workarounds — `Xkasmvnc -dontdetach` or equivalent for every forking daemon.** Doesn't generalise. Every new tier becomes a new audit ("does this daemon fork on startup, what flag prevents it"). Punts the fix to every contributor adding a tier. The cost compounds. Rejected.

---

## Test plan

### Unit tests in `cmd/lohar/spawn_test.go`

1. **Happy path.** Create a tempdir, write `cgroup.procs` (just a file there, not a real cgroup), invoke `runSpawn(["--cgroup", tmpdir, "--", "/bin/echo", "hello"])`, redirect stdout, assert `hello\n`. Verifies argv parsing + the exec actually fires.
2. **Cgroup write failure.** Pass `--cgroup /nonexistent/path`, assert exit 1 and stderr mentions `cgroup.procs`. Verifies we exit cleanly on file errors.
3. **Missing `--` separator.** Pass `--cgroup X /bin/echo hi`, assert exit 2 and usage message. Verifies we don't accidentally treat `/bin/echo` as a flag value.
4. **Empty argv after `--`.** Pass `--cgroup X --`, assert exit 2.
5. **No `--cgroup` flag.** Pass `-- /bin/echo hi`, assert exit 2.

### Integration test in `cmd/lohar/cgroup_test.go` (or a new `start_daemon_test.go`)

`TestStartDaemonPlacesProcessInUnitCgroup`. Follows the same `t.Skip` pattern as the existing `TestCreateCgroup` in `cgroup_test.go`:

```go
if os.Getuid() != 0 {
    t.Skip("requires root for real cgroup operations")
}
if _, err := os.Stat("/sys/fs/cgroup/cgroup.controllers"); err != nil {
    t.Skip("requires cgroup v2 mounted at /sys/fs/cgroup")
}
```

Then operates against the **real** `/sys/fs/cgroup/system.slice` hierarchy. A synthetic tempdir cgroup root won't work — `/proc/<pid>/cgroup` reflects the kernel's view, not whatever directory tree we pointed `Config.CgroupRoot` at. Body:

1. Build a `Registry` with `Config.CgroupRoot = "/sys/fs/cgroup"` (real).
2. Write a tiny `.service` file with `ExecStart=/bin/sleep 30` into a tempdir.
3. Call `svcStart`.
4. Read `/proc/<pid>/cgroup` (where pid comes from `u.ReadPID()`); assert it ends in `system.slice/<unit>.service`. **Fail loud if it's `0::/`** — that's the bug we shipped this PR to close.
5. Clean up: `svcStop`, then verify the cgroup directory is empty / removed.

**Where it runs.** Skips on dev Mac and on any non-root Linux. **Runs for real on the existing Pi cluster GitHub Actions integration runner**, which executes as root with cgroup v2 mounted — the same environment that already exercises `TestCreateCgroup` and `TestApplyResourceLimits` in the same file. No new CI plumbing required.

This is the exact test that would have caught the v1.11.9 bug a year ago, and it would have caught the present spawn-helper PR regressing if we got the placement ordering wrong.

### Integration test on `agni-01`

```bash
# 1. Build a fresh computer-tier rootfs locally using the new lohar.
sudo ./scripts/build-tier.sh computer amd64 ./bin/lohar-linux-amd64

# 2. Side-import as computer-rc so we don't shadow production.
bhatti image import dist/rootfs-computer-amd64.ext4 --as computer-rc

# 3. Smoke: full restart cycle.
bhatti create --name rc-c --image computer-rc --cpus 4 --memory 4096
sleep 5
bhatti exec rc-c -- systemctl is-active kasmvnc                    # active
bhatti exec rc-c -- bash -c 'cat /proc/$(pgrep -x Xkasmvnc)/cgroup' # expect: 0::/system.slice/kasmvnc.service, NOT 0::/
bhatti exec rc-c -- sudo systemctl stop kasmvnc
sleep 2
bhatti exec rc-c -- pgrep -x Xkasmvnc                              # expect: empty (no orphan)
bhatti exec rc-c -- sudo systemctl start kasmvnc
sleep 3
bhatti exec rc-c -- systemctl is-active kasmvnc                    # active

# 4. Restart cycle on the docker tier — should still work (regression check).
bhatti create --name rc-d --image docker
sleep 5
bhatti exec rc-d -- bash -c 'cat /proc/$(pgrep -x dockerd)/cgroup'  # expect: docker.service
bhatti exec rc-d -- sudo systemctl restart docker
sleep 3
bhatti exec rc-d -- systemctl is-active docker                     # active
bhatti exec rc-d -- docker run --rm hello-world                    # works

# 5. Crash-and-restart on the browser tier.
bhatti create --name rc-b --image browser
sleep 3
bhatti exec rc-b -- sudo kill -9 $(pgrep -f headless_shell)
sleep 4
bhatti exec rc-b -- systemctl is-active headless-chrome            # active (Restart fired)
bhatti exec rc-b -- bash -c 'cat /proc/$(pgrep -f headless_shell)/cgroup'  # headless-chrome.service

bhatti destroy rc-c rc-d rc-b
```

If all green: tag, release. CI rebuilds rootfs (no rootfs change is needed for this fix — just the kernel-cmdline `init=/usr/local/bin/lohar` automatically loads the new binary; existing units don't need to change).

---

## Documentation

> **All paths in this section are in the `bhatti.sh` sister repo, under `src/content/docs/docs/...`** — not the `bhatti` monorepo's `docs/` (which is the design-doc archive, including this plan). The website docs and the engineering docs are deliberately separate trees. If you're looking for these files in `/Users/sahil/Projects/bhatti/docs/`, you won't find them.

The cgroup-placement mechanism itself is an implementation detail of the shim. But this release closes the loop on the v1.11.7→v1.11.10 migration: **every built-in tier is now shim-managed**, with the same operator UX. That uniformity is the actual headline, and several pages either don't say it explicitly enough or risk leading future contributors back to the `init.sh` pattern we just retired. The principle for this section: **document where someone could currently get the wrong impression**.

### `docs/under-the-hood/lohar-the-blacksmith.mdx`

Update needed. The page currently describes the systemctl shim's service lifecycle without saying how processes get placed in cgroups (it mentions cgroups exist but not the placement mechanism). After this fix, the placement is no longer a footnote-worthy invariant; it's *the* mechanism that makes the shim trustworthy across forking daemons.

Add a new section, **"How services are spawned"**, between the existing "The systemctl shim" overview and "What the shim doesn't do." Cover:

- The two-process spawn chain (`lohar spawn` then the daemon), with a short diagram similar to the one in this plan.
- Why: removes the cgroup-placement race for any daemon that forks during startup.
- The contract for unit-file authors: nothing changes. ExecStart works the same way.
- Reference: tier scripts (docker, browser, computer) use this transparently.

Also update **"What the shim doesn't do"** — currently it cites Go's lack of `clone3` as a constraint and presents the race as accepted. Reframe it: we don't use `clone3`, but we don't have the race either, because we use a different mechanism. Document `clone3` as a possible future migration if the extra `execve` ever shows up in profiling.

### `docs/under-the-hood/decisions.mdx`

Add a new decision entry: **"Spawn helper instead of `clone3`."** Two paragraphs:
- The problem (cgroup-placement race when daemons fork during startup).
- The choice (`lohar spawn`) and why over `clone3` (Go runtime fork-safety surgery vs. ~60 LOC of plain Go; same correctness; lower maintenance surface; matches the busybox-pattern dispatch we already use for `systemctl` and `journalctl`).

This is exactly the kind of "we considered X, picked Y, here's why" entry that the rest of the page is built around. Sets us up well for the day someone asks "why don't you just use clone3."

### `docs/managing/tiers/index.md`

Update needed. The page already has a "common operator story" section that gestures at the shim model, but with all four built-in tiers now uniformly shim-managed it's worth being concrete about *which units live where*. Two additions:

1. **A "Units shipped by each tier" reference table**, immediately after the four-tier overview table. One row per unit, columns for tier / unit name / Type / what it does. Lets a reader see at a glance that the shim isn't an abstract claim — it's how *every* tier's daemons are managed. Includes both daemons and the oneshot helpers (`kasmvnc-firstboot`, `bhatti-display-env`), because those participate in the same activation graph.

2. **Tighten the "common operator story" paragraph** to say explicitly that the model is uniform across tiers ("Every long-running process in every built-in tier is managed by lohar's systemctl shim. The same three commands — `systemctl status`, `journalctl -u`, `systemctl restart` — work the same way regardless of which tier you started from."). The current text gets close; this nails it.

No changes to the per-tier pages themselves — they each document their own unit layouts correctly already.

### `docs/contributing/adding-a-tier.md`

Update needed. **This is the doc most likely to lead a future contributor astray.** Today it walks through the build-script skeleton (`apt-get install`, size default, CI matrix entry, install-script menu) and stops there. Nothing tells a new tier author what to do with a long-running daemon, and the deprecated bhatti repo `docs/tiers.md` they might fall back on still mentions `init.sh`. Without an update they'll write `init.sh` because it's what the existing pattern in their head suggests — and they'll hit the exact race + orphan + restart-fails sequence this plan is closing.

Add a new section, **"Long-running daemons: ship them as systemd units"**, between "Create the tier script" and "Add size default." Cover:

- The standard unit-file pattern: `Type=simple` (most things) or `Type=notify` (sd_notify-aware daemons like `dockerd`), `ExecStart=` pointing at the binary directly (no `/bin/sh -c` wrapper unless you genuinely need `${VAR}` expansion that's not satisfiable via `Environment=`), `Restart=on-failure`, `RestartSec=2s`.
- The user-tunable knob convention: `Environment=` for defaults, `EnvironmentFile=-/run/bhatti/config-env` so `bhatti create --env KEY=value` overrides take effect. Reference the existing config-env bridge section in lohar internals.
- Where to drop the unit: `/etc/systemd/system/<name>.service` in your tier's chroot, then symlink into `/etc/systemd/system/multi-user.target.wants/<name>.service` so `startEnabledServices` picks it up at boot.
- A worked example using `headless-chrome.service` as the canonical short one (it's the simplest shipped unit: one Type=simple daemon with two env knobs and Restart=on-failure).
- An **"Anti-patterns"** subsection naming three landmines explicitly:
  - `init.sh` is legacy. Existing tiers used to use it; new tiers shouldn't.
  - Don't wrap `ExecStart=` in `/bin/sh -c '...'`. The shim already does that. An inner shell is at best redundant and at worst introduces the kind of fork that v1.11.10 closed.
  - Don't try to manage cgroups, PID files, or restart logic yourself. The shim handles all of it once your unit is enabled.
- A note that tier authors don't need to know about `lohar spawn` directly — their unit's ExecStart is what they care about; the spawn helper is upstream of that, invisible to the unit file.

Also add a one-line update to the **checklist** at the bottom of the page: "[ ] `scripts/tiers/<name>.sh` ships any required `.service` unit files and symlinks them into `multi-user.target.wants/`."

### Tier pages (browser, docker, computer)

**No changes.** Each tier page already documents its own unit layout correctly (we updated all three during the v1.11.8 / v1.11.9 work). The uniformity claim moves up to `tiers/index.md` where it belongs; the per-tier pages remain specific.

### `bhatti.sh/public/agents.md`

**No changes.** Agent-facing flows don't touch the spawn mechanism. Restart behaviour is already documented as part of the tier docs; agents.md cross-links to those. (Path note: this lives under `public/`, not `src/content/docs/docs/`, because it's the served-flat `/agents.md` route, not a docs page.)

---

## Risk + mitigations

| Risk | Mitigation |
|---|---|
| `lohar spawn` writes its PID to `cgroup.procs` but then fails to find the daemon binary (typo in ExecStart) — pidfile points at a dead lohar-spawn, watcher restart loops forever | Same behaviour as today's "ExecStart points at a missing binary": watcher sees rapid exits, hits `restart-burst` limit, gives up. Marked as failed in `is-failed`. The user sees a meaningful error in `journalctl -u` because `lohar spawn` writes its own startup failures there before `execve` succeeds. |
| The cgroup directory is removed between `CreateCgroup` and `lohar spawn` running | `os.WriteFile` returns `ENOENT`, spawn exits 1, supervisor reports failure. We don't currently delete cgroup dirs mid-spawn so this is theoretical; if it ever does happen the failure mode is clean. |
| Pidfile reads wrong because `cmd.Process.Pid` is set before the cgroup write | `cmd.Process.Pid` IS the PID we want in the pidfile — it's the PID of `lohar spawn`, which is preserved across the two `execve`s and ends up being the daemon's PID. No change to pidfile semantics. |
| The extra `execve` slows boot | Measured: a single `execve` is ~50 µs. Negligible against current `services_started` times (54 ms browser, 93 ms computer, 1028 ms docker). Will benchmark anyway. |
| `User=`/`Group=` drop interacts badly with `cgroup.procs` write | Not currently triggered — the shim doesn't honour `User=`/`Group=` at all today (no `Credential` is set on the outer `exec.Command`, the directive isn't parsed). Tracked future state: when a tier needs it, the drop lives inside `lohar spawn` after the `cgroup.procs` write and before the `syscall.Exec`. It must not be set on the outer `exec.Command` because `cgroup.procs` is `0644` root-owned and the dropped uid cannot write it. (Pre-writing the PID from the supervisor before `cmd.Start()` is the other alternative; it resurrects the race we just fixed, so it's a non-option.) |
| New tier authors don't read the docs and write `ExecStart=/bin/sh -c '...'` thinking it matters | It doesn't. The shim wraps in `/bin/sh -c "exec ..."` regardless, so an inner shell is at worst redundant (one extra exec). No incorrect-behaviour shape — the cgroup placement happens upstream of any of that. |
| The integration test sleeps don't catch a slow start | Each smoke step's `sleep` is sized to the slowest realistic boot (5 s for Type=notify dockerd's READY=1, 3 s for simple daemons). If a daemon's start consistently exceeds these, the unit's own `TimeoutStartSec=` would have caught it already. |

---

## Phasing

One PR, one tag (`v1.11.10`), in this commit order:

1. **`lohar spawn` subcommand.** New file `cmd/lohar/spawn.go` plus the argv dispatch hook in `main.go`. Includes the new `spawn_test.go`. Reviewable as a self-contained piece — does one thing, has no callers yet.

2. **Wire the supervisor through it.** Change `startDaemon` in `cmd/lohar/systemctl.go` to invoke `lohar spawn` via `/proc/self/exe`. **Remove the post-spawn `PlaceInCgroup` call outright** — no defensive copy. Two writers to the same kernel file means two places that can drift and a fuzzier contract for `lohar spawn`. The whole point of the helper is to make the write happen at a known correctness checkpoint; the new test is the safety net. Add `TestStartDaemonPlacesProcessInUnitCgroup` to `cgroup_test.go`.

3. **Smoke on agni-01.** Run the integration test plan above. If a real bug shows up here, fix it; don't ship a rotted v1.11.10.

4. **Tag `v1.11.10`.** CI builds binaries + rootfs (unchanged tier scripts, but rootfs rebuilds anyway so the new lohar gets baked in).

5. **Verify on a fresh sandbox** (the loop we ran for v1.11.8 and v1.11.9). Update the integration test plan if anything new turns up.

6. **`bhatti.sh` docs PR** (separate repo, separate PR). Four pages touched, in one commit, all paths under `src/content/docs/docs/`:
   - `under-the-hood/lohar-the-blacksmith.mdx` — new "How services are spawned" section + reframed "What the shim doesn't do" footnote.
   - `under-the-hood/decisions.mdx` — new "Spawn helper instead of `clone3`" entry.
   - `managing/tiers/index.md` — new units-per-tier reference table + tightened uniformity paragraph.
   - `contributing/adding-a-tier.md` — new "Long-running daemons: ship them as systemd units" section + anti-patterns callout + checklist line.

7. **Move this PLAN to `docs/archive/`** once shipped.

---

## Pre-flight verifications (done while writing this plan)

1. **`cgroup.procs` writes are permission-checked, not capability-checked.** Confirmed via the `Documentation/admin-guide/cgroup-v2.rst` kernel doc — the writing process needs write permission on `cgroup.procs` (`0644`, owner root) and write permission on a "common ancestor" cgroup, which lohar (PID 1, root) trivially satisfies. No `CAP_SYS_ADMIN` magic needed. The dropped-uid concern in the risk table is theoretical for now.

2. **PID preservation across `execve`.** Confirmed via `man 2 execve`: "The PID and parent PID do not change." So `cmd.Process.Pid` recorded by the supervisor is the PID that ends up running the daemon, after two execve steps. Pidfile semantics unchanged.

3. **Cgroup inheritance on fork.** Confirmed via `Documentation/admin-guide/cgroup-v2.rst`: a child is placed in the parent's cgroup at fork time. Any subsequent fork by the daemon (the X-server detach, dbus pre-fork workers, etc.) inherits the unit's cgroup since the daemon is placed there before its first fork.

4. **`syscall.Exec` is the right Go primitive.** Wraps `execve(2)` directly, doesn't fork. Matches what we want: replace the current process image, keep PID, keep cgroup membership. (Distinct from `exec.Command` + `.Start()`, which forks.)

5. **The two-`execve` chain (lohar → sh → daemon) is preserved across both transitions.** `lohar spawn` does `syscall.Exec("/bin/sh", ...)`, then sh's built-in `exec` keyword does its own `execve` into the daemon. Both are real kernel-level execves, both preserve PID and cgroup. The "sh as intermediary" is the same shape as today; the only thing we're adding is "lohar as intermediary" in front of it. No semantic shift.

6. **Existing per-unit cgroup creation is unchanged.** `u.CreateCgroup()` still runs before the supervisor calls `exec.Command(...)`. The new path consumes the same cgroup directory.

7. **`/proc/self/exe` is the right way to find lohar's binary path.** It's a kernel-maintained symlink to the inode of the binary currently running this process — cheaper than `os.Executable()` (no syscall on Linux; it's just a `readlink`-style resolve done by the kernel when `execve` opens the target), test-friendly (no install-path assumption baked in), and what `gosu`, `su-exec`, and `runc` all use for the same "re-exec myself" pattern. Survives the binary being renamed or moved after start because the symlink resolves through the inode, not the path.

8. **Type=notify activation-marker ordering survives the helper layer.** `svcStart` writes the `.activating` marker before calling `startDaemon` (`systemctl.go:585`). With the helper, the daemon's `sd_notify(READY=1)` could fire before the marker is cleared — same as today. The notify receiver attributes incoming messages by the sender's cgroup-membership (which is the unit's cgroup from execve #1 onwards), so the helper changes nothing here. The brief window where `/proc/<pid>/comm` reads `lohar` or `sh` instead of the daemon's name doesn't affect attribution; we don't use comm for anything.

## Open questions

1. **Should `lohar spawn` also handle the optional `User=`/`Group=` drop?** Not in v1.11.10. The shim doesn't honour `User=` today (verified: `startDaemon` sets no `Credential` on `cmd.SysProcAttr`), and no built-in tier needs it. When a future tier does, the drop lives **inside `lohar spawn`**, after the `cgroup.procs` write and before `syscall.Exec`. The cgroup write must happen as root because the file is `0644`, root-owned; doing the drop on the outer `exec.Command` would break the write. This open question is closed: the future home is recorded as part of the design (see the Out-of-scope paragraph above) rather than left as an undecided point.

2. **Should we add a defensive `pkill -9 -x <daemon>` ExecStartPre to `kasmvnc.service` as belt-and-braces?** No — once the cgroup placement is correct, there are no orphans to kill. Adding a `pkill` would mask future placement regressions. Better to fail loud (start fails with "server already running") if the spawn-helper invariant ever breaks, and re-investigate.

3. **Should we expose `lohar spawn` as a documented public verb for users to wrap their own commands?** No. It's an internal mechanism for the supervisor. Users who want cgroup-placed `bhatti exec` invocations can do that via `--cgroup` on the engine side, separately. The spawn helper stays "for systemctl shim use", not promised stable.
