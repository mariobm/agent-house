# Bhatti v0.6 — CLI Polish, Local Image Import, Thermal Fixes

v0.1 shipped multi-tenant security. v0.2 shipped CLI improvements (cobra,
--timing, --json). v0.3 shipped images, persistent volumes, named snapshots,
OCI support. v0.4 shipped custom kernel, rootfs tiers, 30-second install.
v0.5 shipped public preview URLs with domain mode + host-based routing.

v0.6 makes the CLI feel like a production tool instead of a developer
prototype. Three pillars:

1. **CLI discoverability** — command groups, examples, missing commands
2. **Local image import** — use Docker-pulled or locally-built images without registry auth
3. **Thermal + list fixes** — warm→cold transitions, thermal state in list output, published URLs

---

## Current State

The CLI has 18+ top-level commands in a flat list. No examples, no long
descriptions, no command grouping. The `--help` output is a wall of text
that tells you command names but not how to use them.

Private registry auth is broken. `bhatti image pull ghcr.io/org/img --auth
user:token` sends creds from CLI to server, but `authn.DefaultKeychain`
reads the server's `~/.docker/config.json`, not the client's. There's
no persistent credential store. Rather than rebuilding Docker's credential
ecosystem, we sidestep the problem entirely: let Docker handle registry
auth, import the result as a tarball.

The API has routes for stop, start, and inspect that have no CLI commands.
`bhatti user update` (quota changes) requires raw SQLite.

The thermal manager has a bug: warm sandboxes never transition to cold.
The thermal cycle tries to query the guest agent on warm (paused) VMs to
check idle time. The agent can't respond because vCPUs are frozen. This
either times out (skipping the warm→cold check forever) or the TCP
connection attempt wakes the VM back to hot. `bhatti list` also doesn't
show thermal state or published URLs.

---

## Design Principles

**Don't reimplement what Docker already does well.** Docker has credential
helpers, keychain integration, ECR plugins, GHCR token management. All of
this works. bhatti should consume Docker's output, not compete with it.

**Learn from mass-used CLIs.** Docker groups commands into Management and
Commands. kubectl groups by workflow. gh groups by domain. All of them use
`Example` strings extensively. Users learn from examples, not flag descriptions.

**Cobra features we're not using.** `cmd.Example`, `cmd.Long`,
`rootCmd.AddGroup()`, `RegisterFlagCompletionFunc`. Zero-cost improvements
that transform help output.

**Fewer, sharper commands.** Every command should earn its place. If it's
reachable via `curl` and rarely used, it doesn't need a CLI wrapper.

---

## Part 1 — Local Image Import

### 1.1 The Problem

Private image pulls require registry credentials. The current flow sends
`--auth user:token` per invocation — no persistence, no credential helpers,
no GHCR/ECR-specific handling. Building a credential management system
means reimplementing what Docker already handles (helpers, keychain,
platform-specific binaries).

Separately, locally-built images (`docker build -t my-env .`) that were
never pushed to any registry have no path into bhatti at all.

### 1.2 Solution: CLI Runs `docker save`, Streams to Server

The user has Docker on their machine (laptop, CI runner, etc.). The
bhatti server is remote. The image exists locally in Docker — the server
has never seen it.

The CLI runs `docker save` **locally** on the user's machine, then streams
the tarball to the server over HTTP. The server receives the tarball and
runs the existing conversion pipeline. The user never touches a `.tar` file.

The `go-containerregistry` library we already depend on has
`pkg/v1/tarball` which reads `docker save` tarballs and returns
`v1.Image` — the same interface our existing layer extraction pipeline
consumes.

New code path: CLI `docker save <ref>` (local) → HTTP stream to server →
server `tarball.ImageFromPath()` → `extractLayer()` (existing) →
`injectLohar()` (existing) → `createExt4FromDir()` (existing).

The flow:

```bash
# Private registry image (Docker on user's machine handles auth)
docker pull ghcr.io/org/private:latest
bhatti image import ghcr.io/org/private:latest
# → imported "private-latest" (420MB)

# Locally built image (never pushed to any registry)
docker build -t my-env .
bhatti image import my-env
# → imported "my-env" (280MB)

# Custom name
bhatti image import python:3.12 --name py312

# From a tarball (--name required since there's no ref to derive from)
bhatti image import --tar /tmp/image.tar --name from-tar

# Then use it
bhatti create --name dev --image private-latest
```

The name is derived from the ref automatically (same logic as `image pull`):
- `python:3.12` → `python-3.12`
- `ghcr.io/org/private:latest` → `private-latest`
- `my-env` → `my-env`

`--name` overrides the default. Required only with `--tar`.

This covers:
- Private registries (GHCR, ECR, GCR, self-hosted) — Docker handles auth locally
- Locally built images — Docker already has them on the user's machine
- Images from other tools (podman, nerdctl) — they all produce compatible tarballs
- Air-gapped environments — copy tarball via scp, import with `--tar`
- CI pipelines — `docker build` + `bhatti image import` in the same job

### 1.3 `bhatti image import` Command (CLI)

The CLI runs `docker save` **locally** on the user's machine, then streams
the tarball to the server. The `--tar` flag skips the Docker step and
streams a file directly.

```go
var imageImportCmd = &cobra.Command{
    Use:   "import <docker-ref>",
    Short: "Import a local Docker image as a bhatti rootfs",
    Long: `Import an image that exists in your local Docker daemon. The CLI runs
'docker save' on your machine and streams the result to the bhatti server,
which converts it to an ext4 rootfs for use with 'bhatti create --image'.

The image name is derived from the ref automatically (same as 'image pull').
Use --name to override. Docker must be installed locally.

For private registries, pull with Docker first (which handles auth):
  docker pull ghcr.io/org/private:latest
  bhatti image import ghcr.io/org/private:latest

