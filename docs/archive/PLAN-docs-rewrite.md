# Documentation Rewrite — bhatti.sh

The bhatti.sh docs work. Every page is factually accurate, covers the
API surface, and has both CLI and API examples. Someone who needs to use
bhatti can figure it out from these docs.

The problem isn't correctness. It's that the docs don't know who they're
talking to. A user trying to publish a preview URL gets a paragraph about
singleflight deduplication. Someone who wants to understand how thermal
snapshots work gets the same level of depth as "bhatti secret list." And
the person who found bhatti on Hacker News and wants to understand the
engineering — the person PLAN-learning.md was written for — has to dig
through reference-style pages to find the interesting parts.

Three audiences, one flat sidebar, every page written in the same voice.

---

## Current State

### The sidebar

```
Getting Started
  Quickstart                 ← good, tight, works
  Self-Hosting               ← updated recently, solid
  Concepts                   ← good overview, earned its place
Sandboxes
  Lifecycle                  ← how-to + internals mixed
  Running Commands           ← how-to, clean
  Interactive Shell          ← how-to, clean
  Web Shell                  ← how-to, clean
  Files                      ← how-to + performance table bolted on
  Networking                 ← internals page wearing a how-to hat
  Preview URLs               ← how-to + singleflight/LRU implementation leaking
  Thermal Management         ← internals page, almost zero how-to
Managing
  Users & Auth               ← how-to, clean
  Secrets                    ← how-to, the env priority diagram is good
  Volumes                    ← how-to, clean
  Templates                  ← thin, says "managed via API" but no CLI note
  Images                     ← how-to, share/unshare semantics unclear
Reference
  API Reference              ← reference, fine
  CLI Reference (7 pages)    ← reference, fine
  Wire Protocol              ← internals, correct place in sidebar
  Configuration              ← incomplete — missing most server config fields
Architecture
  Overview                   ← internals + reference hybrid
  Guest Agent (Lohar)        ← deep internals, the best technical writing in the docs
  Firecracker Engine         ← internals, good
  Design Decisions           ← the best page in the entire site
Contributing
  Testing                    ← minimal, fine
  Kernel                     ← minimal, fine
```

### What's wrong — page by page

| Page | Problem |
|------|---------|
| **Lifecycle** | "What happens during create" lists 7 internal steps (TAP, rootfs copy, config drive, FC boot, agent poll). A user doesn't need this to create a sandbox. The options table is useful. The internal steps belong in Architecture. |
| **Files** | Performance table at the bottom (p50/p95 latencies) is a flex, not user documentation. These numbers are on the homepage and README already. |
| **Networking** | Almost entirely internals: bridge diagram, IP allocation pool, kernel `ip=`, TAP lifecycle. The only user-facing content is "every sandbox gets internet access" and the proxy examples. |
| **Preview URLs** | "Auto-wake" section explains singleflight, LRU cache size, ensureHot implementation. A user needs: "cold sandboxes wake automatically, first request takes ~50ms extra." |
| **Thermal Management** | Zero user-facing content. The entire page is the state machine internals, diff snapshot implementation, background goroutine tick. The only user-facing bits are: `--keep-hot` flag and `bhatti edit --allow-cold`. |
| **Templates** | Says "Templates are managed via the API" with no CLI. If there's no CLI, say "There's no CLI for templates yet." Don't leave a section header suggesting CLI content exists. |
| **Images** | `image share` / `image unshare` — who can call these? The page says "By default, images are scoped to the user who created them. Sharing makes them available to all users" but it's not clear if this requires admin access. |
| **Configuration** | Shows only `listen` and `data_dir`. The install script generates a config with `firecracker_bin`, `firecracker_kernel`, `firecracker_rootfs`, `jail_uid`, `jail_gid`, `firecracker_jailer`. This page is incomplete. |
| **Architecture Overview** | Tries to be both a system diagram and a reference. Data flow sequences are great for understanding the system, but the disk layout and concurrency model are reference material. |
| **Guest Agent** | The best technical doc. Boot sequence, config drive format, session model diagram, PTY allocation, process group kill rationale. But it's filing-cabinet'd under "Architecture" where casual readers won't find it. |
| **Design Decisions** | The best page on the site. Real problems, real tradeoffs, personality. But it's the last page in the Architecture section. The content that teaches the most is the hardest to find. |

