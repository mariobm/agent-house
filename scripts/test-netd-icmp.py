#!/usr/bin/env python3
"""Opt-in Linux ICMP gate using a real netd, without booting any VMs.

Run as the daemon's service user: python3 scripts/test-netd-icmp.py /path/to/ahvm-netd
Requires ping_group_range permission and public echo access to 1.1.1.1.
"""
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time

GW = "100.64.0.1"
GUEST = "100.64.0.2"
PUBLIC = "1.1.1.1"
GUEST_MAC = bytes.fromhex("020000000002")
GATEWAY_MAC = bytes.fromhex("020000000001")


def checksum(data):
    data += b"\0" * (len(data) % 2)
    value = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while value >> 16:
        value = (value & 65535) + (value >> 16)
    return (~value) & 65535


def request(destination, sequence, payload):
    icmp = struct.pack("!BBHHH", 8, 0, 0, 0x1234, sequence) + payload
    icmp = icmp[:2] + struct.pack("!H", checksum(icmp)) + icmp[4:]
    ip = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(icmp), 0, 0, 64, 1, 0,
                     socket.inet_aton(GUEST), socket.inet_aton(destination))
    ip = ip[:10] + struct.pack("!H", checksum(ip)) + ip[12:]
    frame = GATEWAY_MAC + GUEST_MAC + b"\x08\x00" + ip + icmp
    return struct.pack("!I", len(frame)) + frame


def receive_exact(link, size):
    data = bytearray()
    while len(data) < size:
        part = link.recv(size - len(data))
        if not part:
            raise RuntimeError("gateway closed the guest link")
        data.extend(part)
    return bytes(data)


def receive_echo(link, destination, sequence, payload):
    size = struct.unpack("!I", receive_exact(link, 4))[0]
    assert 42 <= size <= 1514, size
    frame = receive_exact(link, size)
    assert frame[:6] == GUEST_MAC and frame[6:12] == GATEWAY_MAC
    ip, icmp = frame[14:34], frame[34:]
    assert checksum(ip) == 0 and checksum(icmp) == 0
    assert ip[12:16] == socket.inet_aton(destination)
    assert ip[16:20] == socket.inet_aton(GUEST)
    kind, code, _, ident, seq = struct.unpack("!BBHHH", icmp[:8])
    assert (kind, code, ident, seq) == (0, 0, 0x1234, sequence)
    assert icmp[8:] == payload


def main():
    binary = str(Path(sys.argv[1]).resolve(strict=True))
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
        route.connect((PUBLIC, 53))
        host_ip = route.getsockname()[0]
    with tempfile.TemporaryDirectory(prefix="ahvm-icmp-gate-") as directory:
        root = Path(directory)
        config = root / "net.json"
        config.write_text(json.dumps({"ethernet_contract": 1, "socket": str(root / "net.sock"),
                                     "resolver": "127.0.0.53", "private_access": []}))
        config.chmod(0o600)
        with (root / "netd.log").open("w+") as log:
            proc = subprocess.Popen([binary, str(config)], stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 5
                while not (root / "net.sock").exists():
                    assert proc.poll() is None and time.monotonic() < deadline
                    time.sleep(0.01)
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as link:
                    link.connect(str(root / "net.sock"))
                    link.settimeout(3)
                    payload = os.urandom(32)
                    for seq, destination in enumerate([GW, PUBLIC], 1):
                        link.sendall(request(destination, seq, payload))
                        receive_echo(link, destination, seq, payload)
                    # The host address really exists. Neither it nor private,
                    # metadata or shared-address space may produce echo replies.
                    for destination in [host_ip, "127.0.0.1", "169.254.169.254", "10.0.0.1", GUEST]:
                        link.sendall(request(destination, 3, payload))
                    link.settimeout(0.3)
                    try:
                        unexpected = link.recv(1)
                    except TimeoutError:
                        pass
                    else:
                        raise AssertionError(f"denied ping produced data/closed link: {unexpected!r}")
                    # Still usable after denial; also check the largest supported frame.
                    link.settimeout(3)
                    large = os.urandom(1472)
                    link.sendall(request(PUBLIC, 4, large))
                    receive_echo(link, PUBLIC, 4, large)
                print("PASS: real public echo, gateway echo, private/host denial, MTU-sized echo")
            except BaseException:
                log.flush()
                log.seek(0)
                print(log.read(), file=sys.stderr)
                raise
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()


if __name__ == "__main__":
    main()
