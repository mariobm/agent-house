#!/bin/bash
set -euo pipefail
source /tmp/versions.env
printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
chmod 755 /usr/sbin/policy-rc.d
apt-get update
apt-get upgrade -y
apt-get install -y --no-install-recommends ca-certificates curl wget git openssh-client \
    python3 python3-pip python3-venv python-is-python3 build-essential pkg-config \
    bash bash-completion sudo locales tzdata iproute2 iputils-ping dnsutils \
    procps psmisc util-linux ripgrep fd-find jq less nano vim-tiny tmux unzip zip xz-utils sqlite3 file rsync
useradd -m -s /bin/bash -U ahvm
printf 'ahvm ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/ahvm
chmod 440 /etc/sudoers.d/ahvm
mkdir -p /workspace /opt/ahvm-tools /usr/local/share/ahvm
chown ahvm:ahvm /workspace /opt/ahvm-tools
# Agent uploads inherit the workspace group and stay editable by ahvm.
chmod 2775 /workspace
# Use npm's integrity-checked packages and record the full resolved dependency
# lock in the image. Build scripts run as the guest ahvm, not host root.
python3 - "$BUN_VERSION" "$CLAUDE_VERSION" "$CODEX_VERSION" "$OPENCODE_VERSION" "$PI_VERSION" <<'PY'
import json,sys
names=['bun','@anthropic-ai/claude-code','@openai/codex','@opencode/cli','@earendil-works/pi-coding-agent']
with open('/opt/ahvm-tools/package.json','w') as f:
    json.dump({'name':'ahvm-dev-tools','private':True,'dependencies':dict(zip(names,sys.argv[1:]))},f,indent=2)
PY
chown ahvm:ahvm /opt/ahvm-tools/package.json
runuser -u ahvm -- bash -c 'cd /opt/ahvm-tools && npm install --no-audit --no-fund'
for tool in bun bunx claude codex opencode pi; do
    test -x "/opt/ahvm-tools/node_modules/.bin/$tool"
    ln -s "/opt/ahvm-tools/node_modules/.bin/$tool" "/usr/local/bin/$tool"
done
chown -R root:root /opt/ahvm-tools
# Pi Durable is an opt-in application runtime, separate from the Pi CLI and
# OpenCode. Install its reviewed dependency lock without package build scripts.
python3 - "$PI_DURABLE_VERSION" <<'PY'
import json,sys
package=json.load(open('/opt/ahvm-pi-durable/package.json'))
assert set(package['dependencies']) == {'@earendil-works/pi-durable','@earendil-works/pi-ai','@earendil-works/chord'}
assert all(version == sys.argv[1] for version in package['dependencies'].values()), 'Pi runtime pin mismatch'
PY
chown -R ahvm:ahvm /opt/ahvm-pi-durable
runuser -u ahvm -- bash -c 'cd /opt/ahvm-pi-durable && npm ci --omit=dev --ignore-scripts --no-audit --no-fund'
chown -R root:root /opt/ahvm-pi-durable
chmod 755 /opt/ahvm-pi-durable/tool.py
ln -s /opt/ahvm-pi-durable/tool.py /usr/local/bin/ahvm-pi-tool
python3 - "$NODE_VERSION" <<'PY'
import hashlib,json,sys
from pathlib import Path
root=Path('/opt/ahvm-pi-durable')
package=json.loads((root/'package.json').read_text())
manifest={'schema':1,'harness':'pi-durable','experimental':True,
          'module_root':str(root/'node_modules'),'node_minimum':'22.19.0',
          'node_version':sys.argv[1],'packages':package['dependencies'],
          'tool_scope_helper':'/usr/local/bin/ahvm-pi-tool','tool_scope_protocol':1,
          'package_lock_sha256':hashlib.sha256((root/'package-lock.json').read_bytes()).hexdigest()}
Path('/usr/local/share/ahvm/pi-durable-runtime.json').write_text(json.dumps(manifest,indent=2)+'\n')
PY
cat > /usr/local/bin/ahvm-pi-durable-check <<'CHECK'
#!/bin/sh
set -eu
exec /usr/local/bin/node /opt/ahvm-pi-durable/check.mjs "$@"
CHECK
chmod 755 /usr/local/bin/ahvm-pi-durable-check
runuser -u ahvm -- /usr/local/bin/ahvm-pi-durable-check
ln -s /usr/bin/fdfind /usr/local/bin/fd
cat > /usr/local/bin/ahvm-dev <<'DEV'
#!/bin/sh
set -eu
cd /workspace
if [ "$#" -eq 0 ]; then
    exec sudo -iu ahvm
fi
# sudo -i rebuilds the command through a shell and changes newline arguments.
# Preserve argv for automated commands (including multiline bash -c scripts).
exec sudo -H -u ahvm -- "$@"
DEV
chmod 755 /usr/local/bin/ahvm-dev
# Default interactive entry point. Forge retains root for network/file operations.
cat > /usr/local/bin/ahvm-shell <<'SHELL'
#!/bin/sh
set -eu
cd /workspace
exec sudo -H -u ahvm -- env USER=ahvm LOGNAME=ahvm /bin/bash -i
SHELL
chmod 755 /usr/local/bin/ahvm-shell
cat >> /home/ahvm/.bashrc <<'BASHRC'

# Builtins only: no subprocesses on every prompt. Bracket escapes for Readline.
case "$TERM" in
  dumb|'') PS1='\u@\h:\w\$ ' ;;
  *) PS1='\[\e[38;5;150m\]\u@\h\[\e[0m\]:\[\e[38;5;110m\]\w\[\e[0m\]\$ ' ;;
esac
BASHRC

printf '\nexport LANG=C.UTF-8\ncd /workspace\n' >> /home/ahvm/.profile
cp /tmp/versions.env /usr/local/share/ahvm/image-versions.env
dpkg-query -W > /usr/local/share/ahvm/ubuntu-packages.tsv
sha256sum /usr/local/bin/ahvm-forge > /usr/local/share/ahvm/forge.sha256
printf '127.0.0.1 localhost\n127.0.1.1 ahvm\n::1 localhost ip6-localhost\n' > /etc/hosts
printf 'ahvm\n' > /etc/hostname
# No SSH server, background updater or account credentials in the template.
apt-get clean
rm -rf /var/lib/apt/lists/* /home/ahvm/.npm /root/.npm /tmp/* /var/tmp/*
find /var/log -type f -exec truncate -s 0 {} +
truncate -s 0 /etc/machine-id