### The "AI voice" markers

These patterns repeat across pages:

1. **Definition-sentence openings.** "Volumes are persistent ext4
   filesystems that can be attached to sandboxes." Every page opens by
   restating the title. Cut these — the title and sidebar context are
   enough.

2. **Uniform structure.** Every page: one-sentence intro → CLI section
   → API section → behavior/internals → reference link. Real docs vary.
   Some things need a paragraph. Some need a table. Some need a war
   story.

3. **Over-specified behavior sections.** "Process groups. Each exec
   creates a new process group (Setpgid: true). Kill signals target
   the entire group, so child processes (like those spawned by npm)
   are cleaned up." This is interesting engineering context, but it's
   in a page called "Running Commands" where someone just wants to
   know how to run npm install.

4. **Equal depth for everything.** The secret env priority diagram gets
   the same treatment as "bhatti secret delete KEY". One is a real
   question users have. The other is a one-liner that doesn't need a
   section.

---

## The Redesign

### Three tiers, clearly separated

**Tier 1: Docs** — "How do I do X?"
Task-oriented. Every page title is an action or a noun the user is
working with. No implementation details. Links to Tier 2 for the
curious.

**Tier 2: Under the Hood** — "How does this actually work?"
Engineering deep-dives. Each page stands alone as something you'd share
on Hacker News. This is where the Design Decisions page lives, alongside
the thermal state machine, the wire protocol, the guest agent internals,
and the vsock-breaks-after-restore story.

**Tier 3: Reference** — "Give me the spec."
CLI flags, API endpoints, config options, wire protocol frames. Lookup
tables, not prose.

### New sidebar

```
Getting Started
  Quickstart
  Self-Hosting
  Concepts

Sandboxes
  Create & Destroy
  Run Commands
  Interactive Shell
  Web Shell
  Files
  Preview URLs

Managing
  Users & Auth
  Secrets
  Volumes
  Images & Tiers
  Templates

Updating & Uninstalling              ← new, from our install work

Under the Hood                       ← new section
  Architecture Overview
  Lohar: PID 1 Inside Every VM
  Thermal States & Snapshots
  Networking: Bridges, TAP, and ip=
  The Wire Protocol
  Design Decisions

Reference
  CLI Reference
  API Reference
  Configuration
  Wire Protocol Frames              ← the byte-level spec, split from narrative

Contributing
  Development Setup                  ← new, currently missing
  Testing
  Building the Kernel
  Adding a Tier                      ← new, from tiers.md in the repo
```

### What moves where

| Current page | Goes to | What changes |
|-------------|---------|-------------|
| Lifecycle | **Sandboxes → Create & Destroy** | Cut the 7-step internal create sequence. Keep options table, CLI/API examples, stop/start, destroy. |
| Running Commands | **Sandboxes → Run Commands** | Keep as-is. Move "Process groups" and "Filesystem sync" to a callout or "Under the Hood" link. |
| Interactive Shell | **Sandboxes → Interactive Shell** | Clean, keep as-is. |
| Web Shell | **Sandboxes → Web Shell** | Clean, keep as-is. |
| Files | **Sandboxes → Files** | Drop the performance table. |
| Networking | **Under the Hood → Networking** | This was never a user guide. Move wholesale. Add a one-liner to the Sandboxes section: "Every sandbox gets its own IP and internet access. See [Networking under the hood](/docs/under-the-hood/networking/)." |
| Preview URLs | **Sandboxes → Preview URLs** | Cut singleflight/LRU internals to one sentence: "Cold sandboxes wake automatically (~50ms)." |
| Thermal Management | **Under the Hood → Thermal States & Snapshots** | Move wholesale. Add to Concepts page: "Idle sandboxes pause and resume automatically. See [Thermal States](/docs/under-the-hood/thermal/) for the engineering." |
| Users & Auth | **Managing → Users & Auth** | Cut "operates directly on the local SQLite database." Keep everything else. |
| Secrets | **Managing → Secrets** | Clean. Keep the env priority diagram. |
| Volumes | **Managing → Volumes** | Clean. Check that `volume restore` exists in the CLI. |
| Templates | **Managing → Templates** | Add explicit note: "Templates don't have CLI commands yet — use the API directly." |
| Images | **Managing → Images & Tiers** | Merge with tier documentation. Clarify who can share (admin only? any user?). Add tier table from README. |
| Architecture Overview | **Under the Hood → Architecture Overview** | Keep ASCII diagrams and data flow. Move disk layout and concurrency model to a subsection or separate page. |
| Guest Agent | **Under the Hood → Lohar: PID 1 Inside Every VM** | Rename for discoverability. Keep the deep content — it's good. |
| Firecracker Engine | Merge into **Architecture Overview** and **Thermal States** | The engine page is a grab bag. The VM creation sequence fits in Architecture. The snapshot/restore fits in Thermal States. |
| Design Decisions | **Under the Hood → Design Decisions** | Keep as-is. It's the best page. |
| Wire Protocol | Split: narrative → **Under the Hood → The Wire Protocol**, byte spec → **Reference → Wire Protocol Frames** | The current page mixes "why binary framing" (interesting) with frame type tables (reference). |
| Configuration | **Reference → Configuration** | Fill in the missing fields. Parse the actual Go struct and document every field. |
| CLI Reference | **Reference → CLI Reference** | Keep as-is. |
| API Reference | **Reference → API Reference** | Keep as-is. |