For raw tarballs (no Docker needed), use --tar:
  bhatti image import --tar /path/to/image.tar --name my-image`,
    Example: `  # Import from local Docker (name derived from ref)
  bhatti image import python:3.12

  # Private image (pull with Docker first, it handles auth)
  docker pull ghcr.io/org/private:latest
  bhatti image import ghcr.io/org/private:latest

  # Locally built image
  docker build -t my-env .
  bhatti image import my-env

  # Custom name
  bhatti image import python:3.12 --name py312

  # From a raw tarball (--name required)
  bhatti image import --tar /tmp/image.tar --name from-tar`,
    Args: cobra.MaximumNArgs(1),
    RunE: func(cmd *cobra.Command, args []string) error {
        setupTiming(cmd)
        defer printTiming()

        name, _ := cmd.Flags().GetString("name")
        tarPath, _ := cmd.Flags().GetString("tar")

        var body io.Reader

        if tarPath != "" {
            // Tarball mode: --name is required (no ref to derive from)
            if name == "" {
                return fmt.Errorf("--name is required when using --tar")
            }
            f, err := os.Open(tarPath)
            if err != nil {
                return fmt.Errorf("open tarball: %w", err)
            }
            defer f.Close()
            body = f
            fmt.Printf("importing %q from tarball...\n", name)
        } else {
            // Docker mode: run docker save locally, stream stdout
            if len(args) == 0 {
                return fmt.Errorf("docker image ref required (or use --tar)")
            }
            ref := args[0]

            // Derive name from ref if not provided
            if name == "" {
                name = deriveImageName(ref)
            }

            // Check Docker is available on user's machine
            if _, err := exec.LookPath("docker"); err != nil {
                return fmt.Errorf("docker not found \u2014 install Docker or use --tar with a tarball")
            }

            // Verify image exists locally before starting the upload
            check := exec.Command("docker", "image", "inspect", ref)
            check.Stdout = nil
            check.Stderr = nil
            if err := check.Run(); err != nil {
                return fmt.Errorf("image %q not found in Docker \u2014 run 'docker pull %s' first", ref, ref)
            }

            // Pipe docker save stdout directly to the HTTP request body.
            // No temp file \u2014 streams from Docker to the server.
            save := exec.Command("docker", "save", ref)
            stdout, err := save.StdoutPipe()
            if err != nil {
                return fmt.Errorf("docker save pipe: %w", err)
            }
            if err := save.Start(); err != nil {
                return fmt.Errorf("docker save start: %w", err)
            }
            defer save.Wait()
            body = stdout
            fmt.Printf("importing %q from local Docker...\n", ref)
        }

        // Stream tarball to server
        req, err := http.NewRequest("POST",
            apiURL+"/images/import?name="+url.QueryEscape(name),
            body)
        if err != nil {
            return err
        }
        req.Header.Set("Content-Type", "application/x-tar")
        if apiToken != "" {
            req.Header.Set("Authorization", "Bearer "+apiToken)
        }

        resp, err := httpClient().Do(req)
        if err != nil {
            return err
        }
        defer resp.Body.Close()

        if resp.StatusCode >= 400 {
            var errBody struct{ Error string `json:"error"` }
            json.NewDecoder(resp.Body).Decode(&errBody)
            return fmt.Errorf("%s: %s", resp.Status, errBody.Error)
        }

        var result struct {
            Name   string `json:"name"`
            SizeMB int    `json:"size_mb"`
        }
        json.NewDecoder(resp.Body).Decode(&result)

        if isJSON(cmd) {
            outputJSON(result)
        } else {
            fmt.Printf("imported %q (%dMB)\n", result.Name, result.SizeMB)
        }
        return nil
    },
}

func init() {
    imageImportCmd.Flags().String("name", "", "Image name (default: derived from ref)")
    imageImportCmd.Flags().String("tar", "", "Import from tarball path instead of Docker")
    imageCmd.AddCommand(imageImportCmd)
}

// deriveImageName extracts a short image name from a Docker ref.
// Same logic as imagePullCmd — extract from existing code into a shared helper.
//   "python:3.12"                        → "python-3.12"
//   "ghcr.io/org/private:latest"         → "private-latest"
//   "my-env"                             → "my-env"
//   "docker.io/library/python:3.12"      → "python-3.12"
func deriveImageName(ref string) string {
    // Take the last path component (strips registry + org)
    parts := strings.Split(ref, "/")
    base := parts[len(parts)-1]
    // Replace : with - for tag
    return strings.ReplaceAll(base, ":", "-")
}
```

The Docker mode pipes `docker save` stdout directly into the HTTP request
body — no temp file on the client side. The tarball streams from the
local Docker daemon through the CLI to the remote server in a single pass.

The `deriveImageName` helper should be extracted from the existing
`imagePullCmd` logic so both commands share the same derivation. The
current pull code is inline and slightly convoluted — this is a good
opportunity to clean it up.

### 1.4 Server-Side: `POST /images/import`

New route in `routes.go`. The server always receives a tarball — the CLI
handles the Docker interaction. One code path, simple.

```go
func (s *Server) handleImageImport(w http.ResponseWriter, r *http.Request, user *store.User) {
    if r.Method != http.MethodPost {
        errResp(w, 405, "method not allowed")
        return
    }

    name := r.URL.Query().Get("name")
    if name == "" || !isValidName(name) {
        errResp(w, 400, "valid name required")
        return
    }

    // Check if image already exists
    if _, err := s.store.GetImage(user.ID, name); err == nil {
        errResp(w, 409, fmt.Sprintf("image %q already exists \u2014 delete first", name))
        return
    }

    // Write streamed tarball to temp file (don't hold multi-GB in memory)
    tmpFile, err := os.CreateTemp("", "bhatti-import-*.tar")
    if err != nil {
        errRespInternal(w, r, "create temp file", err)
        return
    }
    defer os.Remove(tmpFile.Name())

    limited := io.LimitReader(r.Body, 10<<30) // 10GB cap
    if _, err := io.Copy(tmpFile, limited); err != nil {
        tmpFile.Close()
        errRespInternal(w, r, "receive tarball", err)
        return
    }
    tmpFile.Close()

    // Convert tarball \u2192 ext4
    loharPath := filepath.Join(s.dataDir, "lohar")
    outputDir := filepath.Join(s.dataDir, "images", user.ID)
    os.MkdirAll(outputDir, 0700)
    outputPath := filepath.Join(outputDir, name+".ext4")

    config, err := oci.ImportFromTarball(
        r.Context(), tmpFile.Name(), outputPath, loharPath)
    if err != nil {
        os.Remove(outputPath)
        errResp(w, 400, "import failed: "+err.Error())
        return
    }

    configJSON, _ := json.Marshal(config)
    sizeMB := int(config.TotalSize / 1024 / 1024)

    img := store.ImageRecord{
        ID: genID(), UserID: user.ID, Name: name,
        Source:        "import:" + name,
        FilePath:      outputPath,
        SizeMB:        sizeMB,
        OCIConfigJSON: string(configJSON),
        CreatedAt:     time.Now(),
    }
    if err := s.store.CreateImage(img); err != nil {
        os.Remove(outputPath)
        errRespInternal(w, r, "store image", err)
        return
    }

    slog.Info("image.imported", "name", name, "user", user.Name, "size_mb", sizeMB)
    writeJSON(w, 201, map[string]any{
        "name": name, "size_mb": sizeMB,
    })
}
```

Route registration (in `handleImage`, alongside the existing `pull` sub-route):

```go
// In handleImage:
if path == "import" {
    s.handleImageImport(w, r, user)
    return
}
```

The server handler is deliberately simple — one code path, always receives
a tarball stream. All the Docker interaction (checking if the image exists,
running `docker save`, error messaging) happens on the CLI side where the
user has Docker installed.

### 1.5 `oci.ImportFromTarball`

New function in `pkg/oci/oci.go`. Reads a `docker save` tarball using
`go-containerregistry/pkg/v1/tarball`, then runs the same pipeline as
`PullAndConvert` (extract layers → inject lohar → create ext4).

