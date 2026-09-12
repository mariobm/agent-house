#!/usr/bin/env python3
"""Single-volume, root-only qualification adapter for the engine's Unix protocol.
Not an installed daemon: no fencing, multi-client scheduling or automatic restart.
Use a fresh private directory and unused NBD device. R2 fixtures are retained for
explicit prefix cleanup, including after logical delete. Stop the test VM first.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--root', required=True)
p.add_argument('--config', required=True)
p.add_argument('--server', required=True)
p.add_argument('--device', default='/dev/nbd0')
a = p.parse_args()
assert os.geteuid() == 0
assert re.fullmatch(r'/dev/nbd[0-9]+', a.device)
assert Path(a.device).is_block_device() and not Path(a.device).is_symlink()
lock = open('/run/lock/ahvm-volume-' + Path(a.device).name + '.lock', 'w')
fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
assert not Path('/sys/block', Path(a.device).name, 'pid').exists()
root = Path(a.root)
root.mkdir(mode=0o700)  # refuse existing roots: no unsafe service adoption
os.umask(0o077)
process = None
attached = False
volume = None
last_status = None
prepared = False
deleted = False

def run(argv, timeout=300):
    subprocess.run(argv, check=True, timeout=timeout)

def persist():
    temp = root/'record.tmp'
    with temp.open('w') as f:
        json.dump(dict(volume_id=volume, prepared=prepared, deleted=deleted, status=last_status), f)
        f.flush(); os.fsync(f.fileno())
    temp.replace(root/'record.json')
    fd = os.open(root, os.O_RDONLY)
    try: os.fsync(fd)
    finally: os.close(fd)

def control(op):
    assert process is not None and process.poll() is None
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(300 if op == 'sync' else 2)
        s.connect(str(root/'disk.control'))
        s.sendall(op.encode()+b'\n')
        data = b''
        while True:
            part = s.recv(4096)
            if not part: break
            data += part
            assert len(data) <= 4096
    status = json.loads(data)
    assert 'error' not in status
    return status

def detach():
    global process, attached
    if attached:
        run(['nbd-client','-d',a.device]); attached = False
    if process is not None:
        if process.poll() is None: process.terminate()
        try: process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill(); process.wait(timeout=10)
        process = None

def attach():
    global process, attached
    if process is not None:
        assert process.poll() is None and attached, 'sidecar failure requires explicit test cleanup'
        return
    for name in ['disk.sock','disk.control']:
        (root/name).unlink(missing_ok=True)
    (root/'journal').mkdir(mode=0o700,exist_ok=True)
    with (root/'storage.log').open('ab',buffering=0) as log:
        process = subprocess.Popen([a.server,'serve',a.config,volume,str(root/'disk.sock'),str(root/'journal')],stdout=log,stderr=log)
    deadline = time.monotonic()+30
    while not (root/'disk.control').exists():
        assert process.poll() is None and time.monotonic()<deadline
        time.sleep(.05)
    run(['nbd-client','-unix',str(root/'disk.sock'),a.device,'-timeout','120'])
    attached = True
    Path('/sys/block',Path(a.device).name,'queue/max_sectors_kb').write_text('1024')

def request(req):
    global volume, prepared, deleted, last_status
    assert req['version']==1
    ident = req['volume_id']
    assert isinstance(ident,str) and re.fullmatch('[a-f0-9]{64}',ident)
    operation=req['operation']
    if volume is None:
        assert operation=='prepare'
        volume=ident; persist()  # account even for an interrupted import
    assert volume==ident, 'one volume per qualification run'
    if operation=='delete':
        detach(); deleted=True; persist()
    else:
        assert not deleted
        if operation=='prepare':
            if not prepared:
                # An incomplete import fails on retry rather than replacing its
                # remote head. The engine's retained record can still delete it.
                run([a.server,'import',a.config,volume,req['image']],timeout=600)
                prepared=True; persist()
        elif operation=='attach':
            assert prepared
            attach(); last_status=control('status')
        elif operation=='inspect':
            assert attached and process is not None and process.poll() is None
            last_status=control('status')
        elif operation=='status':
            if attached: last_status=control('status')
            assert last_status is not None
        elif operation=='sync':
            assert prepared
            was_attached=attached
            attach(); last_status=control('sync'); persist()
            if not was_attached: detach()
        elif operation=='detach':
            detach()
        else: raise ValueError('unknown operation')
    return dict(ok=True,volume_id=volume,device=a.device if attached else None,status=last_status)

def stop(_signum, _frame):
    raise KeyboardInterrupt
signal.signal(signal.SIGTERM,stop)
signal.signal(signal.SIGINT,stop)
with socket.socket(socket.AF_UNIX) as server:
    server.bind(str(root/'service.sock'));server.listen(4)
    print('Engine volume qualification service ready',flush=True)
    try:
        while True:
            conn,_=server.accept()
            with conn:
                conn.settimeout(5)
                req={}
                try:
                    with conn.makefile('rb') as f: line=f.readline(4097)
                    assert len(line)<=4096 and line.endswith(b'\n')
                    req=json.loads(line)
                    response=request(req)
                except Exception as e:
                    print('Qualification operation failed:',type(e).__name__,flush=True)
                    response=dict(ok=False,volume_id=req.get('volume_id',''))
                try: conn.sendall(json.dumps(response).encode()+b'\n')
                except OSError: pass  # lost reply: operation/identity remain recorded
    except KeyboardInterrupt:
        detach()
