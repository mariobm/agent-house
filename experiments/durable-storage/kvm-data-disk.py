#!/usr/bin/env python3
"""Opt-in root/KVM gate. One 1-CPU/1-GiB VM; a 64-MiB R2 data disk.
Requires a loaded, unused NBD device, nbd-client, iptables and an Ubuntu dev image.
Only the exact numeric sidecar UID is blocked during the network-outage test.
R2 fixture objects remain for explicit prefix cleanup. No production daemon used.
"""
import argparse
import base64
import json
import fcntl
import re
import os
from pathlib import Path
import shutil
import socket
import struct
import subprocess
import tempfile
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--config', required=True)
p.add_argument('--server', required=True)
p.add_argument('--vmm', required=True)
p.add_argument('--lib', required=True)
p.add_argument('--image', required=True)
p.add_argument('--device', default='/dev/nbd0')
p.add_argument('--uid', type=int, default=199999)
a = p.parse_args()
assert os.geteuid() == 0, 'root required for NBD attachment and isolated UID outage'
assert a.uid > 65535, 'use a dedicated, unused high numeric UID'
assert re.fullmatch(r'/dev/nbd[0-9]+', a.device) and not Path(a.device).is_symlink() and Path(a.device).is_block_device()
device_lock = open('/run/lock/ahvm-volume-' + Path(a.device).name + '.lock', 'w')
fcntl.flock(device_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
assert not Path('/sys/block', Path(a.device).name, 'pid').exists(), 'NBD device already attached'
for proc in Path('/proc').iterdir():
    if proc.name.isdigit():
        try:
            assert proc.stat().st_uid != a.uid, 'sidecar UID already in use'
        except FileNotFoundError:
            pass
work = Path(tempfile.mkdtemp(prefix='ahvm-durable-guest-'))
os.chown(work, a.uid, a.uid)
volume = f'guest-{time.time_ns()}'
print(f'Fixture volume: {volume}; logs: {work}', flush=True)
config = work / 'r2.json'
shutil.copyfile(a.config, config)
config.chmod(0o600)
os.chown(config, a.uid, a.uid)
server = work / 'nbd_serve'
shutil.copyfile(a.server, server)
server.chmod(0o755)
vm = storage = None
attached = False
blocked = []
rule = ['OUTPUT', '-m', 'owner', '--uid-owner', str(a.uid), '-p', 'tcp', '--dport', '443', '-m', 'comment', '--comment', volume, '-j', 'REJECT']
env = {'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LD_LIBRARY_PATH': a.lib}

def command(args, **kwargs):
    return subprocess.run(args, check=True, timeout=90, **kwargs)

def kill(proc):
    if proc is not None and proc.poll() is None:
        proc.kill()
        proc.wait(timeout=15)

def start_storage(create):
    global storage, attached
    sock = work / 'nbd.sock'
    sock.unlink(missing_ok=True)  # only our prior single-client socket
    log = (work / ('storage-create.log' if create else 'storage-reopen.log')).open('ab', buffering=0)
    storage = subprocess.Popen([str(server), str(config), volume, str(sock), 'create' if create else 'open'],
                               stdout=log, stderr=log, env=env, user=a.uid, group=a.uid, extra_groups=[])
    deadline = time.monotonic() + 30
    while not sock.exists():
        assert storage.poll() is None, 'storage exited; inspect its log'
        assert time.monotonic() < deadline, 'storage socket timeout'
        time.sleep(.05)
    command(['nbd-client', '-unix', str(sock), a.device, '-timeout', '60'])
    attached = True
    # Keep kernel requests small even when the client uses EXPORT_NAME.
    Path('/sys/block', Path(a.device).name, 'queue/max_sectors_kb').write_text('1024')
    print('Host NBD cache:', Path('/sys/block', Path(a.device).name, 'queue/write_cache').read_text().strip(), flush=True)

def rpc(argv, timeout=180):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(timeout)
        sock.connect(str(work / 'forge.sock'))
        data = b'\x20' + json.dumps({'argv': argv}).encode()
        sock.sendall(struct.pack('>I', len(data)) + data)
        def read(n):
            out = bytearray()
            while len(out) < n:
                chunk = sock.recv(n-len(out))
                if not chunk:
                    raise RuntimeError('forge disconnected')
                out.extend(chunk)
            return bytes(out)
        length, = struct.unpack('>I', read(4))
        assert 0 < length <= 1048576
        frame = read(length)
        assert frame[0] == 0x24, 'unexpected forge response'
        result = json.loads(frame[1:])
        out = base64.b64decode(result.get('stdout_b64', '')).decode(errors='replace')
        err = base64.b64decode(result.get('stderr_b64', '')).decode(errors='replace')
        assert result['exit_code'] == 0, f'guest exit {result["exit_code"]}: {err}\n{out}'
        return out

def start_vm():
    global vm
    for name in ['root.qcow2', 'forge.sock']:
        (work / name).unlink(missing_ok=True)
    command([a.vmm, 'create-overlay', str(work/'root.qcow2'), a.image, str(os.path.getsize(a.image))], env=env)
    spec = {'vcpus': 1, 'mem_mib': 1024, 'root_disk': str(work/'root.qcow2'), 'root_disk_format': 'qcow2',
            'pid1': True, 'exec_path': '/init.krun', 'vsock_control_uds': str(work/'forge.sock'),
            'volumes': [{'block_id': 'durable', 'path': a.device, 'format': 'raw'}]}
    (work/'spec.json').write_text(json.dumps(spec))
    log = (work/'vm.log').open('ab', buffering=0)
    vm = subprocess.Popen([a.vmm, str(work/'spec.json')], env=env, stdout=log, stderr=log)
    deadline = time.monotonic()+30
    while True:
        assert vm.poll() is None, 'VMM exited; inspect vm.log'
        try:
            rpc(['/bin/true'], timeout=2)
            break
        except (OSError, RuntimeError):
            assert time.monotonic() < deadline, 'guest readiness timeout'
            time.sleep(.1)

workload = r'''
import os, sqlite3, subprocess, zipfile, pathlib
root=pathlib.Path('/mnt/durable')
repo=root/'repo';repo.mkdir()
subprocess.run(['git','init','-q',str(repo)],check=True)
(repo/'marker').write_text('durable-git-marker\n')
subprocess.run(['git','-C',str(repo),'add','.'],check=True)
subprocess.run(['git','-C',str(repo),'-c','user.name=AHVM','-c','user.email=test@example.invalid','commit','-qm','durable'],check=True)
wheel=pathlib.Path('/tmp/probe_pkg-1.0-py3-none-any.whl')
with zipfile.ZipFile(wheel,'w') as z:
 z.writestr('probe_pkg.py','VALUE = "installed-on-durable-disk"\n')
 z.writestr('probe_pkg-1.0.dist-info/METADATA','Metadata-Version: 2.1\nName: probe-pkg\nVersion: 1.0\n')
 z.writestr('probe_pkg-1.0.dist-info/WHEEL','Wheel-Version: 1.0\nGenerator: ahvm\nRoot-Is-Purelib: true\nTag: py3-none-any\n')
 z.writestr('probe_pkg-1.0.dist-info/RECORD','')
subprocess.run(['python3','-m','pip','install','--no-index','--no-deps','--target',str(root/'packages'),str(wheel)],check=True,stdout=subprocess.DEVNULL)
c=sqlite3.connect(root/'proof.db');c.execute('pragma synchronous=FULL');c.execute('create table proof (id integer primary key, value text)')
for i in range(3):
 c.execute('insert into proof values (?,?)',(i,'survived-'+str(i)));c.commit()
c.close()
with open(root/'marker','w') as f:
 f.write('guest-fsync-reached-r2\n');f.flush();os.fsync(f.fileno())
# Flush all Git/package file and directory metadata as well as SQLite's own fsync.
fd=os.open(root,os.O_RDONLY)
import ctypes
libc=ctypes.CDLL(None,use_errno=True)
assert libc.syncfs(fd)==0, 'syncfs failed'
os.close(fd)
print('WORKLOAD-SYNCED')
'''
verify = r'''
import pathlib,sqlite3,subprocess,sys
root=pathlib.Path('/mnt/durable')
assert (root/'marker').read_text()=='guest-fsync-reached-r2\n'
subprocess.run(['git','-C',str(root/'repo'),'fsck','--full'],check=True)
assert subprocess.check_output(['git','-C',str(root/'repo'),'show','HEAD:marker']).decode()=='durable-git-marker\n'
c=sqlite3.connect(root/'proof.db');assert c.execute('pragma integrity_check').fetchone()==('ok',)
assert c.execute('select * from proof order by id').fetchall()==[(i,'survived-'+str(i)) for i in range(3)]
c.close();sys.path.insert(0,str(root/'packages'));import probe_pkg
assert probe_pkg.VALUE=='installed-on-durable-disk'
print('RECOVERED-GIT-PACKAGE-SQLITE')
'''
try:
    start_storage(True)
    start_vm()
    print(rpc(['/bin/sh','-c','mkfs.ext4 -q -F -E nodiscard,lazy_itable_init=0,lazy_journal_init=0 /dev/vdb && mkdir -p /mnt/durable && mount /dev/vdb /mnt/durable']), flush=True)
    print(rpc(['python3','-c',workload]), flush=True)
    # Abrupt compute and storage loss; no shutdown/snapshot/unmount/flush here.
    kill(vm); vm=None
    kill(storage); storage=None
    command(['nbd-client','-d',a.device]); attached=False
    start_storage(False)
    start_vm()  # fresh local root overlay; no old guest or host page cache
    rpc(['/bin/sh','-c','mkdir -p /mnt/durable && mount /dev/vdb /mnt/durable'])
    print(rpc(['python3','-c',verify]), flush=True)
    # Deny only this test sidecar's TLS network access, including existing flows.
    for firewall in ['iptables', 'ip6tables']:
        command([firewall,'-I',*rule]); blocked.append(firewall)
    print(rpc(['python3','-c',r'''
import os
with open('/mnt/durable/outage','wb',buffering=0) as f:
 f.write(b'not-durable'*100)
 try:
  os.fsync(f.fileno())
 except OSError:
  print('OUTAGE-FSYNC-FAILED')
 else:
  raise RuntimeError('fsync falsely acknowledged durability during outage')
''']), flush=True)
    denied = 0
    for firewall in blocked:
        counters = subprocess.check_output([firewall + '-save', '-c'], text=True)
        for line in counters.splitlines():
            if volume in line:
                denied += int(line.split(':', 1)[0].lstrip('['))
    assert denied > 0, 'fault injection blocked no packets'
    print(f'Confirmed {denied} denied sidecar packets', flush=True)
    for firewall in blocked:
        command([firewall,'-D',*rule])
    blocked.clear()
    print('PASS guest ext4/R2 data-disk crash recovery and network-outage failure', flush=True)
finally:
    for firewall in blocked:
        subprocess.run([firewall,'-D',*rule],check=True)
    for proc in [vm, storage]:
        if proc is not None and proc.poll() is None:
            proc.kill()
    for proc in [vm, storage]:
        if proc is not None:
            proc.wait(timeout=30)
    if attached:
        subprocess.run(['nbd-client','-d',a.device],check=True,timeout=30)
    config.unlink(missing_ok=True)
    (work/'root.qcow2').unlink(missing_ok=True)
    prefix = json.loads(Path(a.config).read_text())['prefix']
    print(f'Compute stopped; credentials/root overlay removed. R2 fixture prefix: {prefix}/{volume}/',flush=True)
