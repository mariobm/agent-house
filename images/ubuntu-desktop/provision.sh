#!/bin/bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
# HTTPS also avoids hosts/networks that drop Ubuntu mirror HTTP downloads.
find /etc/apt/sources.list.d -type f -exec sed -i 's|http://archive.ubuntu.com|https://archive.ubuntu.com|g; s|http://security.ubuntu.com|https://security.ubuntu.com|g' {} +
apt-get -o Acquire::ForceIPv4=true -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 update
apt-get -o Acquire::ForceIPv4=true -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 install -y --no-install-recommends xfce4 xfce4-terminal tigervnc-standalone-server \
    dbus-x11 xauth x11-utils fonts-dejavu fonts-liberation adwaita-icon-theme gnupg
# Official Mozilla DEB repository; Ubuntu's Firefox package otherwise requires Snap.
install -d -m755 /etc/apt/keyrings
curl -fsSL https://packages.mozilla.org/apt/repo-signing-key.gpg -o /etc/apt/keyrings/packages.mozilla.org.asc
fingerprint=$(gpg --show-keys --with-colons /etc/apt/keyrings/packages.mozilla.org.asc | awk -F: '$1=="fpr" {print $10; exit}')
[[ $fingerprint == 35BAA0B33E9EB396F59CA838C0BA5CE6DC6315A3 ]] || { echo 'Mozilla signing key mismatch' >&2; exit 1; }
printf 'deb [signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main\n' > /etc/apt/sources.list.d/mozilla.list
printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n\nPackage: firefox\nPin: release o=Ubuntu\nPin-Priority: -1\n' > /etc/apt/preferences.d/mozilla
apt-get -o Acquire::ForceIPv4=true -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 update
apt-get -o Acquire::ForceIPv4=true -o Acquire::http::Timeout=30 -o Acquire::https::Timeout=30 install -y --no-install-recommends firefox
mkdir -p /home/developer/.config/autostart /usr/local/share/ahvm
cat > /home/developer/.config/autostart/terminal.desktop <<'EOF'
[Desktop Entry]
Type=Application
Name=Terminal
Exec=xfce4-terminal --working-directory=/workspace --command=/bin/bash
EOF
chown -R developer:developer /home/developer/.config
printf 'ubuntu-desktop\n' > /usr/local/share/ahvm/image-profile
dpkg-query -W > /usr/local/share/ahvm/ubuntu-packages.tsv
apt-get clean
rm -rf /var/lib/apt/lists/* /root/.gnupg /tmp/* /var/tmp/*
find /var/log -type f -exec truncate -s 0 {} +
