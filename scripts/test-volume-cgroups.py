#!/usr/bin/env python3
"""Opt-in Linux/systemd cgroup qualification; no VMs, NBD or cloud access."""
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

if sys.platform != "linux" or os.geteuid() != 0:
    raise SystemExit("Run on a Linux systemd host as root; requires systemd 254+.")
root = Path(__file__).resolve().parent.parent
build = subprocess.check_output([
    "cargo", "test", "--manifest-path", str(root / "rust/Cargo.toml"),
    "--locked", "-p", "ahvm-volume", "--lib", "--no-run", "--message-format=json",
], cwd=root, text=True)
binaries = [entry["executable"] for line in build.splitlines()
            if (entry := json.loads(line)).get("reason") == "compiler-artifact"
            and entry.get("target", {}).get("name") == "ahvm_volume"
            and entry.get("executable")]
if len(binaries) != 1:
    raise SystemExit("Expected one volume test binary")
suffix = uuid.uuid4().hex[:12]
workers = "ahvm-test-volume-workers-" + suffix
supervisor = "ahvm-test-volume-supervisor-" + suffix
try:
    subprocess.run([
        "systemd-run", "--quiet", "--unit", workers,
        "-p", "Delegate=cpu memory pids", "-p", "DelegateSubgroup=keeper",
        "-p", "MemoryMax=2G", "-p", "MemorySwapMax=0",
        "-p", "CPUQuota=200%", "-p", "TasksMax=1024", "/bin/sleep", "infinity",
    ], check=True)
    group = subprocess.check_output([
        "systemctl", "show", workers, "-p", "ControlGroup", "--value",
    ], text=True).strip()
    if not group.startswith("/") or group == "/":
        raise RuntimeError("Invalid qualification group")
    subprocess.run([
        "systemd-run", "--wait", "--pipe", "--collect", "--unit", supervisor,
        "-p", "MemoryMax=1G", "-p", "CPUQuota=100%", "-p", "TasksMax=256",
        "/usr/bin/env", "AHVM_TEST_VOLUME_CGROUP=/sys/fs/cgroup" + group,
        binaries[0], "--ignored", "--exact",
        "service::resources::tests::delegated_worker_launch_membership_and_cleanup",
    ], check=True)
finally:
    subprocess.run(["systemctl", "stop", workers], check=False)
    subprocess.run(["systemctl", "reset-failed", workers, supervisor],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