```go
// ImportFromTarball converts a Docker save tarball to an ext4 rootfs.
// The tarball can be produced by 'docker save <ref> -o <file>'.
func ImportFromTarball(ctx context.Context, tarballPath, outputPath, loharPath string) (*Config, error) {
    // Open the tarball as an OCI image.
    // docker save produces a tarball with a manifest.json; the tarball
    // package handles both single-image and multi-image tarballs.
    img, err := tarball.ImageFromPath(tarballPath, nil)
    if err != nil {
        return nil, fmt.Errorf("read tarball: %w", err)
    }

    // Extract OCI config
    cfgFile, err := img.ConfigFile()
    if err != nil {
        return nil, fmt.Errorf("config: %w", err)
    }
    config := extractConfig(cfgFile)

    // Flatten layers to temp directory
    tmpDir, err := os.MkdirTemp("", "bhatti-import-*")
    if err != nil {
        return nil, err
    }
    defer os.RemoveAll(tmpDir)

    layers, err := img.Layers()
    if err != nil {
        return nil, fmt.Errorf("layers: %w", err)
    }

    for i, layer := range layers {
        if ctx.Err() != nil {
            return nil, ctx.Err()
        }
        if err := extractLayer(layer, tmpDir); err != nil {
            return nil, fmt.Errorf("extract layer %d: %w", i, err)
        }
    }

    // Inject lohar agent
    if err := injectLohar(tmpDir, loharPath); err != nil {
        return nil, fmt.Errorf("inject lohar: %w", err)
    }

    // Validate
    if warnings := validateImage(tmpDir); len(warnings) > 0 {
        for _, w := range warnings {
            slog.Warn("import image warning", "issue", w)
        }
    }

    // Create ext4 image
    if err := createExt4FromDir(tmpDir, outputPath); err != nil {
        return nil, fmt.Errorf("create ext4: %w", err)
    }

    if fi, err := os.Stat(outputPath); err == nil {
        config.TotalSize = fi.Size()
    }

    return config, nil
}
```

This is ~40 lines of new code. The rest is reuse.

### 1.6 Why Not Use `go-containerregistry/pkg/v1/daemon`?

The library has a `daemon` package that talks to the Docker API directly
(no shell-out). But it imports `github.com/moby/moby` — a heavy
dependency (~15 packages) for something `docker save` does in one
process. The shell-out approach:
- Zero new Go dependencies
- Works with Docker, podman, nerdctl (anything with a `save` command)
- Easy to debug (`docker save` failures are self-explanatory)
- The tarball format is the same regardless of container runtime

### 1.7 What This Replaces

The entire `bhatti login` / `bhatti logout` / credential store design from
the original plan is removed. No `~/.bhatti/credentials.yaml`, no
`extractRegistry()`, no credential resolution chain, no per-registry
management.

The existing `bhatti image pull` continues to work for **public** registry
images (Docker Hub public, etc.). For private images, the path is:
`docker pull` on the user's machine (Docker handles auth) →
`bhatti image import <ref>` (CLI runs `docker save` locally, streams to
server).

### 1.8 `image pull` Auth Errors Guide to `import`

When the server-side pull fails with an authentication error, the CLI
should detect this and suggest the import path. This is the UX bridge
that makes two commands feel like one workflow.

In the existing `imagePullCmd`, after a task fails:

```go
// In the poll loop, when task.Status == "failed":
if strings.Contains(task.Error, "unauthorized") ||
    strings.Contains(task.Error, "UNAUTHORIZED") ||
    strings.Contains(task.Error, "403") {
    fmt.Fprintf(os.Stderr, "This image may require authentication.\n")
    fmt.Fprintf(os.Stderr, "Pull it with Docker locally, then import:\n")
    fmt.Fprintf(os.Stderr, "  docker pull %s\n", ref)
    fmt.Fprintf(os.Stderr, "  bhatti image import %s\n", ref)
    return fmt.Errorf("pull failed: authentication required")
}
return fmt.Errorf("pull failed: %s", task.Error)
```

This means a user who tries `bhatti image pull ghcr.io/org/private:latest`
gets a clear next step instead of an opaque "unauthorized" error.

### 1.9 Image Sharing

Images are user-scoped (`user_id` in the `images` table). A user's image
is invisible to other users. The only exception is admin images
(`user_id = ''`), which are visible to everyone.

For org use cases (share a golden image with teammates but not external
API key holders), we need a share list — not full ACLs, just "this image
is also visible to these user IDs."

**New table:**

```sql
CREATE TABLE IF NOT EXISTS image_shares (
    image_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    PRIMARY KEY (image_id, user_id)
);
```

**Updated `GetImage` resolution order:**

```go
func (s *Store) GetImage(userID, name string) (*ImageRecord, error) {
    // 1. User's own image
    // ... existing user_id = ? AND name = ? query ...

    // 2. Image shared with this user
    err = s.db.QueryRow(
        `SELECT i.id, i.user_id, i.name, i.source, i.file_path,
                i.size_mb, i.oci_digest, i.oci_config_json, i.created_at
         FROM images i
         JOIN image_shares s ON s.image_id = i.id
         WHERE s.user_id = ? AND i.name = ?`, userID, name).Scan(...)
    if err == nil {
        return &img, nil
    }

    // 3. Global admin image (user_id = '')
    // ... existing fallback query ...
}
```

**Updated `ListImages`:**

```go
func (s *Store) ListImages(userID string) ([]ImageRecord, error) {
    rows, err := s.db.Query(
        `SELECT id, user_id, name, source, file_path, size_mb,
                oci_digest, oci_config_json, created_at
         FROM images
         WHERE user_id = ?
            OR user_id = ''
            OR id IN (SELECT image_id FROM image_shares WHERE user_id = ?)
         ORDER BY created_at DESC`, userID, userID)
    // ...
}
```

**CLI command** (admin, operates on local DB like `user` commands):

```bash
sudo bhatti image share spc-golden --user kowshik --user sumo
# Shared "spc-golden" with: kowshik, sumo

sudo bhatti image unshare spc-golden --user sumo
# Unshared "spc-golden" from: sumo

sudo bhatti image share spc-golden --list
# spc-golden shared with: kowshik, sumo
```

```go
var imageShareCmd = &cobra.Command{
    Use:   "share <image-name>",
    Short: "Share an image with other users",
    Example: `  sudo bhatti image share spc-golden --user kowshik --user sumo
  sudo bhatti image share spc-golden --list`,
    Args: cobra.ExactArgs(1),
    RunE: func(cmd *cobra.Command, args []string) error {
        st := openLocalStore()
        defer st.Close()

        imageName := args[0]
        listShares, _ := cmd.Flags().GetBool("list")
        users, _ := cmd.Flags().GetStringSlice("user")

        // Find the image (any owner)
        img, err := st.GetImageByName(imageName)
        if err != nil {
            return fmt.Errorf("image %q not found", imageName)
        }

        if listShares {
            shares, _ := st.ListImageShares(img.ID)
            if len(shares) == 0 {
                fmt.Printf("%s: not shared\n", imageName)
            } else {
                fmt.Printf("%s shared with: %s\n", imageName,
                    strings.Join(shares, ", "))
            }
            return nil
        }

        if len(users) == 0 {
            return fmt.Errorf("--user required")
        }

        for _, userName := range users {
            user, err := st.GetUserByName(userName)
            if err != nil {
                return fmt.Errorf("user %q not found", userName)
            }
            st.ShareImage(img.ID, user.ID)
        }
        fmt.Printf("shared %q with: %s\n", imageName,
            strings.Join(users, ", "))
        return nil
    },
}
```

