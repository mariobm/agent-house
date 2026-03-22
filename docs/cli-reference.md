# CLI Reference

`bhatti` is a single binary — `bhatti serve` starts the daemon, everything else is a CLI command that talks to the daemon's HTTP API.

All commands accept sandbox name or ID interchangeably.

## Configuration

```bash
export BHATTI_URL=http://localhost:8080   # API endpoint
export BHATTI_TOKEN=your-auth-token       # auth token
```

Or configure via `~/.bhatti/config.yaml`:

```yaml
auth_token: your-auth-token
listen: :8080
```

The install script creates this file automatically.

## Commands

### serve

Start the daemon.

```bash
sudo bhatti serve
```

Reads config from (first match): `$BHATTI_CONFIG`, `./config.yaml`, `~/.bhatti/config.yaml`.

### create

Create a new sandbox.

```bash
bhatti create --name dev --cpus 2 --memory 1024
bhatti create --name worker --env API_KEY=sk-abc,NODE_ENV=prod
bhatti create --name builder --init "npm install && npm run build"
```

| Flag | Default | Description |
|------|---------|-------------|
| `--name` | auto-generated | Sandbox name |
| `--cpus` | 1 | Number of vCPUs |
| `--memory` | 512 | Memory in MB |
| `--env` | — | Comma-separated KEY=VALUE pairs |
| `--init` | — | Init script (runs as TTY session "init") |

Output: `ID  NAME  IP`

### list / ls

```bash
bhatti list
```

```
ID                   NAME                 STATUS     IP
a1b2c3d4e5f6         dev                  running    192.168.137.2
f7e8d9c0b1a2         worker               stopped    192.168.137.3
```

### destroy / rm

```bash
bhatti destroy dev
bhatti rm a1b2c3d4e5f6
```

### exec

```bash
bhatti exec dev -- echo hello
bhatti exec dev -- npm install
bhatti exec worker -- sh -c 'echo $API_KEY'
```

Everything after `--` is the command. Exit code is forwarded — `bhatti exec dev -- false` exits with code 1.

Stdout goes to stdout, stderr goes to stderr. Suitable for piping:

```bash
bhatti exec dev -- cat /workspace/data.json | jq .name
```

### shell / sh

```bash
bhatti shell dev
```

Opens an interactive terminal inside the sandbox. Full zsh with syntax highlighting and starship prompt.

- Terminal size is synced automatically (SIGWINCH → resize)
- `Ctrl+\` to detach (shell keeps running inside the VM)
- The shell session survives network disconnects

### ps

```bash
bhatti ps dev
```

```
ID         COMMAND                                  RUNNING  ATTACHED
init       npm install                              true     false
s1         /bin/zsh -li                             true     true
```

### file

```bash
# Read a file to stdout
bhatti file read dev /workspace/app.js

# Write a file from stdin
echo 'console.log("hello")' | bhatti file write dev /workspace/app.js
cat local-file.tar.gz | bhatti file write dev /workspace/archive.tar.gz

# List a directory
bhatti file ls dev /workspace/
```

```
d0755     4096 node_modules
-0644     1234 app.js
-0644      567 package.json
```

### secret

```bash
bhatti secret set API_KEY sk-abc123def
bhatti secret list
bhatti secret delete API_KEY
```

Secrets are encrypted at rest with age. They're injected into sandboxes via the config drive.
