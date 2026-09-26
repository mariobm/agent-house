"""Configure AHVM's packaged desktops before opening their private VNC stream."""
import fcntl
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import time


def run(argv, **kwargs):
    return subprocess.check_output(argv, text=True, timeout=20, **kwargs)


def configure(resolution):
    modes = {'720p': (1280, 720), '1080p': (1920, 1080)}
    width, height = modes[resolution]
    mode = f'{width}x{height}'
    # Serialize resolution changes by concurrent viewers within this VM.
    with open('/run/ahvm-desktop-resolution.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if Path('/usr/local/bin/desktop-session').exists():
            instances = [p for p in Path('/run/user/1000/hypr').glob('*') if (p / '.socket.sock').exists()]
            if len(instances) != 1:
                raise RuntimeError('Hyprland is not ready')
            prefix = ['runuser', '-u', 'desktop', '--', 'env', 'XDG_RUNTIME_DIR=/run/user/1000',
                      'DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus',
                      'HYPRLAND_INSTANCE_SIGNATURE=' + instances[0].name]
            monitors = json.loads(run(prefix + ['hyprctl', '-j', 'monitors']))
            monitor = next(m for m in monitors if m['name'] == 'desktop')
            if (monitor['width'], monitor['height']) != (width, height):
                result = run(prefix + ['hyprctl', 'eval',
                    'hl.monitor({output="desktop",mode="' + mode + '@60",position="auto",scale=1})'])
                if result.strip() != 'ok':
                    raise RuntimeError('Hyprland refused resolution: ' + result)
                # WayVNC retains its old capture size. Restart only the transport,
                # leaving the compositor and all desktop applications running.
                services = ['ahvm-vnc.service', 'ahvm-desktop-relay.service']
                run(prefix + ['systemctl', '--user', 'stop'] + services)
                try:
                    Path('/run/user/1000/vnc.sock').unlink(missing_ok=True)
                finally:
                    run(prefix + ['systemctl', '--user', 'start'] + services)
            sock = '/run/user/1000/vnc.sock'
        elif Path('/usr/local/bin/ahvm-desktop-session').exists():
            run(['runuser', '-u', 'ahvm', '--', 'env', 'DISPLAY=:1',
                 'xrandr', '--output', 'VNC-0', '--mode', mode])
            sock = '/run/ahvm-desktop/vnc.sock'
        else:
            raise RuntimeError('resolution selection requires an AHVM desktop image')
        # Check actual VNC size, not just the compositor configuration.
        for attempt in range(20):
            try:
                with socket.socket(socket.AF_UNIX) as s:
                    s.settimeout(2)
                    s.connect(sock)
                    def read(n):
                        data = b''
                        while len(data) < n:
                            block = s.recv(n-len(data))
                            if not block:
                                raise OSError('VNC closed during handshake')
                            data += block
                        return data
                    if read(12) != b'RFB 003.008\n':
                        raise RuntimeError('unsupported VNC version')
                    s.sendall(b'RFB 003.008\n')
                    if 1 not in read(read(1)[0]):
                        raise RuntimeError('unexpected private VNC authentication')
                    s.sendall(b'\x01')
                    if read(4) != bytes(4):
                        raise RuntimeError('VNC authentication failed')
                    s.sendall(b'\x01')
                    actual = struct.unpack('!HH', read(4))
                    if actual != (width, height):
                        raise OSError(f'VNC is still {actual[0]}x{actual[1]}')
                print(mode)
                return
            except OSError:
                if attempt == 19:
                    raise
                time.sleep(.1)


if __name__ == '__main__':
    try:
        configure(sys.argv[1])
    except Exception as error:
        print(f'Cannot set desktop resolution: {error}', file=sys.stderr)
        sys.exit(1)
