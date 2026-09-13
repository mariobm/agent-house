#!/usr/bin/env python3
"""Test-only hook; environment points exclusively to this gate's private root."""
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time

spec = importlib.util.spec_from_file_location('adapter', Path(__file__).with_name('engine-service.py'))
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)
root = Path(os.environ['AHVM_SERVICE_ROOT'])
config = json.loads((root/'gate.json').read_text())
record = json.loads((root/'record.json').read_text())
op = sys.argv[1]
if op == 'restart':
    saved = config['supervisor']
    assert adapter.alive(saved)
    fd = os.pidfd_open(saved['pid'])
    try:
        assert adapter.alive(saved)
        signal.pidfd_send_signal(fd, signal.SIGKILL)
    finally:
        os.close(fd)
    deadline = time.monotonic()+10
    while adapter.alive(saved):
        assert time.monotonic()<deadline
        time.sleep(.05)
    with (root/'service.log').open('ab') as log:
        process = subprocess.Popen(config['command'], stdout=log, stderr=log, start_new_session=True)
    config['supervisor'] = adapter.identity(process.pid)
    (root/'gate.json').write_text(json.dumps(config))
    deadline = time.monotonic()+10
    while True:
        try:
            with socket.socket(socket.AF_UNIX) as s:
                s.connect(str(root/'service.sock'))
                peer = s.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
                import struct
                if struct.unpack('3i', peer)[0] == process.pid:
                    s.sendall(json.dumps(dict(version=1, volume_id=record['volume_id'], operation='inspect')).encode()+b'\n')
                    assert json.loads(s.recv(4096))['ok']
                    break
        except (ConnectionRefusedError, FileNotFoundError): pass
        assert time.monotonic()<deadline and process.poll() is None
        time.sleep(.05)
elif op == 'kill-storage':
    saved = record['worker']
    fd = os.pidfd_open(saved['pid'])
    try:
        assert adapter.alive(saved)
        signal.pidfd_send_signal(fd, signal.SIGKILL)
    finally: os.close(fd)
    deadline = time.monotonic()+10
    while adapter.alive(saved):
        assert time.monotonic()<deadline
        time.sleep(.05)
else:
    assert op in ['busy-detach', 'busy-attach']
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(10)
        s.connect(str(root/'service.sock'))
        s.sendall(json.dumps(dict(version=1, volume_id=record['volume_id'], operation=op[5:])).encode()+b'\n')
        response = json.loads(s.recv(4096))
        assert response['ok'] is False, 'must refuse while VM holds disk'
print('PASS', op, flush=True)
