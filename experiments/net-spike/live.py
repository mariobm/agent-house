#!/usr/bin/env python3
"""Run only on a disposable guest image. Child processes are always reaped."""
import argparse
import base64
import json
import os
from pathlib import Path
import socket
import shutil
import struct
import subprocess
import time


def exact(sock, size):
    result = bytearray()
    while len(result) < size:
        data = sock.recv(size - len(result))
        if not data:
            raise RuntimeError('forge closed connection')
        result.extend(data)
    return bytes(result)


def execute(path, command, timeout=180):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(timeout)
        sock.connect(str(path))
        body = b'\x20' + json.dumps({'argv': ['/bin/sh', '-c', command]}).encode()
        sock.sendall(struct.pack('>I', len(body)) + body)
        size, = struct.unpack('>I', exact(sock, 4))
        if not 1 <= size <= 1024 * 1024:
            raise RuntimeError(f'invalid response size: {size}')
        response = exact(sock, size)
        if response[0] != 0x24:
            raise RuntimeError(f'forge error: {response!r}')
        return json.loads(response[1:])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('work', type=Path)
    parser.add_argument('--vmm', required=True)
    parser.add_argument('--image', required=True, type=Path)
    parser.add_argument('--prototype', required=True)
    parser.add_argument('--resolver', required=True)
    parser.add_argument('--host-ip', required=True)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=True)
    # Exclusive run marker prevents socket cleanup from touching another run.
    marker = work / 'running'
    with marker.open('x') as f:
        f.write(str(os.getpid()))
    if args.image.resolve() == work / 'guest.ext4':
        marker.unlink()
        raise ValueError('base image must differ from disposable guest.ext4')
    children = []
    logs = []
    results = []
    try:
        shutil.copyfile(args.image, work / 'guest.ext4')
        for name in ('net.sock', 'c.sock', 'f.sock', 'k.sock'):
            (work / name).unlink(missing_ok=True)
        log = (work / 'netd.log').open('w')
        logs.append(log)
        netd = subprocess.Popen([args.prototype, str(work / 'net.sock'), args.resolver, args.host_ip], stdout=log, stderr=log)
        children.append(netd)
        deadline = time.monotonic() + 10
        while not (work / 'net.sock').exists():
            if netd.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('prototype failed to listen')
            time.sleep(.05)
        spec = dict(vcpus=1, mem_mib=512, log_level=3,
                    root_disk=str(work / 'guest.ext4'), root_disk_format='raw',
                    pid1=True, exec_path='/init.krun',
                    net_uds=str(work / 'net.sock'), net_mac='02:00:00:00:00:02',
                    vsock_control_uds=str(work / 'c.sock'),
                    vsock_forward_uds=str(work / 'f.sock'),
                    control_socket_uds=str(work / 'k.sock'), env=[])
        (work / 'spec.json').write_text(json.dumps(spec))
        log = (work / 'vmm.log').open('w')
        logs.append(log)
        env = dict(PATH='/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',
                   HOME='/root', LANG='C.UTF-8', LD_LIBRARY_PATH='/usr/local/lib64')
        vm = subprocess.Popen([args.vmm, str(work / 'spec.json')], env=env, stdout=log, stderr=log, stdin=subprocess.DEVNULL)
        children.append(vm)
        deadline = time.monotonic() + 45
        while True:
            if vm.poll() is not None or netd.poll() is not None:
                raise RuntimeError('worker or prototype exited during readiness')
            try:
                if execute(work / 'c.sock', 'true', timeout=2)['exit_code'] == 0:
                    break
            except (OSError, RuntimeError):
                pass
            if time.monotonic() > deadline:
                raise RuntimeError('guest not ready')
            time.sleep(.1)
        commands = [
            ('network', 'mkdir -p /proc /sys /dev; mount -t proc proc /proc 2>/dev/null || true; mount -t sysfs sysfs /sys 2>/dev/null || true; mount -t devtmpfs devtmpfs /dev 2>/dev/null || true; ip link set lo up; ip link set eth0 up; ip addr add 100.64.0.2/24 dev eth0; ip route add default via 100.64.0.1; printf "nameserver 100.64.0.1\\n" > /etc/resolv.conf; ip addr show eth0'),
            ('dns', 'nslookup dl-cdn.alpinelinux.org'),
            ('packages', 'apk update && apk add git curl ca-certificates ethtool'),
            ('offloads', 'ethtool -k eth0'),
            ('https', 'curl -4 --fail --max-time 30 https://api.github.com/repos/octocat/Hello-World -o /tmp/api.json && test -s /tmp/api.json && head -c 160 /tmp/api.json'),
            ('clone', 'dest=$(mktemp -d /workspace/clone.XXXXXX) && git clone --depth 1 https://github.com/octocat/Hello-World.git "$dest/repo" && test -s "$dest/repo/README" && cat "$dest/repo/README"'),
            ('blocked-metadata', 'curl -4 --fail --connect-timeout 2 --max-time 3 http://169.254.169.254/; test $? -eq 28'),
            ('blocked-host', f'curl -4 --fail --connect-timeout 2 --max-time 3 http://{args.host_ip.split(",")[0]}/; test $? -eq 28'),
        ]
        for label, command in commands:
            start = time.monotonic()
            result = execute(work / 'c.sock', command)
            for key in ("stdout", "stderr"):
                result[key] = base64.b64decode(result.pop(key + "_b64", "")).decode(errors="replace")
            result.update(label=label, seconds=round(time.monotonic()-start,3))
            results.append(result)
            print(json.dumps(result), flush=True)
            if result['exit_code'] != 0:
                raise RuntimeError(f'{label} failed')
    finally:
        if len(children) == 2 and children[-1].poll() is None:
            try:
                execute(work / 'c.sock', 'sync', timeout=5)
            except Exception:
                pass
        for process in reversed(children):
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for log in logs:
            log.close()
        (work / 'results.json').write_text(json.dumps(results, indent=2))
        marker.unlink(missing_ok=True)


if __name__ == '__main__':
    main()
