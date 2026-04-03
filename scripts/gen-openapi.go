// scripts/gen-openapi.go generates an OpenAPI 3.1 spec from the actual server routes.
// Run: go run scripts/gen-openapi.go > docs/openapi.yaml
//
// This is the single source of truth for the API. The Scalar docs page
// (web/api-docs.html) and the markdown reference (docs/api-reference.md)
// are both derived from this spec.
package main

import (
	"fmt"
	"os"
	"strings"
)

// Route describes one API endpoint.
type Route struct {
	Method      string
	Path        string
	OperationID string
	Summary     string
	Description string
	Tags        []string
	RequestBody string // YAML fragment
	Responses   string // YAML fragment
	Parameters  string // YAML fragment (for path/query params)
}

// PathGroup holds all methods for one API path.
type PathGroup struct {
	Path   string
	Routes []Route
}

func main() {
	routes := allRoutes()

	// Group routes by path (preserving first-seen order)
	pathOrder := []string{}
	pathMap := map[string][]Route{}
	for _, r := range routes {
		if _, exists := pathMap[r.Path]; !exists {
			pathOrder = append(pathOrder, r.Path)
		}
		pathMap[r.Path] = append(pathMap[r.Path], r)
	}
	var groups []PathGroup
	for _, p := range pathOrder {
		groups = append(groups, PathGroup{Path: p, Routes: pathMap[p]})
	}

	if err := renderOpenAPI(groups); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func allRoutes() []Route {
	return []Route{
		// Health & Metrics (unauthenticated)
		{
			Method: "get", Path: "/health", OperationID: "getHealth",
			Summary: "Health check", Tags: []string{"System"},
			Description: "Returns server status, sandbox count, and uptime. No authentication required.",
			Responses: `
        "200":
          description: Server is healthy
          content:
            application/json:
              schema:
                type: object
                properties:
                  status: { type: string, example: "ok" }
                  sandboxes: { type: integer, example: 3 }
                  uptime: { type: string, example: "2h15m30s" }`,
		},
		{
			Method: "get", Path: "/metrics", OperationID: "getMetrics",
			Summary: "Server metrics", Tags: []string{"System"},
			Description: "Returns server metrics including sandbox counts by thermal state, user counts, host stats, and request counters. No authentication required.",
			Responses: `
        "200":
          description: Server metrics
          content:
            application/json:
              schema:
                type: object
                properties:
                  uptime: { type: string }
                  sandboxes:
                    type: object
                    properties:
                      total: { type: integer }
                      hot: { type: integer }
                      warm: { type: integer }
                      cold: { type: integer }
                  users:
                    type: object
                    properties:
                      total: { type: integer }
                      active: { type: integer }
                  host:
                    type: object
                    properties:
                      load_1m: { type: number }
                      memory_total_mb: { type: integer }
                      memory_available_mb: { type: integer }
                  requests:
                    type: object
                    properties:
                      total: { type: integer }
                      errors_5xx: { type: integer }
                      auth_failures: { type: integer }`,
		},

		// Sandboxes
		{
			Method: "get", Path: "/sandboxes", OperationID: "listSandboxes",
			Summary: "List sandboxes", Tags: []string{"Sandboxes"},
			Description: "List all sandboxes owned by the authenticated user. Includes thermal state and published URLs.",
			Responses: `
        "200":
          description: List of sandboxes
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/Sandbox"`,
		},
		{
			Method: "post", Path: "/sandboxes", OperationID: "createSandbox",
			Summary: "Create a sandbox", Tags: []string{"Sandboxes"},
			Description: "Create a new sandbox VM. Supports direct creation (specify resources inline) or template-based creation. Each sandbox is an isolated Linux environment with its own kernel, filesystem, and network.",
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            properties:
              name: { type: string, description: "Sandbox name (auto-generated if omitted)", example: "dev" }
              template_id: { type: string, description: "Template ID for template-based creation" }
              image: { type: string, description: "Rootfs image name", example: "python-3.12" }
              cpus: { type: number, description: "Number of vCPUs", default: 1, example: 2 }
              memory_mb: { type: integer, description: "Memory in MB", default: 2048, example: 1024 }
              disk_size_mb: { type: integer, description: "Rootfs disk size in MB (0 = use image size)" }
              env: { type: object, additionalProperties: { type: string }, description: "Environment variables", example: { "API_KEY": "sk-..." } }
              init: { type: string, description: "Init script to run at boot", example: "cd /workspace && npm install" }
              keep_hot: { type: boolean, description: "Prevent thermal manager from pausing this sandbox", default: false }
              hugepages: { type: boolean, description: "Use 2MB hugepages for memory", default: false }
              persistent_volumes:
                type: array
                items:
                  type: object
                  properties:
                    name: { type: string }
                    mount: { type: string }
                    auto_create: { type: boolean }
                    size_mb: { type: integer }
                    read_only: { type: boolean }`,
			Responses: `
        "201":
          description: Sandbox created
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Sandbox"
        "409":
          description: Sandbox name already exists
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Error"
        "429":
          description: Sandbox limit reached
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Error"`,
		},
		{
			Method: "get", Path: "/sandboxes/{id}", OperationID: "getSandbox",
			Summary: "Get sandbox details", Tags: []string{"Sandboxes"},
			Description: "Get details of a specific sandbox. Status is refreshed from the engine.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }
        description: Sandbox ID`,
			Responses: `
        "200":
          description: Sandbox details
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Sandbox"
        "404":
          description: Sandbox not found`,
		},
		{
			Method: "patch", Path: "/sandboxes/{id}", OperationID: "updateSandbox",
			Summary: "Update sandbox settings", Tags: []string{"Sandboxes"},
			Description: "Update mutable sandbox properties. Currently supports toggling keep_hot. Setting keep_hot=true immediately wakes the sandbox.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            properties:
              keep_hot: { type: boolean, description: "Prevent thermal transitions" }`,
			Responses: `
        "200":
          description: Updated sandbox
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Sandbox"`,
		},
		{
			Method: "delete", Path: "/sandboxes/{id}", OperationID: "destroySandbox",
			Summary: "Destroy a sandbox", Tags: []string{"Sandboxes"},
			Description: "Permanently destroy a sandbox and all its data. Persistent volumes are detached but not deleted. Published URLs are cleaned up.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Sandbox destroyed
          content:
            application/json:
              schema:
                type: object
                properties:
                  status: { type: string, example: "destroyed" }`,
		},
		{
			Method: "post", Path: "/sandboxes/{id}/stop", OperationID: "stopSandbox",
			Summary: "Snapshot and stop", Tags: []string{"Sandboxes"},
			Description: "Pause the sandbox and save a snapshot to disk. First stop creates a full snapshot; subsequent stops create diff snapshots (dirty pages only). Resume with start.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Sandbox stopped
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Sandbox"`,
		},
		{
			Method: "post", Path: "/sandboxes/{id}/start", OperationID: "startSandbox",
			Summary: "Resume a stopped sandbox", Tags: []string{"Sandboxes"},
			Description: "Restore a sandbox from its snapshot. Continues exactly where it left off — processes running, files open.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Sandbox started
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Sandbox"`,
		},

		// Exec
		{
			Method: "post", Path: "/sandboxes/{id}/exec", OperationID: "execCommand",
			Summary: "Execute a command", Tags: []string{"Execution"},
			Description: "Execute a command inside a sandbox. Sleeping sandboxes wake automatically. Supports buffered JSON response (default) or streaming NDJSON (set Accept: application/x-ndjson).",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [cmd]
            properties:
              cmd:
                type: array
                items: { type: string }
                description: Command and arguments
                example: ["echo", "hello"]
              timeout_sec:
                type: integer
                description: "Exec timeout in seconds (default: 300, max: 3600)"`,
			Responses: `
        "200":
          description: Command result (buffered)
          content:
            application/json:
              schema:
                type: object
                properties:
                  exit_code: { type: integer, example: 0 }
                  stdout: { type: string, example: "hello\n" }
                  stderr: { type: string, example: "" }
            application/x-ndjson:
              schema:
                description: "Streaming NDJSON — one event per line, flushed immediately"
                type: object
                properties:
                  type: { type: string, enum: [stdout, stderr, exit, error] }
                  data: { type: string }
                  exit_code: { type: integer }`,
		},

		// WebSocket Shell
		{
			Method: "get", Path: "/sandboxes/{id}/ws", OperationID: "shellWebSocket",
			Summary: "Interactive shell (WebSocket)", Tags: []string{"Execution"},
			Description: "Upgrade to WebSocket for an interactive terminal. Supports session reattach (detached sessions keep running). Send binary messages for keystrokes, text messages with JSON {\"type\":\"resize\",\"rows\":N,\"cols\":N} for terminal resize.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }
      - name: session
        in: query
        schema: { type: string }
        description: Session ID to reattach to
      - name: new
        in: query
        schema: { type: string, enum: ["true"] }
        description: Force a new session (don't reattach)`,
			Responses: `
        "101":
          description: WebSocket upgrade successful`,
		},

		// Files
		{
			Method: "get", Path: "/sandboxes/{id}/files", OperationID: "readFile",
			Summary: "Read a file or list directory", Tags: []string{"Files"},
			Description: "Read file content or list a directory. Supports server-side truncation with offset/limit/max_bytes parameters. Set ls=true to list directory contents.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }
      - name: path
        in: query
        required: true
        schema: { type: string }
        description: Absolute path inside the sandbox
        example: /workspace/app.js
      - name: ls
        in: query
        schema: { type: string, enum: ["true"] }
        description: List directory contents instead of reading file
      - name: offset
        in: query
        schema: { type: integer }
        description: 1-indexed line number to start from
      - name: limit
        in: query
        schema: { type: integer }
        description: Maximum lines to return
      - name: max_bytes
        in: query
        schema: { type: integer }
        description: Maximum bytes to return`,
			Responses: `
        "200":
          description: File content or directory listing
          content:
            application/octet-stream:
              schema:
                type: string
                format: binary
                description: Raw file content (for file reads)
            application/json:
              schema:
                type: array
                description: Directory listing (when ls=true)
                items:
                  type: object
                  properties:
                    name: { type: string }
                    size: { type: integer }
                    mode: { type: string }
                    is_dir: { type: boolean }
                    mtime: { type: integer }`,
		},
		{
			Method: "put", Path: "/sandboxes/{id}/files", OperationID: "writeFile",
			Summary: "Write a file", Tags: []string{"Files"},
			Description: "Write content to a file inside the sandbox. Content-Length header is required. Writes are atomic (temp file + rename) — concurrent readers never see partial content.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }
      - name: path
        in: query
        required: true
        schema: { type: string }
      - name: mode
        in: query
        schema: { type: string, default: "0644" }
        description: File permissions`,
			RequestBody: `
      required: true
      content:
        application/octet-stream:
          schema:
            type: string
            format: binary`,
			Responses: `
        "200":
          description: File written
          content:
            application/json:
              schema:
                type: object
                properties:
                  status: { type: string, example: "ok" }`,
		},
		{
			Method: "head", Path: "/sandboxes/{id}/files", OperationID: "statFile",
			Summary: "Stat a file", Tags: []string{"Files"},
			Description: "Get file metadata without reading content. Returns X-File-Size, X-File-Mode, and X-File-IsDir headers.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }
      - name: path
        in: query
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: File metadata in headers
          headers:
            X-File-Size: { schema: { type: integer }, description: "File size in bytes" }
            X-File-Mode: { schema: { type: string }, description: "Permissions (e.g., 0644)" }
            X-File-IsDir: { schema: { type: string }, description: "true or false" }`,
		},

		// Sessions
		{
			Method: "get", Path: "/sandboxes/{id}/sessions", OperationID: "listSessions",
			Summary: "List sessions", Tags: []string{"Execution"},
			Description: "List all sessions (init scripts, shells) running inside the sandbox.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Session list
          content:
            application/json:
              schema:
                type: array
                items:
                  type: object
                  properties:
                    session_id: { type: string }
                    argv: { type: string }
                    tty: { type: boolean }
                    running: { type: boolean }
                    attached: { type: boolean }
                    created_at: { type: integer }`,
		},

		// Ports
		{
			Method: "get", Path: "/sandboxes/{id}/ports", OperationID: "listSandboxPorts",
			Summary: "List listening ports", Tags: []string{"Networking"},
			Description: "List all listening TCP ports inside a sandbox.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Listening ports
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/PortInfo"`,
		},
		{
			Method: "get", Path: "/ports", OperationID: "listAllPorts",
			Summary: "List all listening ports", Tags: []string{"Networking"},
			Description: "List all listening ports across all running sandboxes owned by the authenticated user.",
			Responses: `
        "200":
          description: All listening ports
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/PortInfo"`,
		},

		// Proxy
		{
			Method: "get", Path: "/sandboxes/{id}/proxy/{port}/{path}", OperationID: "proxyRequest",
			Summary: "Reverse proxy to sandbox", Tags: []string{"Networking"},
			Description: "HTTP requests and WebSocket connections are tunneled through the engine into the sandbox. The request is rewritten to target localhost:{port} inside the VM. Supports all HTTP methods.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }
      - name: port
        in: path
        required: true
        schema: { type: integer, minimum: 1, maximum: 65535 }
      - name: path
        in: path
        required: false
        schema: { type: string }`,
			Responses: `
        "200":
          description: Proxied response from the sandbox
        "502":
          description: Bad gateway (sandbox process not listening)`,
		},

		// Publish
		{
			Method: "post", Path: "/sandboxes/{id}/publish", OperationID: "publishPort",
			Summary: "Publish a port", Tags: []string{"Networking"},
			Description: "Publish a sandbox port with a public URL (e.g., my-app.bhatti.sh). The URL is publicly accessible without authentication. Sandboxes wake automatically from any thermal state when a request arrives.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [port]
            properties:
              port: { type: integer, minimum: 1, maximum: 65535, example: 3000 }
              alias: { type: string, description: "Custom alias (auto-generated if omitted)", example: "my-app" }`,
			Responses: `
        "201":
          description: Port published
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: { type: string }
                  sandbox_id: { type: string }
                  port: { type: integer }
                  alias: { type: string }
                  url: { type: string, example: "https://my-app.bhatti.sh" }
                  created_at: { type: string, format: date-time }
        "409":
          description: Alias already taken or port already published`,
		},
		{
			Method: "get", Path: "/sandboxes/{id}/publish", OperationID: "listPublishRules",
			Summary: "List published ports", Tags: []string{"Networking"},
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Published ports
          content:
            application/json:
              schema:
                type: array
                items:
                  type: object
                  properties:
                    id: { type: string }
                    port: { type: integer }
                    alias: { type: string }
                    url: { type: string }`,
		},
		{
			Method: "delete", Path: "/sandboxes/{id}/publish/{port}", OperationID: "unpublishPort",
			Summary: "Unpublish a port", Tags: []string{"Networking"},
			Description: "Remove a published port. The URL stops working immediately.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }
      - name: port
        in: path
        required: true
        schema: { type: integer }`,
			Responses: `
        "204":
          description: Port unpublished`,
		},

		// Templates
		{
			Method: "get", Path: "/templates", OperationID: "listTemplates",
			Summary: "List templates", Tags: []string{"Templates"},
			Responses: `
        "200":
          description: Template list
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/Template"`,
		},
		{
			Method: "post", Path: "/templates", OperationID: "createTemplate",
			Summary: "Create a template", Tags: []string{"Templates"},
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            $ref: "#/components/schemas/Template"`,
			Responses: `
        "201":
          description: Template created
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Template"`,
		},
		{
			Method: "get", Path: "/templates/{id}", OperationID: "getTemplate",
			Summary: "Get a template", Tags: []string{"Templates"},
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Template details
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Template"`,
		},
		{
			Method: "delete", Path: "/templates/{id}", OperationID: "deleteTemplate",
			Summary: "Delete a template", Tags: []string{"Templates"},
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Template deleted`,
		},

		// Secrets
		{
			Method: "get", Path: "/secrets", OperationID: "listSecrets",
			Summary: "List secrets", Tags: []string{"Secrets"},
			Description: "List secret names (values are never returned). Secrets are encrypted at rest with age.",
			Responses: `
        "200":
          description: Secret names
          content:
            application/json:
              schema:
                type: array
                items:
                  type: object
                  properties:
                    name: { type: string }
                    created_at: { type: string, format: date-time }`,
		},
		{
			Method: "post", Path: "/secrets", OperationID: "createSecret",
			Summary: "Create or update a secret", Tags: []string{"Secrets"},
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [name, value]
            properties:
              name: { type: string, example: "API_KEY" }
              value: { type: string, example: "sk-abc123" }`,
			Responses: `
        "201":
          description: Secret created`,
		},
		{
			Method: "delete", Path: "/secrets/{name}", OperationID: "deleteSecret",
			Summary: "Delete a secret", Tags: []string{"Secrets"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Secret deleted`,
		},

		// Volumes
		{
			Method: "get", Path: "/volumes", OperationID: "listVolumes",
			Summary: "List persistent volumes", Tags: []string{"Volumes"},
			Responses: `
        "200":
          description: Volume list
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/Volume"`,
		},
		{
			Method: "post", Path: "/volumes", OperationID: "createVolume",
			Summary: "Create a persistent volume", Tags: []string{"Volumes"},
			Description: "Create an ext4 persistent volume. Survives sandbox destruction. Attach with persistent_volumes on sandbox create.",
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [name, size_mb]
            properties:
              name: { type: string, example: "workspace" }
              size_mb: { type: integer, minimum: 1, example: 5120 }`,
			Responses: `
        "201":
          description: Volume created
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Volume"
        "409":
          description: Volume name already exists
        "429":
          description: Storage quota exceeded`,
		},
		{
			Method: "get", Path: "/volumes/{name}", OperationID: "getVolume",
			Summary: "Get volume details", Tags: []string{"Volumes"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Volume details
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Volume"`,
		},
		{
			Method: "delete", Path: "/volumes/{name}", OperationID: "deleteVolume",
			Summary: "Delete a volume", Tags: []string{"Volumes"},
			Description: "Delete a persistent volume. Fails if the volume is currently attached to a sandbox.",
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Volume deleted
        "409":
          description: Volume is attached to a sandbox`,
		},
		{
			Method: "post", Path: "/volumes/{name}/resize", OperationID: "resizeVolume",
			Summary: "Resize a volume (grow only)", Tags: []string{"Volumes"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [size_mb]
            properties:
              size_mb: { type: integer, description: "New size (must be larger than current)" }`,
			Responses: `
        "200":
          description: Volume resized`,
		},
		{
			Method: "post", Path: "/volumes/{name}/snapshot", OperationID: "snapshotVolume",
			Summary: "Create a volume copy", Tags: []string{"Volumes"},
			Description: "Create an independent copy (snapshot) of a persistent volume. Volume must be detached.",
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [name]
            properties:
              name: { type: string, description: "Name for the new volume copy" }`,
			Responses: `
        "201":
          description: Volume copy created
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Volume"`,
		},

		// Volume Backups
		{
			Method: "get", Path: "/volumes/{name}/backups", OperationID: "listVolumeBackups",
			Summary: "List volume backups", Tags: []string{"Backups"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Backup list
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/VolumeBackup"`,
		},
		{
			Method: "post", Path: "/volumes/{name}/backups", OperationID: "backupVolume",
			Summary: "Backup a volume to S3", Tags: []string{"Backups"},
			Description: "Create a compressed (zstd) backup of a volume and upload to S3-compatible storage.",
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "201":
          description: Backup created
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/VolumeBackup"`,
		},
		{
			Method: "post", Path: "/volumes/{name}/backups/restore", OperationID: "restoreVolumeBackup",
			Summary: "Restore a volume from backup", Tags: []string{"Backups"},
			Description: "Restore a volume from a previous backup. Volume must be detached.",
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [backup_id]
            properties:
              backup_id: { type: string }`,
			Responses: `
        "200":
          description: Volume restored
        "409":
          description: Volume is attached`,
		},
		{
			Method: "delete", Path: "/volumes/{name}/backups/{backupId}", OperationID: "deleteVolumeBackup",
			Summary: "Delete a backup", Tags: []string{"Backups"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }
      - name: backupId
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Backup deleted`,
		},

		// Images
		{
			Method: "get", Path: "/images", OperationID: "listImages",
			Summary: "List images", Tags: []string{"Images"},
			Description: "List all rootfs images available to the authenticated user (own images + admin images + shared images).",
			Responses: `
        "200":
          description: Image list
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/Image"`,
		},
		{
			Method: "get", Path: "/images/{name}", OperationID: "getImage",
			Summary: "Get image details", Tags: []string{"Images"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Image details
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Image"`,
		},
		{
			Method: "delete", Path: "/images/{name}", OperationID: "deleteImage",
			Summary: "Delete an image", Tags: []string{"Images"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Image deleted`,
		},
		{
			Method: "post", Path: "/images/pull", OperationID: "pullImage",
			Summary: "Pull an OCI image", Tags: []string{"Images"},
			Description: "Pull a public OCI/Docker image from any registry and convert it to an ext4 rootfs. Returns a task ID for polling. If the image already exists with the same digest, returns immediately.",
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [ref, name]
            properties:
              ref: { type: string, example: "python:3.12" }
              name: { type: string, example: "python-3.12" }
              auth: { type: string, description: "Registry auth (user:token)" }`,
			Responses: `
        "202":
          description: Pull started (async)
          content:
            application/json:
              schema:
                type: object
                properties:
                  task_id: { type: string }
                  status: { type: string, example: "running" }
        "200":
          description: Image already exists with same digest`,
		},
		{
			Method: "post", Path: "/images/import", OperationID: "importImage",
			Summary: "Import image from tarball", Tags: []string{"Images"},
			Description: "Import a Docker save tarball and convert to ext4 rootfs. Stream the tarball as the request body.",
			Parameters: `
      - name: name
        in: query
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/x-tar:
          schema:
            type: string
            format: binary`,
			Responses: `
        "201":
          description: Image imported
          content:
            application/json:
              schema:
                type: object
                properties:
                  name: { type: string }
                  size_mb: { type: integer }`,
		},
		{
			Method: "post", Path: "/sandboxes/{id}/save-image", OperationID: "saveImage",
			Summary: "Save sandbox as image", Tags: []string{"Images"},
			Description: "Save the current rootfs of a running sandbox as a reusable image.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [name]
            properties:
              name: { type: string }`,
			Responses: `
        "201":
          description: Image saved
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Image"`,
		},

		// Snapshots
		{
			Method: "get", Path: "/snapshots", OperationID: "listSnapshots",
			Summary: "List named snapshots", Tags: []string{"Snapshots"},
			Responses: `
        "200":
          description: Snapshot list
          content:
            application/json:
              schema:
                type: array
                items:
                  $ref: "#/components/schemas/Snapshot"`,
		},
		{
			Method: "get", Path: "/snapshots/{name}", OperationID: "getSnapshot",
			Summary: "Get snapshot details", Tags: []string{"Snapshots"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Snapshot details
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Snapshot"`,
		},
		{
			Method: "delete", Path: "/snapshots/{name}", OperationID: "deleteSnapshot",
			Summary: "Delete a snapshot", Tags: []string{"Snapshots"},
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Snapshot deleted`,
		},
		{
			Method: "post", Path: "/sandboxes/{id}/checkpoint", OperationID: "createCheckpoint",
			Summary: "Create a named snapshot", Tags: []string{"Snapshots"},
			Description: "Checkpoint a running sandbox — captures entire VM state: memory, CPU, disk. Resume produces an exact continuation.",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      required: true
      content:
        application/json:
          schema:
            type: object
            required: [name]
            properties:
              name: { type: string }`,
			Responses: `
        "201":
          description: Checkpoint created
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Snapshot"
        "429":
          description: Snapshot limit reached`,
		},
		{
			Method: "post", Path: "/snapshots/{name}/resume", OperationID: "resumeSnapshot",
			Summary: "Resume from snapshot", Tags: []string{"Snapshots"},
			Description: "Create a new sandbox by resuming from a named snapshot. The new sandbox is an exact copy of the VM at checkpoint time.",
			Parameters: `
      - name: name
        in: path
        required: true
        schema: { type: string }`,
			RequestBody: `
      content:
        application/json:
          schema:
            type: object
            properties:
              name: { type: string, description: "New sandbox name (auto-generated if omitted)" }`,
			Responses: `
        "201":
          description: Sandbox created from snapshot
          content:
            application/json:
              schema:
                $ref: "#/components/schemas/Sandbox"`,
		},

		// Tasks
		{
			Method: "get", Path: "/tasks/{id}", OperationID: "getTask",
			Summary: "Get task status", Tags: []string{"Tasks"},
			Description: "Check the status of an async operation (e.g., image pull).",
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Task status
          content:
            application/json:
              schema:
                type: object
                properties:
                  id: { type: string }
                  status: { type: string, enum: [running, completed, failed] }
                  progress: { type: string }
                  error: { type: string }
                  result: { type: string }`,
		},
		{
			Method: "delete", Path: "/tasks/{id}", OperationID: "cancelTask",
			Summary: "Cancel a task", Tags: []string{"Tasks"},
			Parameters: `
      - name: id
        in: path
        required: true
        schema: { type: string }`,
			Responses: `
        "200":
          description: Task cancelled`,
		},
	}
}

const openAPIHeader = `openapi: "3.1.0"
info:
  title: bhatti API
  description: |
    bhatti creates isolated Linux VMs in seconds. Each sandbox has its own kernel,
    filesystem, and network. Paused sandboxes resume in under 3ms.

    All endpoints require ` + "`" + `Authorization: Bearer <token>` + "`" + ` except ` + "`" + `/health` + "`" + ` and ` + "`" + `/metrics` + "`" + `.
  version: "0.5"
  contact:
    name: Sahil Shubham
    url: https://bhatti.sh
  license:
    name: Source Available
    url: https://github.com/sahil-shubham/bhatti/blob/main/LICENSE

servers:
  - url: https://api.bhatti.sh
    description: Production
  - url: http://localhost:8080
    description: Local development

security:
  - bearerAuth: []

tags:
  - name: System
    description: Health checks and metrics (unauthenticated)
  - name: Sandboxes
    description: Create, manage, and destroy isolated Linux VMs
  - name: Execution
    description: Execute commands and open interactive shells
  - name: Files
    description: Read, write, and list files inside sandboxes
  - name: Networking
    description: Port discovery, reverse proxy, and public URLs
  - name: Templates
    description: Reusable sandbox configurations
  - name: Secrets
    description: Encrypted secret management (age encryption at rest)
  - name: Volumes
    description: Persistent ext4 volumes that survive sandbox destruction
  - name: Backups
    description: Volume backups to S3-compatible storage
  - name: Images
    description: Rootfs images from OCI registries, Docker, or sandbox snapshots
  - name: Snapshots
    description: Named VM snapshots (full memory + CPU + disk state)
  - name: Tasks
    description: Async operation tracking (image pulls)

`

const openAPIComponents = `
components:
  securitySchemes:
    bearerAuth:
      type: http
      scheme: bearer
      description: "API key (e.g., bht_abc123...)"

  schemas:
    Error:
      type: object
      properties:
        error: { type: string }
        request_id: { type: string }

    Sandbox:
      type: object
      properties:
        id: { type: string, example: "a1b2c3d4e5f6g7h8" }
        name: { type: string, example: "dev" }
        template_id: { type: string }
        engine_id: { type: string }
        status: { type: string, enum: [running, stopped, unknown, destroyed] }
        ip: { type: string, example: "192.168.137.2" }
        created_by: { type: string }
        created_at: { type: string, format: date-time }
        stopped_at: { type: string, format: date-time }
        keep_hot: { type: boolean }
        thermal: { type: string, enum: [hot, warm, cold] }
        urls: { type: array, items: { type: string } }

    Template:
      type: object
      properties:
        id: { type: string }
        name: { type: string }
        engine: { type: string, default: "firecracker" }
        image: { type: string }
        cpus: { type: number, default: 1 }
        memory_mb: { type: integer, default: 2048 }
        labels: { type: object, additionalProperties: { type: string } }
        user_data: { type: string }
        secrets: { type: array, items: { type: string } }
        created_at: { type: string, format: date-time }

    Volume:
      type: object
      properties:
        id: { type: string }
        name: { type: string, example: "workspace" }
        size_mb: { type: integer, example: 5120 }
        status: { type: string, enum: [creating, ready] }
        created_at: { type: string, format: date-time }
        attachments:
          type: array
          items:
            type: object
            properties:
              sandbox_id: { type: string }
              mount: { type: string }
              read_only: { type: boolean }

    VolumeBackup:
      type: object
      properties:
        id: { type: string }
        volume_name: { type: string }
        s3_key: { type: string }
        size_bytes: { type: integer }
        created_at: { type: string, format: date-time }

    Image:
      type: object
      properties:
        id: { type: string }
        name: { type: string, example: "python-3.12" }
        source: { type: string, example: "oci:python:3.12" }
        size_mb: { type: integer }
        oci_digest: { type: string }
        created_at: { type: string, format: date-time }

    Snapshot:
      type: object
      properties:
        id: { type: string }
        name: { type: string, example: "dev-ready" }
        source_sandbox: { type: string }
        size_mb: { type: integer }
        created_at: { type: string, format: date-time }

    PortInfo:
      type: object
      properties:
        sandbox_id: { type: string }
        container_port: { type: integer, example: 3000 }
        proxy_url: { type: string, example: "/sandboxes/abc123/proxy/3000/" }
`

func escapeYAML(s string) string {
	s = strings.ReplaceAll(s, `"`, `\"`)
	s = strings.ReplaceAll(s, "\n", " ")
	return s
}

// reindent takes a YAML fragment (with leading newline and base indentation)
// and re-indents it to the target column. It strips the common leading whitespace
// and replaces it with targetIndent spaces.
func reindent(fragment string, targetIndent int) string {
	lines := strings.Split(fragment, "\n")

	// Find minimum indentation of non-empty lines
	minIndent := 999
	for _, line := range lines {
		if strings.TrimSpace(line) == "" {
			continue
		}
		spaces := len(line) - len(strings.TrimLeft(line, " "))
		if spaces < minIndent {
			minIndent = spaces
		}
	}
	if minIndent == 999 {
		minIndent = 0
	}

	pad := strings.Repeat(" ", targetIndent)
	var result []string
	for _, line := range lines {
		if strings.TrimSpace(line) == "" {
			result = append(result, "")
		} else {
			trimmed := line[min(minIndent, len(line)):]
			result = append(result, pad+trimmed)
		}
	}
	return strings.Join(result, "\n")
}

func renderOpenAPI(groups []PathGroup) error {
	w := os.Stdout

	// Write header
	fmt.Fprint(w, openAPIHeader)

	// Write paths
	fmt.Fprintln(w, "paths:")
	for _, g := range groups {
		fmt.Fprintf(w, "  %s:\n", g.Path)
		for _, r := range g.Routes {
			fmt.Fprintf(w, "    %s:\n", r.Method)
			fmt.Fprintf(w, "      operationId: %s\n", r.OperationID)
			fmt.Fprintf(w, "      summary: \"%s\"\n", r.Summary)
			if r.Description != "" {
				fmt.Fprintf(w, "      description: \"%s\"\n", escapeYAML(r.Description))
			}
			fmt.Fprintln(w, "      tags:")
			for _, t := range r.Tags {
				fmt.Fprintf(w, "        - %s\n", t)
			}
			if r.Parameters != "" {
				fmt.Fprintln(w, "      parameters:")
				fmt.Fprintln(w, reindent(r.Parameters, 8))
			}
			if r.RequestBody != "" {
				fmt.Fprintln(w, "      requestBody:")
				fmt.Fprintln(w, reindent(r.RequestBody, 8))
			}
			fmt.Fprintln(w, "      responses:")
			fmt.Fprintln(w, reindent(r.Responses, 8))
		}
	}

	// Write components
	fmt.Fprint(w, openAPIComponents)
	return nil
}
