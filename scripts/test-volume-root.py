#!/usr/bin/env python3
"""Build without elevation, then run root-only volume tests in a separate process.

No VMs, NBD attachments, cloud credentials or object-store access are required.
"""
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    if sys.platform != "linux":
        raise SystemExit("Root volume tests require Linux")
    root = Path(__file__).resolve().parent.parent
    build = subprocess.run(
        ["cargo", "test", "--manifest-path", str(root / "rust/Cargo.toml"),
         "--locked", "-p", "ahvm-volume", "--lib", "--no-run",
         "--message-format=json"],
        cwd=root, check=True, stdout=subprocess.PIPE, text=True,
    )
    binaries = []
    for line in build.stdout.splitlines():
        item = json.loads(line)
        if (item.get("reason") == "compiler-artifact"
                and item.get("profile", {}).get("test")
                and item.get("target", {}).get("name") == "ahvm_volume"
                and item.get("executable")):
            binaries.append(item["executable"])
    if len(binaries) != 1:
        raise SystemExit("Expected exactly one ahvm-volume unit-test binary")
    selection = [binaries[0], "--ignored", "service::tests::"]
    listing = subprocess.check_output([*selection, "--list"], text=True)
    if not any(line.startswith("service::tests::") and line.endswith(": test")
               for line in listing.splitlines()):
        raise SystemExit("No privileged volume tests found")
    command = selection if os.geteuid() == 0 else ["sudo", "--", *selection]
    subprocess.run(command, cwd=root, check=True)


if __name__ == "__main__":
    main()
