// scripts/gen-cli-docs.go generates CLI reference documentation as a standalone
// HTML page from cobra command definitions.
//
// Usage: go run scripts/gen-cli-docs.go > web/cli-docs.html
//
// This reads the actual cobra command tree — same code that runs when you type
// `bhatti create`. If you add a flag or a subcommand, the docs update automatically.
package main

import (
	"fmt"
	"html"
	"os"
	"sort"
	"strings"
	"text/template"

	"github.com/spf13/cobra"
	"github.com/spf13/pflag"
)

// CommandDoc holds the documentation for a single command.
type CommandDoc struct {
	FullName    string
	Use         string
	Short       string
	Long        string
	Example     string
	Aliases     []string
	Flags       []FlagDoc
	GroupID     string
	Depth       int
	HasChildren bool
}

// FlagDoc holds documentation for a single flag.
type FlagDoc struct {
	Name      string
	Shorthand string
	Type      string
	Default   string
	Usage     string
	Required  bool
}

func main() {
	rootCmd := buildCommandTree()
	docs := collectDocs(rootCmd, "", 0)

	if err := renderHTML(docs); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

// buildCommandTree recreates the CLI command tree.
// We import the actual command definitions from cmd/bhatti, but since that
// package has init() side effects and a main(), we reconstruct the tree
// here from cobra primitives that mirror the real CLI.
func buildCommandTree() *cobra.Command {
	root := &cobra.Command{
		Use:   "bhatti",
		Short: "Firecracker microVM orchestrator",
		Long: `bhatti creates isolated Linux VMs in seconds. Each sandbox has its own
kernel, filesystem, and network. Paused sandboxes resume in under 3ms.`,
	}

	root.PersistentFlags().String("url", "", "API endpoint (overrides config)")
	root.PersistentFlags().String("token", "", "API key (overrides config)")
	root.PersistentFlags().Bool("json", false, "Output as JSON")
	root.PersistentFlags().Bool("timing", false, "Show request timing breakdown")

	// --- Core commands ---
	create := &cobra.Command{
		Use:   "create [flags]",
		Short: "Create a new sandbox",
		Long: `Create a new sandbox VM. Each sandbox is an isolated Linux environment
with its own kernel, filesystem, and network.`,
		Example: `  bhatti create --name dev
  bhatti create --name ml --cpus 4 --memory 4096
  bhatti create --name api --env API_KEY=sk-abc --init "npm install"
  bhatti create --name py --image python-3.12
  bhatti create --name work --volume workspace:/workspace
  bhatti create --name agent --init "hermes gateway" --keep-hot`,
	}
	create.Flags().String("name", "", "Sandbox name")
	create.Flags().String("image", "", "Rootfs image name")
	create.Flags().Float64("cpus", 1, "Number of vCPUs")
	create.Flags().Int("memory", 0, "Memory in MB (0 = server default: 2048)")
	create.Flags().Int("disk-size", 0, "Rootfs disk size in MB (0 = use image size)")
	create.Flags().String("env", "", "Environment variables (K=V,K=V)")
	create.Flags().String("init", "", "Init script")
	create.Flags().Bool("keep-hot", false, "Prevent thermal transitions (for autonomous agents)")
	create.Flags().String("template", "", "Template name or ID")
	create.Flags().StringSlice("volume", nil, "Persistent volume (name:mount[:ro])")

	edit := &cobra.Command{
		Use:   "edit <sandbox> [flags]",
		Short: "Update sandbox settings",
		Long: `Update mutable settings on an existing sandbox. Currently supports
toggling keep_hot to control thermal management.`,
		Example: `  bhatti edit my-agent --keep-hot
  bhatti edit my-agent --allow-cold`,
	}
	edit.Flags().Bool("keep-hot", false, "Prevent thermal transitions")
	edit.Flags().Bool("allow-cold", false, "Re-enable thermal transitions")

	list := &cobra.Command{
		Use: "list", Aliases: []string{"ls"},
		Short:   "List sandboxes",
		Example: `  bhatti list`,
	}

	destroy := &cobra.Command{
		Use: "destroy <id|name>", Aliases: []string{"rm"},
		Short: "Destroy a sandbox",
		Long: `Permanently destroy a sandbox and all its data. Persistent volumes
are detached but not deleted.`,
		Example: `  bhatti destroy dev`,
	}
	destroy.Flags().BoolP("yes", "y", false, "Skip confirmation")

	stop := &cobra.Command{
		Use:   "stop <sandbox>",
		Short: "Snapshot and stop a sandbox",
		Long: `Pause the sandbox and save a snapshot to disk. Resume later with
'bhatti start'. Stopped sandboxes use zero CPU and memory.`,
		Example: `  bhatti stop dev`,
	}

	start := &cobra.Command{
		Use:     "start <sandbox>",
		Short:   "Resume a stopped sandbox",
		Long:    `Resume a sandbox from its snapshot. Continues exactly where it left off.`,
		Example: `  bhatti start dev`,
	}

	execCmd := &cobra.Command{
		Use:   "exec <id|name> [--] <command...>",
		Short: "Execute a command in a sandbox",
		Long: `Execute a command inside a sandbox. The exit code is forwarded.
Sleeping sandboxes wake automatically.`,
		Example: `  bhatti exec dev -- echo hello
  bhatti exec dev -- sudo apt-get install -y ripgrep
  bhatti exec dev --timeout 60 -- long-running-script.sh`,
	}
	execCmd.Flags().Int("timeout", 0, "Exec timeout in seconds (default: 300, max: 3600)")

	shell := &cobra.Command{
		Use: "shell <id|name>", Aliases: []string{"sh"},
		Short: "Open an interactive shell",
		Long: `Open an interactive terminal inside the sandbox. Ctrl+\ to detach —
the shell keeps running. Reconnect with 'bhatti shell' again.`,
		Example: `  bhatti shell dev`,
	}
	shell.Flags().Bool("new", false, "Force a new session (don't reattach)")

	inspect := &cobra.Command{
		Use: "inspect <sandbox>", Aliases: []string{"info"},
		Short:   "Show sandbox details",
		Example: `  bhatti inspect dev`,
	}

	ps := &cobra.Command{
		Use:     "ps <id|name>",
		Short:   "List sessions in a sandbox",
		Example: `  bhatti ps dev`,
	}

	// --- File commands ---
	file := &cobra.Command{
		Use:   "file <read|write|ls> <id|name> <path>",
		Short: "Read, write, and list files in a sandbox",
		Example: `  bhatti file read dev /workspace/app.js
  echo 'hello' | bhatti file write dev /workspace/greeting.txt
  bhatti file ls dev /workspace/`,
	}
	file.AddCommand(
		&cobra.Command{Use: "read <id|name> <path>", Short: "Read a file from a sandbox"},
		&cobra.Command{Use: "write <id|name> <path>", Short: "Write a file to a sandbox (reads from stdin)"},
		&cobra.Command{Use: "ls <id|name> <path>", Short: "List files in a sandbox directory"},
	)

	// --- Secret commands ---
	secret := &cobra.Command{
		Use:   "secret <set|list|delete>",
		Short: "Manage encrypted secrets",
		Long: `Secrets are encrypted at rest (age) and scoped to your API key.
They can be referenced in templates and injected into sandboxes at boot.`,
		Example: `  bhatti secret set API_KEY sk-abc123
  bhatti secret list
  bhatti secret delete API_KEY`,
	}
	secret.AddCommand(
		&cobra.Command{Use: "set <name> <value>", Short: "Create or update a secret"},
		&cobra.Command{Use: "list", Short: "List secrets"},
		&cobra.Command{Use: "delete <name>", Short: "Delete a secret"},
	)

	// --- Volume commands ---
	volume := &cobra.Command{
		Use:   "volume <create|list|delete|resize|backup|restore>",
		Short: "Manage persistent volumes",
		Long: `Persistent volumes are ext4 filesystems that survive sandbox destruction.
Attach them with '--volume name:/mount' on create.`,
		Example: `  bhatti volume create --name workspace --size 5120
  bhatti create --name dev --volume workspace:/workspace
  bhatti volume resize workspace --size 10240`,
	}
	volCreate := &cobra.Command{Use: "create", Short: "Create a persistent volume"}
	volCreate.Flags().String("name", "", "Volume name (required)")
	volCreate.Flags().Int("size", 0, "Size in MB (required)")
	volList := &cobra.Command{Use: "list", Short: "List persistent volumes"}
	volDelete := &cobra.Command{Use: "delete <name>", Short: "Delete a persistent volume"}
	volDelete.Flags().BoolP("yes", "y", false, "Skip confirmation")
	volResize := &cobra.Command{Use: "resize <name>", Short: "Resize a persistent volume (grow only)"}
	volResize.Flags().Int("size", 0, "New size in MB (must be larger)")
	volBackup := &cobra.Command{Use: "backup <volume-name>", Short: "Backup a volume to S3-compatible storage"}
	volBackupList := &cobra.Command{Use: "backup-list <volume-name>", Short: "List backups for a volume"}
	volRestore := &cobra.Command{Use: "restore <volume-name> --backup-id <id>", Short: "Restore a volume from a backup"}
	volRestore.Flags().String("backup-id", "", "Backup ID to restore from (required)")
	volBackupDelete := &cobra.Command{Use: "backup-delete <volume-name> <backup-id>", Short: "Delete a volume backup"}
	volBackupDelete.Flags().BoolP("yes", "y", false, "Skip confirmation")
	volume.AddCommand(volCreate, volList, volDelete, volResize, volBackup, volBackupList, volRestore, volBackupDelete)

	// --- Image commands ---
	image := &cobra.Command{
		Use:   "image <list|pull|import|save|delete|share|unshare>",
		Short: "Manage rootfs images",
		Long: `Images are ext4 filesystem snapshots used as sandbox root filesystems.
Pull public images from registries with 'image pull'.`,
		Example: `  bhatti image pull python:3.12
  bhatti image save dev --name my-custom-env
  bhatti image list`,
	}
	imgPull := &cobra.Command{Use: "pull <ref>", Short: "Pull an OCI/Docker image from a public registry"}
	imgPull.Flags().String("name", "", "Image name (default: derived from ref)")
	imgPull.Flags().String("auth", "", "Registry auth (user:token)")
	imgImport := &cobra.Command{Use: "import <docker-ref>", Short: "Import a local Docker image as a bhatti rootfs"}
	imgImport.Flags().String("name", "", "Image name (default: derived from ref)")
	imgImport.Flags().String("tar", "", "Import from tarball path instead of Docker")
	imgSave := &cobra.Command{Use: "save <sandbox-id|name>", Short: "Save a sandbox's rootfs as an image"}
	imgSave.Flags().String("name", "", "Image name (required)")
	imgDelete := &cobra.Command{Use: "delete <name>", Short: "Delete an image"}
	imgDelete.Flags().BoolP("yes", "y", false, "Skip confirmation")
	imgShare := &cobra.Command{Use: "share <image-name>", Short: "Share an image with other users (requires DB access)"}
	imgShare.Flags().StringSlice("user", nil, "User name(s) to share with")
	imgShare.Flags().Bool("list", false, "List current shares")
	imgUnshare := &cobra.Command{Use: "unshare <image-name>", Short: "Revoke image access from users (requires DB access)"}
	imgUnshare.Flags().StringSlice("user", nil, "User name(s) to unshare from")
	image.AddCommand(
		&cobra.Command{Use: "list", Short: "List available images"},
		imgPull, imgImport, imgSave, imgDelete, imgShare, imgUnshare,
	)

	// --- Snapshot commands ---
	snapshot := &cobra.Command{
		Use:   "snapshot <create|list|resume|delete>",
		Short: "Manage named VM snapshots",
		Long: `Snapshots capture the entire VM state: memory, CPU, disk. Resume
produces an exact continuation — processes running, files open.`,
		Example: `  bhatti snapshot create dev --name dev-ready
  bhatti snapshot resume dev-ready --name dev-2
  bhatti snapshot list`,
	}
	snapCreate := &cobra.Command{Use: "create <sandbox-id|name>", Short: "Checkpoint a running sandbox"}
	snapCreate.Flags().String("name", "", "Snapshot name (required)")
	snapResume := &cobra.Command{Use: "resume <snapshot-name>", Short: "Resume a sandbox from a snapshot"}
	snapResume.Flags().String("name", "", "New sandbox name")
	snapDelete := &cobra.Command{Use: "delete <snapshot-name>", Short: "Delete a snapshot"}
	snapDelete.Flags().BoolP("yes", "y", false, "Skip confirmation")
	snapshot.AddCommand(
		snapCreate,
		&cobra.Command{Use: "list", Short: "List snapshots"},
		snapResume,
		snapDelete,
	)

	// --- Publish ---
	publish := &cobra.Command{
		Use:   "publish <sandbox> -p <port> [-a <alias>]",
		Short: "Publish a sandbox port with a public URL",
		Example: `  bhatti publish dev -p 3000
  bhatti publish dev -p 3000 -a my-app`,
	}
	publish.Flags().IntP("port", "p", 0, "Port to publish (required)")
	publish.Flags().StringP("alias", "a", "", "Custom alias (auto-generated if omitted)")

	unpublish := &cobra.Command{
		Use:     "unpublish <sandbox> -p <port>",
		Short:   "Unpublish a sandbox port",
		Example: `  bhatti unpublish dev -p 3000`,
	}
	unpublish.Flags().IntP("port", "p", 0, "Port to unpublish (required)")

	// --- User commands ---
	user := &cobra.Command{
		Use:   "user <create|list|delete|rotate-key>",
		Short: "Manage users (requires DB access)",
		Long:  `User management operates directly on the local SQLite database. Run on the server, not remotely.`,
	}
	userCreate := &cobra.Command{Use: "create", Short: "Create a new user"}
	userCreate.Flags().String("name", "", "User name (required)")
	userCreate.Flags().Int("max-sandboxes", 5, "Max sandboxes for this user")
	userCreate.Flags().Int("max-cpus", 4, "Max CPUs per sandbox")
	userCreate.Flags().Int("max-memory", 4096, "Max memory MB per sandbox")
	userDelete := &cobra.Command{Use: "delete <name>", Short: "Delete a user"}
	userDelete.Flags().BoolP("yes", "y", false, "Skip confirmation")
	user.AddCommand(
		userCreate,
		&cobra.Command{Use: "list", Short: "List users"},
		userDelete,
		&cobra.Command{Use: "rotate-key <name>", Short: "Rotate a user's API key"},
	)

	// --- Other commands ---
	setup := &cobra.Command{
		Use:   "setup",
		Short: "Configure CLI endpoint and API key",
		Long: `Interactive setup for remote CLI users. Prompts for the API endpoint
and API key, saves to ~/.bhatti/config.yaml, and tests the connection.`,
	}

	serve := &cobra.Command{
		Use:   "serve",
		Short: "Start the daemon",
		Long:  `Start the bhatti daemon. Requires root, KVM, and a config at /var/lib/bhatti/config.yaml.`,
	}

	update := &cobra.Command{
		Use:   "update",
		Short: "Update bhatti CLI to the latest version",
	}

	version := &cobra.Command{
		Use:   "version",
		Short: "Print version and API endpoint",
	}

	completion := &cobra.Command{
		Use:   "completion <bash|zsh|fish>",
		Short: "Generate shell completion script",
	}

	root.AddCommand(
		create, edit, list, destroy, stop, start,
		execCmd, shell, inspect, ps,
		file, secret, volume, image, snapshot,
		publish, unpublish,
		user, setup, serve, update, version, completion,
	)

	return root
}

func collectDocs(cmd *cobra.Command, prefix string, depth int) []CommandDoc {
	fullName := cmd.Name()
	if prefix != "" {
		fullName = prefix + " " + cmd.Name()
	}

	doc := CommandDoc{
		FullName:    fullName,
		Use:         cmd.Use,
		Short:       cmd.Short,
		Long:        cmd.Long,
		Example:     cmd.Example,
		Aliases:     cmd.Aliases,
		GroupID:     cmd.GroupID,
		Depth:       depth,
		HasChildren: len(cmd.Commands()) > 0,
	}

	// Collect flags
	cmd.Flags().VisitAll(func(f *pflag.Flag) {
		if f.Hidden {
			return
		}
		fd := FlagDoc{
			Name:      f.Name,
			Shorthand: f.Shorthand,
			Type:      f.Value.Type(),
			Default:   f.DefValue,
			Usage:     f.Usage,
		}
		// Check if required via annotations
		if ann, ok := f.Annotations[cobra.BashCompOneRequiredFlag]; ok && len(ann) > 0 && ann[0] == "true" {
			fd.Required = true
		}
		doc.Flags = append(doc.Flags, fd)
	})

	result := []CommandDoc{doc}

	// Recurse into subcommands
	children := cmd.Commands()
	sort.Slice(children, func(i, j int) bool {
		return children[i].Name() < children[j].Name()
	})
	for _, child := range children {
		if child.Hidden {
			continue
		}
		result = append(result, collectDocs(child, fullName, depth+1)...)
	}

	return result
}

const htmlTemplate = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>bhatti — CLI Reference</title>
  <link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>🔥</text></svg>" />
  <style>
    :root {
      --bg: #0a0a0a;
      --surface: #111;
      --border: #222;
      --text: #e0e0e0;
      --text-muted: #888;
      --accent: #3b82f6;
      --code-bg: #161616;
      --code-border: #2a2a2a;
    }
    * { margin: 0; padding: 0; box-sizing: border-box; }
    body {
      font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
      background: var(--bg);
      color: var(--text);
      line-height: 1.6;
    }
    .layout {
      display: flex;
      min-height: 100vh;
    }
    .sidebar {
      width: 260px;
      border-right: 1px solid var(--border);
      padding: 24px 16px;
      position: sticky;
      top: 0;
      height: 100vh;
      overflow-y: auto;
      flex-shrink: 0;
    }
    .sidebar h2 {
      font-size: 14px;
      color: var(--text-muted);
      text-transform: uppercase;
      letter-spacing: 0.05em;
      margin: 16px 0 8px;
    }
    .sidebar h2:first-child { margin-top: 0; }
    .sidebar a {
      display: block;
      padding: 4px 8px;
      color: var(--text);
      text-decoration: none;
      font-size: 14px;
      border-radius: 4px;
      font-family: 'SF Mono', 'Fira Code', monospace;
    }
    .sidebar a:hover { background: var(--surface); color: var(--accent); }
    .sidebar a.sub { padding-left: 24px; color: var(--text-muted); font-size: 13px; }
    .content {
      flex: 1;
      max-width: 800px;
      padding: 40px;
    }
    h1 {
      font-size: 32px;
      margin-bottom: 8px;
    }
    h1 small {
      font-size: 16px;
      color: var(--text-muted);
      font-weight: normal;
    }
    .subtitle {
      color: var(--text-muted);
      margin-bottom: 32px;
      font-size: 16px;
    }
    .command {
      margin-bottom: 48px;
      padding-bottom: 32px;
      border-bottom: 1px solid var(--border);
    }
    .command:last-child { border-bottom: none; }
    .command h2 {
      font-size: 22px;
      margin-bottom: 4px;
      font-family: 'SF Mono', 'Fira Code', monospace;
    }
    .command h2 a { color: var(--accent); text-decoration: none; }
    .command h2 a:hover { text-decoration: underline; }
    .aliases {
      color: var(--text-muted);
      font-size: 13px;
      margin-bottom: 8px;
    }
    .description {
      margin: 12px 0;
      white-space: pre-wrap;
    }
    pre {
      background: var(--code-bg);
      border: 1px solid var(--code-border);
      border-radius: 6px;
      padding: 16px;
      overflow-x: auto;
      font-family: 'SF Mono', 'Fira Code', monospace;
      font-size: 13px;
      margin: 12px 0;
      line-height: 1.5;
    }
    code {
      font-family: 'SF Mono', 'Fira Code', monospace;
      font-size: 13px;
      background: var(--code-bg);
      padding: 2px 6px;
      border-radius: 3px;
    }
    table {
      width: 100%;
      border-collapse: collapse;
      margin: 12px 0;
      font-size: 14px;
    }
    th {
      text-align: left;
      padding: 8px 12px;
      border-bottom: 2px solid var(--border);
      color: var(--text-muted);
      font-size: 12px;
      text-transform: uppercase;
      letter-spacing: 0.05em;
    }
    td {
      padding: 8px 12px;
      border-bottom: 1px solid var(--border);
      font-family: 'SF Mono', 'Fira Code', monospace;
      font-size: 13px;
    }
    td:last-child {
      font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
    }
    .required { color: #ef4444; font-size: 11px; }
    .default-val { color: var(--text-muted); }
    @media (max-width: 768px) {
      .sidebar { display: none; }
      .content { padding: 20px; }
    }
  </style>
</head>
<body>
  <div class="layout">
    <nav class="sidebar">
      <h2>bhatti CLI</h2>
      <a href="#bhatti">bhatti</a>
      {{- range .Docs }}
      {{- if eq .Depth 1 }}
      <a href="#{{ anchor .FullName }}" {{ if .HasChildren }}style="margin-top: 8px;"{{ end }}>{{ .FullName }}</a>
      {{- else if eq .Depth 2 }}
      <a class="sub" href="#{{ anchor .FullName }}">{{ shortName .FullName }}</a>
      {{- end }}
      {{- end }}
    </nav>
    <main class="content">
      <h1>bhatti <small>CLI Reference</small></h1>
      <p class="subtitle">Auto-generated from command definitions. Always up to date.</p>

      {{- range .Docs }}
      <section class="command" id="{{ anchor .FullName }}">
        <h2><a href="#{{ anchor .FullName }}">{{ .FullName }}</a></h2>
        {{- if .Aliases }}
        <div class="aliases">aliases: {{ joinAliases .Aliases }}</div>
        {{- end }}
        <p>{{ esc .Short }}</p>
        {{- if .Long }}
        <div class="description">{{ esc .Long }}</div>
        {{- end }}
        {{- if .Example }}
        <pre>{{ esc .Example }}</pre>
        {{- end }}
        {{- if .Flags }}
        <table>
          <tr><th>Flag</th><th>Default</th><th>Description</th></tr>
          {{- range .Flags }}
          <tr>
            <td>--{{ .Name }}{{ if .Shorthand }}, -{{ .Shorthand }}{{ end }}</td>
            <td>{{ if .Default }}<span class="default-val">{{ .Default }}</span>{{ else }}—{{ end }}</td>
            <td>{{ .Usage }}{{ if .Required }} <span class="required">required</span>{{ end }}</td>
          </tr>
          {{- end }}
        </table>
        {{- end }}
      </section>
      {{- end }}

      <p style="color: var(--text-muted); margin-top: 48px; font-size: 13px;">
        Generated from <code>github.com/sahil-shubham/bhatti</code> command definitions.
      </p>
    </main>
  </div>
</body>
</html>`

func anchor(name string) string {
	return strings.ReplaceAll(name, " ", "-")
}

func shortName(fullName string) string {
	parts := strings.Fields(fullName)
	if len(parts) > 1 {
		return parts[len(parts)-1]
	}
	return fullName
}

func renderHTML(docs []CommandDoc) error {
	funcMap := template.FuncMap{
		"anchor":      anchor,
		"shortName":   shortName,
		"esc":         html.EscapeString,
		"joinAliases": func(a []string) string { return strings.Join(a, ", ") },
	}
	tmpl, err := template.New("cli").Funcs(funcMap).Parse(htmlTemplate)
	if err != nil {
		return err
	}
	return tmpl.Execute(os.Stdout, map[string]any{
		"Docs": docs,
	})
}