**Store methods needed:**

```go
func (s *Store) ShareImage(imageID, userID string) error
func (s *Store) UnshareImage(imageID, userID string) error
func (s *Store) ListImageShares(imageID string) ([]string, error) // returns user names
func (s *Store) GetImageByName(name string) (*ImageRecord, error) // any owner
func (s *Store) GetUserByName(name string) (*User, error)
```

### 1.10 Tests

- `TestImportFromTarball` — create a tarball with known layers (reuse
  the test helpers in `oci_test.go` that build tar layers), import,
  verify ext4 output exists and contains expected files + lohar binary
- `TestImportPreservesConfig` — verify OCI config (env, workdir, cmd)
  is extracted from tarball correctly
- `TestImportMultiLayerWhiteouts` — tarball with whiteout entries,
  verify correct layer flattening
- `TestImportEndpoint` — HTTP test: POST tarball to `/images/import`,
  verify image appears in store
- `TestImportDuplicateName` — import with existing name → 409
- `TestPullAuthErrorSuggestsImport` — verify error message includes
  `docker pull` + `bhatti image import` guidance
- `TestImageShareVisibility` — share image with user B, verify
  user B can GetImage and ListImages sees it
- `TestImageShareIsolation` — share with user B, verify user C
  cannot see it
- `TestImageUnshare` — unshare, verify user loses access

---

## Part 2 — CLI Discoverability

### 2.1 Command Groups

Three groups instead of six. Keep it tight:

```
Core:
  create      Create a new sandbox
  list        List sandboxes
  destroy     Destroy a sandbox
  exec        Run a command in a sandbox
  shell       Open an interactive shell
  stop        Snapshot and stop a sandbox
  start       Resume a stopped sandbox

Resources:
  image       Manage rootfs images
  volume      Manage persistent volumes
  secret      Manage encrypted secrets
  snapshot    Manage named VM snapshots
  publish     Publish a sandbox port with a public URL
  unpublish   Unpublish a sandbox port

Setup & Admin:
  setup       Configure CLI endpoint and API key
  user        Manage users (requires DB access)
  serve       Start the bhatti daemon
  update      Update bhatti CLI to the latest version

Additional Commands:
  inspect     Show sandbox details
  ps          List sessions in a sandbox
  file        Read, write, and list files in a sandbox
  version     Print version and API endpoint
  completion  Generate shell completion script
```

Why three:
- **Core** is the stuff you use every session. Create → work → destroy.
  Stop/start earn their place here because they're the primary lifecycle
  verbs alongside create/destroy.
- **Resources** is everything that persists across sandboxes.
  Publish/unpublish are here because they're about managing a resource
  (the URL mapping), not about connecting to a sandbox.
- **Setup & Admin** is one-time or rare operations. Server operators only.
- **Additional** (Cobra's default group for commands without a GroupID)
  catches inspect, ps, file — useful but not primary workflow.

**Implementation:**

```go
func init() {
    rootCmd.AddGroup(
        &cobra.Group{ID: "core",     Title: "Core:"},
        &cobra.Group{ID: "resource", Title: "Resources:"},
        &cobra.Group{ID: "admin",    Title: "Setup & Admin:"},
    )

    createCmd.GroupID = "core"
    listCmd.GroupID = "core"
    destroyCmd.GroupID = "core"
    execCmd.GroupID = "core"
    shellCmd.GroupID = "core"
    stopCmd.GroupID = "core"       // Part 3
    startCmd.GroupID = "core"      // Part 3

    imageCmd.GroupID = "resource"
    volumeCmd.GroupID = "resource"
    secretCmd.GroupID = "resource"
    snapshotCmd.GroupID = "resource"
    publishCmd.GroupID = "resource"
    unpublishCmd.GroupID = "resource"

    setupCmd.GroupID = "admin"
    userCmd.GroupID = "admin"
    updateCmd.GroupID = "admin"
    // serveCmd is added in main.go — set GroupID there

    // inspect, ps, file, version, completion get no GroupID →
    // fall into "Additional Commands"
}
```

### 2.2 Example Strings

Every command gets an `Example` field — top-level and leaf subcommands.
This is the single highest-impact change for discoverability.

```go
var createCmd = &cobra.Command{
    Use:   "create [flags]",
    Short: "Create a new sandbox",
    Long: `Create a new sandbox VM. Each sandbox is an isolated Linux environment
with its own kernel, filesystem, and network.`,
    Example: `  # Basic sandbox
  bhatti create --name dev

  # Custom resources
  bhatti create --name ml --cpus 4 --memory 4096

  # With environment variables and init script
  bhatti create --name api --env API_KEY=sk-abc --init "npm install"

  # From a custom image
  bhatti create --name py --image python-3.12

  # From a template
  bhatti create --name exp --template ml-env

  # With a persistent volume
  bhatti create --name work --volume workspace:/workspace`,
}

var execCmd = &cobra.Command{
    Use:   "exec <sandbox> [--] <command...>",
    Short: "Run a command in a sandbox",
    Long: `Execute a command inside a sandbox. The exit code is forwarded.
Sleeping sandboxes wake automatically.`,
    Example: `  bhatti exec dev -- echo hello
  bhatti exec dev echo hello           # -- is optional
  bhatti exec dev -- sudo apt-get install -y ripgrep
  bhatti exec dev --timeout 60 -- long-running-script.sh`,
}

var shellCmd = &cobra.Command{
    Use:   "shell <sandbox>",
    Short: "Open an interactive shell",
    Long: `Open an interactive terminal inside the sandbox. Ctrl+\ to detach —
the shell keeps running. Reconnect with 'bhatti shell' again.`,
    Example: `  bhatti shell dev
  bhatti sh dev        # alias`,
}

var destroyCmd = &cobra.Command{
    Use:   "destroy <sandbox>",
    Short: "Destroy a sandbox",
    Long:  `Permanently destroy a sandbox and all its data. This cannot be undone.
Persistent volumes are detached but not deleted.`,
    Example: `  bhatti destroy dev
  bhatti rm dev        # alias`,
}

var listCmd = &cobra.Command{
    Use:   "list",
    Short: "List sandboxes",
    Example: `  bhatti list
  bhatti ls            # alias
  bhatti ls --json`,
}

var imageCmd = &cobra.Command{
    Use:   "image <list|pull|import|save|delete>",
    Short: "Manage rootfs images",
    Long: `Images are ext4 filesystem snapshots used as sandbox root filesystems.
Pull public images from registries, or import private/local images
from your local Docker daemon.`,
    Example: `  # Pull a public image (server pulls from registry)
  bhatti image pull python:3.12

  # Import from your local Docker (private or locally built)
  docker pull ghcr.io/org/private:latest
  bhatti image import ghcr.io/org/private:latest

  # Import a local build
  docker build -t my-env .
  bhatti image import my-env

  # Save a running sandbox as an image
  bhatti image save dev --name my-custom-env

  # List images
  bhatti image list`,
}

