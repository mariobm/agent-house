# AHVM Cloud login

Cloud accounts are currently invitation-only. This milestone connects your CLI to
your account; hosted VM creation, shells and files are not enabled yet. Self-hosted
SSH connections continue to work as before.

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
