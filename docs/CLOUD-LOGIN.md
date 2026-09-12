# AHVM Cloud login

Cloud accounts are invitation-only. Compute is enabled separately for selected
pilot workspaces. Self-hosted SSH connections continue to work as before.

With a CLI build that includes cloud login:

```sh
ahvm login
ahvm whoami
ahvm logout
```

Login opens your browser. Sign in with your invited GitHub account, select a workspace
and click **Approve CLI**. Only approve requests you started. The CLI binds an ephemeral
callback port on `127.0.0.1`, never a public interface. No firewall changes are needed.
`ahvm login --no-browser` prints the link for you to open on the same computer.

On a headless machine or over SSH, use a device code instead:

```sh
ahvm login --device --credential-store file
```

Visit the displayed dashboard URL on your laptop and enter the code from your terminal.
Check it matches before approving. Login expires after ten minutes if not completed.
This is AHVM's device flow; it does not require enabling GitHub's device-flow setting.

## Credentials and workspace access

On macOS, credentials use Keychain. On Linux desktops, they use the Secret Service
credential store. If that store is unavailable, unlock it or explicitly choose
`--credential-store file`. AHVM does not silently fall back to plaintext storage.

The file fallback stores credentials under `~/.config/ahvm/cloud/` (or
`$AHVM_CONFIG_DIR/cloud/`) with directory mode 700 and file mode 600. Do not share or
commit that directory. Credentials and browser approval codes are never printed by
`whoami`, including `--json` output. Ordinary host configuration remains separate.

A CLI login authorizes one workspace and does not grant platform administrator access.
The API checks current membership and suspension on every request. Access credentials
last 15 minutes; refresh credentials rotate automatically for up to 30 days from login.
After that, sign in again. Concurrent commands serialize credential updates. If a
refresh response is lost after the server commits it, a new login is required.

Logout first revokes the cloud login, then removes its local credentials. If the
network is unavailable, credentials stay locally so you can retry revocation. It does
not sign out your browser, change your SSH hosts/default host, or delete any VMs.
To switch cloud accounts or workspaces, log out and log in again.

`--endpoint`, `--token-file`, `AHVM_TOKEN` and `--host` still configure self-hosted
commands. They do not provide cloud authentication. Developers can point login at a
different HTTPS service with `--cloud-endpoint`; HTTP is accepted only for a local
`127.0.0.1` development service. The origin is stored with the cloud login.

## Hosted machines

The cloud compute commands require a CLI build containing `--cloud` (they are
not in the v0.2.5 login-only release). After your workspace is enabled:

```sh
ahvm --cloud create dev --cpus 2 --memory 4096
ahvm --cloud shell dev
# exit returns to your computer; the VM keeps running
ahvm --cloud files put dev ./hello.txt /workspace/hello.txt
ahvm --cloud stop dev
ahvm --cloud start dev
ahvm --cloud delete dev
```

`--cloud` explicitly selects your approved workspace, bypassing saved SSH host
selection. It conflicts with `--host`, `--endpoint` and `--token-file`, including
their environment variables. Omit it to keep using your normal self-hosted default.
The cloud credential lock is released before starting VM requests or a shell.

`ahvm --cloud list` lists your machines; `ahvm --cloud get dev` reads current status.
The dashboard at [dashboard.ahvm.app](https://dashboard.ahvm.app/) also shows state
and operations needing attention. Machine names are scoped to a workspace.

Lifecycle requests carry an idempotency key. If the response is interrupted or
pending, the CLI prints the key. Retry the same command with that key:

```sh
ahvm --cloud --idempotency-key <printed-key> create dev --cpus 2 --memory 4096
```

This reads the existing operation instead of creating another VM. Pending or
uncertain operations retain quota until the service confirms their outcome. Some
interrupted operations currently need operator recovery. Contact sales@ahvm.app
if one remains unresolved. HTTP 429 reports how long to wait; device login polling
backs off without restarting the approval flow.

The initial pilot supports the default development image, exec, sessions, files
and basic lifecycle operations. Desktop, previews, snapshots and private network
access are not available through the cloud API yet. Use sessions for long work:
a cloud HTTP response has a 90-second deadline, and a timed-out exec response does
not cancel the guest process. Uploads are subject to Cloudflare's request-size limit.