var imagePullCmd = &cobra.Command{
    Use:   "pull <ref>",
    Short: "Pull an OCI/Docker image from a public registry",
    Long: `Pull a public image from any OCI-compatible registry. The server pulls
the image and converts it to an ext4 rootfs for 'bhatti create --image'.

For private registries, use 'bhatti image import' instead:
  docker pull ghcr.io/org/private:latest
  bhatti image import ghcr.io/org/private:latest`,
    Example: `  bhatti image pull python:3.12
  bhatti image pull ubuntu:24.04 --name ubuntu
  bhatti image pull node:22-slim --name node-22`,
}

var volumeCmd = &cobra.Command{
    Use:   "volume <create|list|delete|resize>",
    Short: "Manage persistent volumes",
    Long: `Persistent volumes are ext4 filesystems that survive sandbox destruction.
Attach them with '--volume name:/mount' on create.`,
    Example: `  bhatti volume create --name workspace --size 5120
  bhatti create --name dev --volume workspace:/workspace
  bhatti volume resize workspace --size 10240
  bhatti volume list`,
}

var volumeCreateCmd = &cobra.Command{
    Use:   "create",
    Short: "Create a persistent volume",
    Example: `  bhatti volume create --name workspace --size 5120
  bhatti volume create --name data --size 20480`,
}

var snapshotCmd = &cobra.Command{
    Use:   "snapshot <create|list|resume|delete>",
    Short: "Manage named VM snapshots",
    Long: `Snapshots capture the entire VM state: memory, CPU, disk. Resume
produces an exact continuation — processes running, files open.`,
    Example: `  bhatti snapshot create dev --name dev-ready
  bhatti snapshot resume dev-ready --name dev-2
  bhatti snapshot list`,
}

var publishCmd = &cobra.Command{
    Use:   "publish <sandbox> -p <port>",
    Short: "Publish a sandbox port with a public URL",
    Example: `  bhatti publish dev -p 3000
  bhatti publish dev -p 3000 -a my-app`,
}

var secretCmd = &cobra.Command{
    Use:   "secret <set|list|delete>",
    Short: "Manage encrypted secrets",
    Example: `  bhatti secret set API_KEY sk-abc123
  bhatti secret list
  bhatti secret delete API_KEY`,
}

var fileCmd = &cobra.Command{
    Use:   "file <read|write|ls>",
    Short: "Read, write, and list files in a sandbox",
    Example: `  bhatti file read dev /workspace/app.js
  echo 'hello' | bhatti file write dev /workspace/greeting.txt
  bhatti file ls dev /workspace/`,
}

var inspectCmd = &cobra.Command{
    Use:   "inspect <sandbox>",
    Short: "Show sandbox details",
    Example: `  bhatti inspect dev
  bhatti inspect dev --json`,
}

var setupCmd = &cobra.Command{
    Use:   "setup",
    Short: "Configure CLI endpoint and API key",
    Long: `Interactive setup for remote CLI users. Prompts for the API endpoint
and API key, saves to ~/.bhatti/config.yaml, and tests the connection.`,
    Example: `  bhatti setup`,
}

var userCmd = &cobra.Command{
    Use:   "user <create|list|delete|rotate-key|update>",
    Short: "Manage users (requires DB access)",
    Long: `User management operates directly on the local SQLite database.
Run on the server, not remotely.`,
    Example: `  sudo bhatti user create --name alice --max-sandboxes 10
  sudo bhatti user list
  sudo bhatti user rotate-key alice
  sudo bhatti user update alice --max-sandboxes 20`,
}
```

### 2.3 Root Command Enhancement

```go
var rootCmd = &cobra.Command{
    Use:   "bhatti",
    Short: "Firecracker microVM orchestrator",
    Long: `bhatti creates isolated Linux VMs in seconds. Each sandbox has its own
kernel, filesystem, and network. Paused sandboxes resume in under 3ms.

Quick start:
  bhatti setup                         # configure endpoint + API key
  bhatti create --name dev             # create a sandbox
  bhatti exec dev -- echo hello        # run a command
  bhatti shell dev                     # interactive shell (Ctrl+\ to detach)
  bhatti destroy dev                   # clean up`,
    SilenceUsage: true,
}
```

### 2.4 Completion Hint After Setup

```go
// At the end of setupCmd.RunE:
shell := os.Getenv("SHELL")
switch {
case strings.HasSuffix(shell, "/zsh"):
    fmt.Println("\nEnable completions:")
    fmt.Println("  echo 'source <(bhatti completion zsh)' >> ~/.zshrc")
case strings.HasSuffix(shell, "/bash"):
    fmt.Println("\nEnable completions:")
    fmt.Println("  echo 'source <(bhatti completion bash)' >> ~/.bashrc")
case strings.HasSuffix(shell, "/fish"):
    fmt.Println("\nEnable completions:")
    fmt.Println("  bhatti completion fish > ~/.config/fish/completions/bhatti.fish")
}
```

---

## Part 3 — Thermal Manager Fix + List Enrichment (Issue #3)

Three related bugs reported in [#3](https://github.com/sahil-shubham/bhatti/issues/3).

### 3.1 Bug: Warm sandboxes never transition to cold

**Root cause:** The thermal cycle runs every 10 seconds. For each warm
sandbox, it calls `te.Activity()` which opens a TCP connection to the
guest agent to check idle time. But warm = vCPUs paused. The agent can't
respond. Two failure modes:

1. **Timeout path:** `Activity()` times out after 5 seconds → `err != nil`
   → `continue` → the `thermal == "warm" && idle > cfg.ColdTimeout`
   check is never reached. Warm→cold never fires.

2. **Wake path:** On some network stacks, the TCP SYN to the paused
   guest tickles Firecracker's virtio-net enough to resume vCPUs →
   sandbox goes hot → idles back to warm → cycle repeats. The VM
   bounces between hot↔warm permanently.

The current code:

```go
// In runThermalCycle:
thermal := te.ThermalState(sb.EngineID)
if thermal == "cold" || thermal == "" {
    continue
}

// ... lastActivity fast-path check ...

// Slow path: ask the agent
activity, err := te.Activity(actCtx, sb.EngineID)
if err != nil {
    continue // ← SKIPS warm→cold check
}