### New pages to write

**Updating & Uninstalling** — from our install UX work. `bhatti update`,
`--tiers`, the curl fallback, `bhatti.sh/uninstall`, `--purge`. This
exists in the repo README and quickstart.md but not as a standalone doc
page.

**Development Setup** — how to build bhatti from source, run it locally,
run the test suite. Currently undocumented. A contributor's first stop.

**Adding a Tier** — the checklist from `docs/tiers.md` in the repo,
adapted for the website. The bats tests validate this now.

---

## How to Rewrite Each Tier

### Tier 1 rewrites: cut, don't add

The Tier 1 pages (Sandboxes, Managing) are mostly fine. The fix is
**subtraction**: remove internal details, don't add new content.

For each page:
1. Read the page and highlight every sentence that describes
   *implementation* rather than *behavior*.
2. If the implementation detail answers a user question ("why is my
   sandbox slow to respond after being idle?" → "cold sandboxes take
   ~50ms to wake"), keep it as one sentence.
3. If it doesn't answer a user question, cut it or move it to a
   "Under the Hood" link.

Example cut from **Preview URLs**:

Before:
> When a request arrives:
> 1. The proxy looks up the alias in an in-memory LRU cache (10K entries)
> 2. If the sandbox is cold, `ensureHot()` restores it (~50ms)
> 3. Concurrent requests to the same cold sandbox share one wake via
>    `singleflight`
> 4. The request is proxied to the port inside the VM

After:
> Published URLs work even when the sandbox is sleeping. The first
> request wakes it (~50ms), then everything is instant.

Four lines → two lines. The user gets the same information. The curious
reader follows a link to the thermal states page.

### Tier 2 rewrites: add narrative

The "Under the Hood" pages need the opposite treatment — more context,
more "why", more of what went wrong. The Design Decisions page is the
template.

Each page should answer:
- What problem does this solve?
- What did we try first?
- What surprised us?
- What would we do differently?

**Thermal States & Snapshots** should include:
- The state machine (from current thermal page — good)
- Why diff snapshots were disabled (the rory incident — from
  PLAN-learning.md / PLAN-reliability.md)
- The balloon device for warm VMs (not documented anywhere currently)
- The force-pause circuit breaker (10 consecutive failures)
- Why vsock breaks after restore (from Design Decisions, expanded)

**Lohar: PID 1 Inside Every VM** should be the current Guest Agent page
with a better title. The boot sequence, config drive, session model,
PTY allocation — all strong content. Add:
- Why lohar is injected into the rootfs on every create (protocol drift)
- What happens when lohar crashes (PID 1 death = kernel panic = VM dies)

**Networking: Bridges, TAP, and ip=** — the current Networking page,
but rewritten to reflect per-user bridges (the current page still
describes the old single-bridge model per PLAN-learning.md audit).

### Tier 3: fill gaps

**Configuration** is the biggest gap. Parse the Go struct:

```go
type Config struct {
    Engine            string
    Listen            string
    DataDir           string
    Domain            *DomainConfig
    FirecrackerBin    string
    FirecrackerKernel string
    FirecrackerRootfs string
    FirecrackerJailer string
    JailUID           int
    JailGID           int
    Backup            *BackupConfig
}
```

Every field should be documented with its default, what it does, and
when you'd change it. The current page has 2 of ~12 fields.

---

## The AI Voice Fix

Not a separate pass — it's part of every rewrite. Three rules:

**1. Cut the definition sentence.** If the page is titled "Secrets",
don't open with "Secrets are key-value pairs encrypted at rest with
age." The reader already knows what page they're on. Open with what
they need to do: "Store sensitive values that are automatically injected
as environment variables."

**2. One page, one structure.** Secrets needs: set, list, delete, how
env priority works. That's it — CLI, API, the priority diagram, done.
Thermal States needs: the state diagram, the rory incident, the balloon
device, the circuit breaker. These pages have nothing in common
structurally, and they shouldn't pretend to.

**3. Let short pages be short.** Web Shell is 38 lines. That's fine.
Templates is 52 lines. Also fine. Don't pad them to match the 106-line
Lifecycle page. If a page answers its questions in 30 lines, it's done.

---

## Relationship to PLAN-learning.md

PLAN-learning.md covers rewriting the **repo docs** (`/docs/` in the
bhatti repo) as a learning exercise. Those docs are for contributors
and contain implementation details, code references with line numbers,
and architecture notes.

This plan covers the **website docs** (`bhatti.sh/docs/`). These are
for users and the curious public. The content overlaps — both have
architecture docs, both have a thermal management page — but the
audience and voice are different.

Where the two connect:
- PLAN-learning.md's staleness audit (diff snapshots disabled, per-user
  bridges, vsock always-TCP) applies to both. Fix in both places.
