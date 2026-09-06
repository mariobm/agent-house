#!/usr/bin/env bash
# Enforce the forge guest-agent budgets: static musl binary under 8 MiB.
# Runs on Linux (server + CI). Usage: rust/check-forge-size.sh
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
BUDGET=$((8 << 20))
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release -p ahvm-forge --target x86_64-unknown-linux-musl
BIN=target/x86_64-unknown-linux-musl/release/ahvm-forge
SIZE=$(stat -c%s "$BIN")
if ldd "$BIN" 2>&1 | grep -qv "statically linked"; then
  echo "FAIL: $BIN is not statically linked"
  exit 1
fi
echo "forge static binary: $SIZE bytes (budget $BUDGET)"
if [ "$SIZE" -gt "$BUDGET" ]; then
  echo "FAIL: over budget"
  exit 1
fi
echo OK