// This line is never reached for warm VMs:
if thermal == "warm" && idle > cfg.ColdTimeout {
```

**Fix:** Split the thermal cycle into two code paths. Hot sandboxes query
the agent (vCPUs are running, agent can respond). Warm sandboxes use
host-side `lastActivity` timestamps only (no agent query, no VM contact).

```go
func (s *Server) runThermalCycle(te ThermalEngine, cfg ThermalConfig) {
    sandboxes, err := s.store.ListAllSandboxes()
    if err != nil {
        return
    }
    for _, sb := range sandboxes {
        if sb.Status != "running" {
            continue
        }

        thermal := te.ThermalState(sb.EngineID)

        // --- Warm → Cold: host-side timing only, no agent query ---
        if thermal == "warm" {
            ts, ok := s.lastActivity.Load(sb.EngineID)
            if !ok {
                continue // no activity record — shouldn't happen, skip
            }
            idle := time.Since(ts.(time.Time))
            if idle > cfg.ColdTimeout {
                stopCtx, stopCancel := context.WithTimeout(
                    context.Background(), 30*time.Second)
                if err := s.engine.Stop(stopCtx, sb.EngineID); err != nil {
                    stopCancel()
                    slog.Error("thermal snapshot failed",
                        "sandbox", sb.Name, "error", err)
                    s.store.UpdateSandboxStatus(sb.ID, "unknown")
                    continue
                }
                stopCancel()
                s.saveVMState(sb.ID, sb.EngineID)
                slog.Info("thermal transition", "sandbox", sb.Name,
                    "from", "warm", "to", "cold", "idle", idle.Round(time.Second))
            }
            continue
        }

        if thermal != "hot" {
            continue
        }

        // --- Hot → Warm: query agent (vCPUs running, agent can respond) ---
        if ts, ok := s.lastActivity.Load(sb.EngineID); ok {
            if time.Since(ts.(time.Time)) < cfg.WarmTimeout {
                continue
            }
        }

        actCtx, actCancel := context.WithTimeout(
            context.Background(), 5*time.Second)
        activity, err := te.Activity(actCtx, sb.EngineID)
        actCancel()
        if err != nil {
            continue
        }

        idle := time.Since(time.Unix(activity.LastActivityUnix, 0))
        if idle > cfg.WarmTimeout && activity.AttachedSessions == 0 {
            if err := te.Pause(context.Background(), sb.EngineID); err != nil {
                slog.Warn("thermal pause failed",
                    "sandbox", sb.Name, "error", err)
                continue
            }
            // Record the pause time as lastActivity so the warm→cold
            // timer starts from when we actually paused, not from
            // the last user interaction.
            s.lastActivity.Store(sb.EngineID, time.Now())
            slog.Info("thermal transition", "sandbox", sb.Name,
                "from", "hot", "to", "warm", "idle", idle.Round(time.Second))
        }
    }
}
```

Key changes:
- **Warm path comes first** — no agent query, only `lastActivity` check.
  The `lastActivity` entry is set when hot→warm fires (line marked above),
  so the cold timer starts from the moment of pause.
- **Hot path unchanged** — agent query is correct here (vCPUs running).
- **`lastActivity.Store` on pause** — ensures the warm→cold timeout
  measures from pause time, not from the last user interaction.

### 3.2 `bhatti list` shows thermal state

Enrich the `GET /sandboxes` response with thermal state from the engine.
The `/metrics` endpoint already does this pattern.

**Server change** in `handleSandboxes` GET:

```go
case http.MethodGet:
    list, err := s.store.ListSandboxes(user.ID)
    if err != nil {
        errRespInternal(w, r, "list sandboxes failed", err)
        return
    }
    if list == nil {
        list = []store.Sandbox{}
    }

    // Enrich with thermal state (read-only, no VM interaction)
    type enrichedSandbox struct {
        store.Sandbox
        Thermal string `json:"thermal,omitempty"`
    }
    te, hasThermal := s.engine.(ThermalEngine)
    enriched := make([]enrichedSandbox, len(list))
    for i, sb := range list {
        enriched[i] = enrichedSandbox{Sandbox: sb}
        if hasThermal && sb.Status == "running" {
            enriched[i].Thermal = te.ThermalState(sb.EngineID)
        }
    }
    writeJSON(w, 200, enriched)
```

`ThermalState()` reads an in-memory string under a lock — no VM
interaction, no Firecracker API calls, no wake risk. Safe to call
on every list request.

**CLI change** in `listCmd`:

```go
var sandboxes []struct {
    ID      string `json:"id"`
    Name    string `json:"name"`
    Status  string `json:"status"`
    Thermal string `json:"thermal"`
    IP      string `json:"ip"`
}
// ... fetch ...

fmt.Printf("%-20s %-20s %-10s %-8s %-16s\n",
    "ID", "NAME", "STATUS", "THERMAL", "IP")
for _, sb := range sandboxes {
    thermal := sb.Thermal
    if thermal == "" {
        thermal = "-"
    }
    fmt.Printf("%-20s %-20s %-10s %-8s %-16s\n",
        sb.ID, sb.Name, sb.Status, thermal, sb.IP)
}
```

### 3.3 `bhatti list` shows published URLs

Add a store method to batch-fetch publish rules for a user, then include
them in the list response.

**New store method:**

```go
// ListUserPublishRules returns all publish rules for sandboxes owned by a user.
func (s *Store) ListUserPublishRules(userID string) ([]PublishRule, error) {
    rows, err := s.db.Query(
        `SELECT id, sandbox_id, user_id, port, alias, created_at
         FROM publish_rules WHERE user_id = ?`, userID)
    if err != nil {
        return nil, err
    }
    defer rows.Close()
    var rules []PublishRule
    for rows.Next() {
        var r PublishRule
        if err := rows.Scan(&r.ID, &r.SandboxID, &r.UserID,
            &r.Port, &r.Alias, &r.CreatedAt); err != nil {
            return nil, err
        }
        rules = append(rules, r)
    }
    return rules, rows.Err()
}
```

**Server change** — enrich list response with publish URLs:

```go
// After thermal enrichment, add published URLs
rules, _ := s.store.ListUserPublishRules(user.ID)
rulesByID := make(map[string][]string) // sandbox_id → []urls
for _, r := range rules {
    url := publishedURL(r.Alias, s.proxyZone, s.publicProxyAddr)
    rulesByID[r.SandboxID] = append(rulesByID[r.SandboxID], url)
}

type enrichedSandbox struct {
    store.Sandbox
    Thermal string   `json:"thermal,omitempty"`
    URLs    []string `json:"urls,omitempty"`
}
// ... (set URLs field from rulesByID[sb.ID])
```

**CLI change** — show URL column if any sandbox has published URLs:

```go
// Only show URL column if at least one sandbox has URLs
hasURLs := false
for _, sb := range sandboxes {
    if len(sb.URLs) > 0 {
        hasURLs = true
        break
    }
}
if hasURLs {
    fmt.Printf("%-20s %-20s %-10s %-8s %-16s %s\n",
        "ID", "NAME", "STATUS", "THERMAL", "IP", "URL")
} else {
    fmt.Printf("%-20s %-20s %-10s %-8s %-16s\n",
        "ID", "NAME", "STATUS", "THERMAL", "IP")
}
for _, sb := range sandboxes {
    url := ""
    if len(sb.URLs) > 0 {
        url = sb.URLs[0] // show first; --json shows all
    }
    // ...
}
```

### 3.4 Tests

- `TestThermalWarmToCold` — pause a VM (warm), verify cold transition
  fires after ColdTimeout using host-side lastActivity, without
  querying the agent
- `TestThermalHotToWarm` — verify hot→warm still uses agent Activity()
  correctly
- `TestThermalPauseSetsLastActivity` — verify lastActivity is updated
  when hot→warm fires, so cold timer starts from pause time
- `TestListEnrichedThermal` — GET /sandboxes returns `thermal` field
- `TestListEnrichedURLs` — publish a port, verify GET /sandboxes
  returns `urls` field
- `TestListUserPublishRules` — new store method returns rules
  across multiple sandboxes

---

## Part 4 — Missing CLI Commands

Commands where the API route already exists but there's no CLI.
Zero server changes.

### 4.1 `bhatti stop`

```go
var stopCmd = &cobra.Command{
    Use:   "stop <sandbox>",
    Short: "Snapshot and stop a sandbox",
    Long: `Pause the sandbox and save a snapshot to disk. Resume later with
'bhatti start'. Stopped sandboxes use zero CPU and memory.`,
    Example: `  bhatti stop dev
  bhatti start dev     # resume later`,
    Args:              cobra.ExactArgs(1),
    ValidArgsFunction: completeSandboxNames,
    RunE: func(cmd *cobra.Command, args []string) error {
        setupTiming(cmd)
        defer printTiming()
        id, err := resolveID(args[0])
        if err != nil { return err }
        var sb map[string]any
        if err := apiJSON("POST", "/sandboxes/"+id+"/stop", nil, &sb); err != nil {
            return err
        }
        if isJSON(cmd) {
            outputJSON(sb)
        } else {
            fmt.Println("stopped")
        }
        return nil
    },
}
```

### 4.2 `bhatti start`

```go
var startCmd = &cobra.Command{
    Use:   "start <sandbox>",
    Short: "Resume a stopped sandbox",
    Long: `Resume a sandbox from its snapshot. Continues exactly where it left off.`,
    Example: `  bhatti start dev`,
    Args:              cobra.ExactArgs(1),
    ValidArgsFunction: completeSandboxNames,
    RunE: func(cmd *cobra.Command, args []string) error {
        setupTiming(cmd)
        defer printTiming()
        id, err := resolveID(args[0])
        if err != nil { return err }
        var sb map[string]any
        if err := apiJSON("POST", "/sandboxes/"+id+"/start", nil, &sb); err != nil {
            return err
        }
        if isJSON(cmd) {
            outputJSON(sb)
        } else {
            fmt.Printf("started (%s)\n", sb["status"])
        }
        return nil
    },
}
```

### 4.3 `bhatti inspect`

```go
var inspectCmd = &cobra.Command{
    Use:     "inspect <sandbox>",
    Short:   "Show sandbox details",
    Aliases: []string{"info"},
    Example: `  bhatti inspect dev
  bhatti inspect dev --json`,
    Args:              cobra.ExactArgs(1),
    ValidArgsFunction: completeSandboxNames,
    RunE: func(cmd *cobra.Command, args []string) error {
        setupTiming(cmd)
        defer printTiming()
        id, err := resolveID(args[0])
        if err != nil { return err }
        var sb map[string]any
        if err := apiJSON("GET", "/sandboxes/"+id, nil, &sb); err != nil {
            return err
        }
        if isJSON(cmd) {
            outputJSON(sb)
            return nil
        }
        fmt.Printf("Name:       %s\n", sb["name"])
        fmt.Printf("ID:         %s\n", sb["id"])
        fmt.Printf("Status:     %s\n", sb["status"])
        fmt.Printf("IP:         %s\n", sb["ip"])
        fmt.Printf("Created:    %s\n", sb["created_at"])
        if t, ok := sb["template_id"]; ok && t != nil && t != "" {
            fmt.Printf("Template:   %s\n", t)
        }
        if stopped, ok := sb["stopped_at"]; ok && stopped != nil {
            fmt.Printf("Stopped:    %s\n", stopped)
        }
        return nil
    },
}
```

### 4.4 `--template` Flag on Create

The server supports `template_id` in the create request but the CLI
has no flag for it.

```go
// In createCmd init:
createCmd.Flags().String("template", "", "Template name or ID")

// In createCmd.RunE, before the API call:
tmpl, _ := cmd.Flags().GetString("template")
if tmpl != "" {
    req["template_id"] = tmpl
}
```

### 4.5 Registration

```go
func init() {
    rootCmd.AddCommand(stopCmd)
    rootCmd.AddCommand(startCmd)
    rootCmd.AddCommand(inspectCmd)
}
```

---

## Part 5 — User Update

The only admin operation that saves real time over raw SQLite. Changing
user quotas is the most common admin task (someone needs more sandboxes,
more memory, etc.) and currently requires:

```bash
sqlite3 /var/lib/bhatti/state.db "UPDATE users SET max_sandboxes = 20 WHERE name = 'alice'"
```

### 5.1 Store Method

```go
func (s *Store) UpdateUser(name string, updates map[string]any) error {
    // Allowlist of updatable columns
    allowed := map[string]bool{
        "max_sandboxes":           true,
        "max_cpus_per_sandbox":    true,
        "max_memory_mb_per_sandbox": true,
        "max_volume_storage_mb":   true,
        "max_images":              true,
        "max_snapshots":           true,
    }

    if len(updates) == 0 {
        return fmt.Errorf("nothing to update")
    }

    var setClauses []string
    var args []any
    for col, val := range updates {
        if !allowed[col] {
            return fmt.Errorf("cannot update column %q", col)
        }
        setClauses = append(setClauses, col+" = ?")
        args = append(args, val)
    }
    args = append(args, name)
    query := "UPDATE users SET " + strings.Join(setClauses, ", ") + " WHERE name = ?"
    res, err := s.db.Exec(query, args...)
    if err != nil {
        return err
    }
    n, _ := res.RowsAffected()
    if n == 0 {
        return fmt.Errorf("user %q not found", name)
    }
    return nil
}
```

Note: lookup by name directly (not ID) since admin always refers to users
by name. Single query, no intermediate step.

### 5.2 CLI Command

```go
var userUpdateCmd = &cobra.Command{
    Use:   "update <name>",
    Short: "Update user quotas",
    Long:  `Update resource limits for a user. Only specified flags are changed.`,
    Example: `  sudo bhatti user update alice --max-sandboxes 20
  sudo bhatti user update alice --max-cpus 8 --max-memory 8192`,
    Args: cobra.ExactArgs(1),
    RunE: func(cmd *cobra.Command, args []string) error {
        st := openLocalStore()
        defer st.Close()

        updates := make(map[string]any)
        flagToCol := map[string]string{
            "max-sandboxes":      "max_sandboxes",
            "max-cpus":           "max_cpus_per_sandbox",
            "max-memory":         "max_memory_mb_per_sandbox",
            "max-volume-storage": "max_volume_storage_mb",
            "max-images":         "max_images",
            "max-snapshots":      "max_snapshots",
        }
        for flag, col := range flagToCol {
            if cmd.Flags().Changed(flag) {
                v, _ := cmd.Flags().GetInt(flag)
                updates[col] = v
            }
        }

        if len(updates) == 0 {
            return fmt.Errorf("no flags specified — nothing to update")
        }

        if err := st.UpdateUser(args[0], updates); err != nil {
            return err
        }
        fmt.Printf("updated %q\n", args[0])
        for flag, col := range flagToCol {
            if v, ok := updates[col]; ok {
                fmt.Printf("  %s = %v\n", flag, v)
            }
        }
        return nil
    },
}

func init() {
    userUpdateCmd.Flags().Int("max-sandboxes", 0, "Max concurrent sandboxes")
    userUpdateCmd.Flags().Int("max-cpus", 0, "Max vCPUs per sandbox")
    userUpdateCmd.Flags().Int("max-memory", 0, "Max memory MB per sandbox")
    userUpdateCmd.Flags().Int("max-volume-storage", 0, "Max volume storage MB")
    userUpdateCmd.Flags().Int("max-images", 0, "Max images")
    userUpdateCmd.Flags().Int("max-snapshots", 0, "Max snapshots")
    userCmd.AddCommand(userUpdateCmd)
}
```

### 5.3 Tests

- `TestUpdateUser` — update max_sandboxes, verify change persists
- `TestUpdateUserMultipleFields` — update cpus + memory in one call
- `TestUpdateUserNotFound` — non-existent name → error
- `TestUpdateUserNoFlags` — no flags → "nothing to update" error
- `TestUpdateUserInvalidColumn` — column not in allowlist → rejected

---

## Part 6 — Polish

### 6.1 Confirmation for Destructive Operations

`destroy`, `user delete`, `volume delete`, `snapshot delete`, `image delete`
should confirm unless `--yes` / `-y` is passed:

```go
func confirmAction(cmd *cobra.Command, msg string) bool {
    yes, _ := cmd.Flags().GetBool("yes")
    if yes {
        return true
    }
    if !term.IsTerminal(int(os.Stdin.Fd())) {
        fmt.Fprintf(os.Stderr, "Use --yes to confirm in non-interactive mode\n")
        return false
    }
    fmt.Fprintf(os.Stderr, "%s [y/N]: ", msg)
    var answer string
    fmt.Scanln(&answer)
    return strings.ToLower(answer) == "y"
}
```

Add to destructive commands:

```go
destroyCmd.Flags().BoolP("yes", "y", false, "Skip confirmation")
// same for: userDeleteCmd, volumeDeleteCmd, snapshotDeleteCmd, imageDeleteCmd
```

Usage in `destroyCmd.RunE`:

```go
if !confirmAction(cmd, fmt.Sprintf("Destroy sandbox %q?", args[0])) {
    return nil
}
```

### 6.2 `exec` Usage Update

Update the `Use` string to show `--` is optional:

```go
Use: "exec <sandbox> [--] <command...>",
```

---

## Implementation Order

### Phase 1: Thermal Fix (Part 3)

Fix the production bug first. Users are affected now.

1. Rewrite `runThermalCycle` — split hot and warm paths
2. Set `lastActivity` on hot→warm transition
3. Add `TestThermalWarmToCold` and `TestThermalHotToWarm`
4. Deploy, verify with `journalctl` that cold transitions fire

### Phase 2: List Enrichment (Part 3 continued)

Small server changes, completes the issue #3 fix.

1. Add `ListUserPublishRules()` store method
2. Enrich `GET /sandboxes` with thermal state + published URLs
3. Update CLI `listCmd` to show THERMAL and URL columns
4. Test enriched list output

### Phase 3: CLI Discoverability (Part 2)

Zero risk, zero server changes, immediate UX win.

1. Add `AddGroup()` with three groups to root command
2. Add `GroupID` to all existing commands
3. Add `Example` and `Long` strings to all commands (including subcommands)
4. Add root command `Long` with quick start
5. Add completion hint to `setup`
6. Set `serveCmd.GroupID` in `main.go`

### Phase 4: Essential Commands (Part 4)

Zero server changes. Wire existing API routes.

1. Add `stop` command
2. Add `start` command
3. Add `inspect` command
4. Add `--template` flag to `create`
5. Add GroupID for new commands

### Phase 5: Local Image Import + Sharing (Part 1)

CLI change: `docker save` shell-out + HTTP streaming.
Server change: one new route, one new oci function.
Store change: `image_shares` table, updated queries.

1. Add `oci.ImportFromTarball()` in `pkg/oci/oci.go`
2. Add `handleImageImport` route in `pkg/server/routes.go`
   (receives tarball stream, converts to ext4)
3. Add `image import` subcommand in CLI
   (runs `docker save` locally, streams to server)
4. Add `image_shares` table + store methods
5. Update `GetImage` / `ListImages` to include shared images
6. Add `image share` / `image unshare` CLI commands
7. Update `imagePullCmd` to suggest import on auth errors
8. Test import + sharing

### Phase 6: User Update + Polish (Parts 5 & 6)

1. Add `store.UpdateUser()` with column allowlist
2. Add `user update` CLI command
3. Add `--yes` flag to destructive commands
4. Update `exec` usage string

### Dependency Graph

```
Phase 1 (thermal fix)       — standalone, URGENT
Phase 2 (list enrichment)   — standalone (can merge with Phase 1)
Phase 3 (discoverability)   — standalone
Phase 4 (stop/start/inspect) — standalone
Phase 5 (image import)      — standalone
Phase 6 (user update + polish) — standalone
```

Phase 1 first (production bug). Then 2 (finishes the issue). Then 3‒6
in any order. Recommended: 3 before 4 so the new commands get GroupIDs
and examples from the start.

---

## What's Explicitly Not in This Plan

**Registry auth (`bhatti login` / credential store).** Replaced by
`bhatti image import`. The user's local Docker handles registry auth;
the CLI runs `docker save` locally and streams the tarball to the server.

**`bhatti health` / `bhatti metrics` CLI commands.** Reachable via
`curl localhost:8080/health` and `curl localhost:8080/metrics`. Not
worth CLI surface area for how rarely they're used.

**`bhatti template` CLI commands.** Templates are available via the API.
The primary use case (shared golden images) is solved by `image share`.
Full template CRUD via CLI is low-priority — defer until there's a
workflow that image sharing + `--image` on create doesn't cover.

**`bhatti admin status` / `bhatti admin gc`.** GC runs at daemon startup.
Status is covered by metrics. Not worth the code for the audience size.

**Color output.** Adds a dependency and a `NO_COLOR` / `TERM` detection
matrix. Defer to v0.7.

**`--format` / `-o` output modes.** `--json` covers the programmatic case.

**`bhatti logs`.** Requires new storage (exec results aren't persisted).

**Auto-generated docs via cobra/doc.** Nice-to-have, not blocking. The
examples in `--help` are the primary documentation.

**`--quiet` flag.** Useful for scripting but `--json | jq -r .id` covers
the common case. Defer.