- PLAN-learning.md's rewritten repo docs feed the "Under the Hood"
  website pages. The repo version can be more code-heavy. The website
  version should be narrative.
- The "voice" rules from PLAN-learning.md (lead with what the reader
  needs, vary structure, say what surprised you, mark what's missing,
  cut aggressively) apply here too.

---

## Implementation Order

### Phase 1: Structure (sidebar + routing, no content changes)

Rearrange the sidebar. Create the "Under the Hood" section. Move
existing pages to their new locations. Set up redirects so old URLs
don't break. This is a pure reorganization — the content stays
identical, just filed differently.

1. Create the new sidebar structure in `astro.config.mjs`
2. Move/rename files to match new paths
3. Add redirects for any changed URLs
4. Verify all internal links still work

### Phase 2: Tier 1 cuts (user-facing pages)

Go through each Tier 1 page and cut implementation details. This is
the subtraction pass — make the user docs shorter, not longer.

Pages to edit: Lifecycle, Running Commands, Files, Preview URLs,
Templates, Images, Users & Auth.

### Phase 3: Tier 2 narrative (Under the Hood pages)

Write/rewrite the "Under the Hood" pages with narrative, context, and
personality. This is where PLAN-learning.md's rewrite work feeds in.

Pages: Architecture Overview, Lohar, Thermal States, Networking,
Design Decisions (already good — expand with new entries).

### Phase 4: Tier 3 gaps (Reference)

Fill in Configuration page. Split Wire Protocol into narrative +
frame spec. Verify CLI and API reference are complete.

### Phase 5: New pages

Write: Updating & Uninstalling, Development Setup, Adding a Tier.

---

## What's Not in This Plan

**Blog.** The best "Under the Hood" pages could be blog posts too
(cross-posted or linked). But bhatti.sh doesn't have a blog yet. Adding
one is a separate decision. The docs should stand alone first.

**Versioned docs.** One version, always current. Defer until there's a
reason to maintain multiple versions.

**Search.** Starlight already has Pagefind. It works. No changes needed.

**Comments / feedback.** "Was this page helpful?" buttons and inline
feedback. Nice to have, but premature for the current traffic level.
