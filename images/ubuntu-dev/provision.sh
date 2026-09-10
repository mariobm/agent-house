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
useradd -m -s /bin/bash -U developer
printf 'developer ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/developer
chmod 440 /etc/sudoers.d/developer
mkdir -p /workspace /opt/ahvm-tools /usr/local/share/ahvm
chown developer:developer /workspace /opt/ahvm-tools
# Use npm's integrity-checked packages and record the full resolved dependency
# lock in the image. Build scripts run as the guest developer, not host root.
python3 - "$BUN_VERSION" "$CLAUDE_VERSION" "$CODEX_VERSION" "$OPENCODE_VERSION" "$PI_VERSION" <<'PY'
import json,sys
names=['bun','@anthropic-ai/claude-code','@openai/codex','opencode-ai','@earendil-works/pi-coding-agent']
with open('/opt/ahvm-tools/package.json','w') as f:
    json.dump({'name':'ahvm-dev-tools','private':True,'dependencies':dict(zip(names,sys.argv[1:]))},f,indent=2)
PY
chown developer:developer /opt/ahvm-tools/package.json
runuser -u developer -- bash -c 'cd /opt/ahvm-tools && npm install --no-audit --no-fund'
for tool in bun bunx claude codex opencode pi; do
    test -x "/opt/ahvm-tools/node_modules/.bin/$tool"
    ln -s "/opt/ahvm-tools/node_modules/.bin/$tool" "/usr/local/bin/$tool"
done
chown -R root:root /opt/ahvm-tools
ln -s /usr/bin/fdfind /usr/local/bin/fd
cat > /usr/local/bin/ahvm-dev <<'DEV'
#!/bin/sh
set -eu
cd /workspace
if [ "$#" -eq 0 ]; then
    exec sudo -iu developer
fi
# sudo -i rebuilds the command through a shell and changes newline arguments.
# Preserve argv for automated commands (including multiline bash -c scripts).
exec sudo -H -u developer -- "$@"
DEV
chmod 755 /usr/local/bin/ahvm-dev
printf '\nexport LANG=C.UTF-8\ncd /workspace\n' >> /home/developer/.profile
cp /tmp/versions.env /usr/local/share/ahvm/image-versions.env
dpkg-query -W > /usr/local/share/ahvm/ubuntu-packages.tsv
sha256sum /usr/local/bin/ahvm-forge > /usr/local/share/ahvm/forge.sha256
printf '127.0.0.1 localhost\n127.0.1.1 ahvm\n::1 localhost ip6-localhost\n' > /etc/hosts
printf 'ahvm\n' > /etc/hostname
# No SSH server, background updater or account credentials in the template.
apt-get clean
rm -rf /var/lib/apt/lists/* /home/developer/.npm /root/.npm /tmp/* /var/tmp/*
find /var/log -type f -exec truncate -s 0 {} +
truncate -s 0 /etc/machine-id
