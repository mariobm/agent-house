#!/bin/bash
# Install the bhatti CLI binary for remote usage.
# Usage: curl -fsSL https://bhatti.sh/install.sh | bash
set -euo pipefail

VERSION="${BHATTI_VERSION:-latest}"
REPO="sahil-shubham/bhatti"

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)        ARCH="amd64" ;;
    aarch64|arm64) ARCH="arm64" ;;
    *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

if [ "$VERSION" = "latest" ]; then
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep tag_name | cut -d'"' -f4)
fi

BINARY="bhatti-${OS}-${ARCH}"
URL="https://github.com/$REPO/releases/download/${VERSION}/${BINARY}"

echo "Installing bhatti $VERSION ($OS/$ARCH)..."
curl -fsSL "$URL" -o /tmp/bhatti
chmod +x /tmp/bhatti

INSTALL_DIR="/usr/local/bin"
if [ -w "$INSTALL_DIR" ]; then
    mv /tmp/bhatti "$INSTALL_DIR/bhatti"
else
    sudo mv /tmp/bhatti "$INSTALL_DIR/bhatti"
fi

echo "bhatti $VERSION installed to $INSTALL_DIR/bhatti"
echo ""
echo "Quick start:"
echo "  bhatti setup"
