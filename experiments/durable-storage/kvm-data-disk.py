#!/usr/bin/env python3
"""Opt-in root/KVM gate. One 1-CPU/1-GiB VM; R2 data disk or indexed root.
Requires an unused NBD device, nbd-client, iptables and a compatible guest image.
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
import textwrap

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--config', required=True)
p.add_argument('--server', required=True)
p.add_argument('--vmm', required=True)
p.add_argument('--lib', required=True)
p.add_argument('--image', required=True)
p.add_argument('--device', default='/dev/nbd0')
p.add_argument('--indexed-root', action='store_true', help='import image and boot entirely from indexed R2 root')
p.add_argument('--stress-cycles', type=int, choices=range(4), default=0, help='additional synced-write/crash/recovery cycles (0-3)')
p.add_argument('--warm-repeat', action='store_true', help='repeat the workload in a new directory with a warm cache')
p.add_argument('--eventual', action='store_true', help='local durable fsync, asynchronous R2 replication')
p.add_argument('--metrics', action='store_true')
p.add_argument('--block-operations', action='store_true', help='probe discard/zeroing in an appended disposable tail, indexed root only')
p.add_argument('--uid', type=int, default=199999)
a = p.parse_args()
if a.block_operations and not a.indexed_root:
    p.error('--block-operations requires --indexed-root')
if a.eventual and not a.indexed_root:
    p.error('--eventual requires --indexed-root')
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
if a.metrics:
    env['AHVM_VOLUME_METRICS'] = '1'

def command(args, **kwargs):
    kwargs.setdefault('timeout', 90)
    return subprocess.run(args, check=True, **kwargs)

def stop_test():
    global vm, storage, attached
    for proc in [vm, storage]:
        if proc is not None and proc.poll() is None:
            proc.kill()
    if attached:
        command(['nbd-client','-d',a.device]); attached=False
    for proc in [vm, storage]:
        if proc is not None:
            proc.wait(timeout=30)
    vm = storage = None

def control(operation):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(300)
        sock.connect(str(work/'nbd.control'))
        sock.sendall(operation.encode()+b'\n')
        data=bytearray()
        while True:
            part=sock.recv(4096)
            if not part: break
            data.extend(part)
            assert len(data)<=4096
        return json.loads(data)

def remote_sync():
    result=control('sync')
    assert 'error' not in result, result
    print('REMOTE-SYNC',result,flush=True)

def forget_journal():
    shutil.rmtree(work/'journal')

def start_storage(create):
    global storage, attached
    sock = work / 'nbd.sock'
    sock.unlink(missing_ok=True)  # only our prior single-client socket
    log = (work / ('storage-create.log' if create else 'storage-reopen.log')).open('ab', buffering=0)
    arguments = [str(server), 'serve' if a.eventual else 'serve-strict', str(config), volume, str(sock)] if a.indexed_root else [str(server), str(config), volume, str(sock), 'create' if create else 'open']
    if a.eventual:
        (work/'nbd.control').unlink(missing_ok=True)
        journal=work/'journal'
        if not journal.exists():
            journal.mkdir(mode=0o700);os.chown(journal,a.uid,a.uid)
        arguments.append(str(journal))
    storage = subprocess.Popen(arguments,
                               stdout=log, stderr=log, env=env, user=a.uid, group=a.uid, extra_groups=[])
    deadline = time.monotonic() + 30
    while not sock.exists():
        assert storage.poll() is None, 'storage exited; inspect its log'
        assert time.monotonic() < deadline, 'storage socket timeout'
        time.sleep(.05)
    command(['nbd-client', '-unix', str(sock), a.device, '-timeout', '120'])
    attached = True
    # Keep kernel requests small even when the client uses EXPORT_NAME.
    Path('/sys/block', Path(a.device).name, 'queue/max_sectors_kb').write_text('1024')
    print('Host NBD cache:', Path('/sys/block', Path(a.device).name, 'queue/write_cache').read_text().strip(), flush=True)

def rpc(argv, timeout=300):
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
    if not a.indexed_root:
        command([a.vmm, 'create-overlay', str(work/'root.qcow2'), a.image, str(os.path.getsize(a.image))], env=env)
    spec = {'vcpus': 1, 'mem_mib': 1024, 'root_disk': str(work/'root.qcow2'), 'root_disk_format': 'qcow2',
            'pid1': True, 'exec_path': '/init.krun', 'vsock_control_uds': str(work/'forge.sock'),
            'volumes': [{'block_id': 'durable', 'path': a.device, 'format': 'raw'}]}
    if a.indexed_root:
        spec.update(root_disk=a.device, root_disk_format='raw', volumes=[])
    (work/'spec.json').write_text(json.dumps(spec))
    log = (work/'vm.log').open('ab', buffering=0)
    vm = subprocess.Popen([a.vmm, str(work/'spec.json')], env=env, stdout=log, stderr=log)
    deadline = time.monotonic()+90
    while True:
        assert vm.poll() is None, 'VMM exited; inspect vm.log'
        try:
            rpc(['/bin/true'], timeout=2)
            break
        except (OSError, RuntimeError):
            assert time.monotonic() < deadline, 'guest readiness timeout'
            time.sleep(.1)

workload = r'''
import os, sqlite3, subprocess, zipfile, pathlib, time
stage=time.monotonic()
root=pathlib.Path('/mnt/durable')
repo=root/'repo';repo.mkdir()
subprocess.run(['git','init','-q',str(repo)],check=True)
(repo/'marker').write_text('durable-git-marker\n')
subprocess.run(['git','-C',str(repo),'add','.'],check=True)
subprocess.run(['git','-C',str(repo),'-c','user.name=AHVM','-c','user.email=test@example.invalid','commit','-qm','durable'],check=True)
print('Git seconds:', round(time.monotonic()-stage,2));stage=time.monotonic()
wheel=pathlib.Path('/tmp/probe_pkg-1.0-py3-none-any.whl')
with zipfile.ZipFile(wheel,'w') as z:
 z.writestr('probe_pkg.py','VALUE = "installed-on-durable-disk"\n')
 z.writestr('probe_pkg-1.0.dist-info/METADATA','Metadata-Version: 2.1\nName: probe-pkg\nVersion: 1.0\n')
 z.writestr('probe_pkg-1.0.dist-info/WHEEL','Wheel-Version: 1.0\nGenerator: ahvm\nRoot-Is-Purelib: true\nTag: py3-none-any\n')
 z.writestr('probe_pkg-1.0.dist-info/RECORD','')
subprocess.run(['python3','-m','pip','install','--no-index','--no-deps','--target',str(root/'packages'),str(wheel)],check=True,stdout=subprocess.DEVNULL)
print('Package seconds:', round(time.monotonic()-stage,2));stage=time.monotonic()
c=sqlite3.connect(root/'proof.db');c.execute('pragma synchronous=FULL');c.execute('create table proof (id integer primary key, value text)')
for i in range(3):
 c.execute('insert into proof values (?,?)',(i,'survived-'+str(i)));c.commit()
c.close()
print('SQLite seconds:', round(time.monotonic()-stage,2));stage=time.monotonic()
with open(root/'marker','w') as f:
 f.write('guest-fsync-reached-r2\n');f.flush();os.fsync(f.fileno())
# Flush all Git/package file and directory metadata as well as SQLite's own fsync.
fd=os.open(root,os.O_RDONLY)
import ctypes
libc=ctypes.CDLL(None,use_errno=True)
assert libc.syncfs(fd)==0, 'syncfs failed'
os.close(fd)
print('Final sync seconds:', round(time.monotonic()-stage,2))
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
    if a.indexed_root:
        source = work / 'import.ext4'
        shutil.copyfile(a.image, source)
        if a.block_operations:
            tail_offset = source.stat().st_size
            assert tail_offset % 65536 == 0
            with source.open('ab') as f:
                f.truncate(tail_offset + 8 * 1024 * 1024)
        os.chown(source, a.uid, a.uid)
        source.chmod(0o400)
        command([str(server), 'import', str(config), volume, str(source)], env=env, user=a.uid, group=a.uid, extra_groups=[], timeout=600)
        source.unlink()
    start_storage(True)
    start_vm()
    if a.indexed_root:
        rpc(['/bin/mkdir', '-p', '/mnt/durable'])
    else:
        print(rpc(['/bin/sh','-c','mkfs.ext4 -q -F -E nodiscard,lazy_itable_init=0,lazy_journal_init=0 /dev/vdb && mkdir -p /mnt/durable && mount /dev/vdb /mnt/durable']), flush=True)
    if a.block_operations:
        # Never issue raw operations inside ext4. Confirm its declared size
        # independently in the guest before touching the appended tail.
        block_setup = f'offset={tail_offset}\n' + r'''
import os, struct, fcntl
fd=os.open('/dev/vda',os.O_RDWR)
sb=os.pread(fd,1024,1024)
assert sb[56:58]==b'\x53\xef', 'expected unpartitioned ext4'
blocks=struct.unpack_from('<I',sb,4)[0]
if struct.unpack_from('<I',sb,96)[0] & 0x80:
 blocks |= struct.unpack_from('<I',sb,336)[0] << 32
assert blocks * (1024 << struct.unpack_from('<I',sb,24)[0]) <= offset
chunk=65536
'''
        block_verify = r'''
assert os.pread(fd,chunk,offset)==b'G'*chunk
assert os.pread(fd,2*chunk,offset+chunk)==bytes(2*chunk)
assert os.pread(fd,2*chunk,offset+3*chunk)==b'R'*(2*chunk)
assert os.pread(fd,chunk,offset+5*chunk)==b'G'*chunk
os.close(fd)
print('BLOCK-ZERO-DISCARD-REWRITE-VERIFIED')
'''
        print(rpc(['python3','-c',block_setup+r'''
assert os.pwrite(fd,b'G'*(6*chunk),offset)==6*chunk
os.fsync(fd)
# Linux include/uapi/linux/fs.h: BLKZEROOUT, BLKDISCARD; two u64 byte ranges.
fcntl.ioctl(fd,0x127f,struct.pack('=QQ',offset+chunk,2*chunk))
fcntl.ioctl(fd,0x1277,struct.pack('=QQ',offset+3*chunk,2*chunk))
# Discard need not return zeroes; prove the range can be reused instead.
assert os.pwrite(fd,b'R'*(2*chunk),offset+3*chunk)==2*chunk
os.fsync(fd)
'''+block_verify]),flush=True)
    started = time.monotonic()
    print(rpc(['python3','-c',workload]), flush=True)
    print(f'Workload: {time.monotonic() - started:.2f}s', flush=True)
    if a.warm_repeat:
        warmed = workload.replace("root=pathlib.Path('/mnt/durable')", "root=pathlib.Path('/mnt/durable/warm');root.mkdir()")
        started = time.monotonic()
        print(rpc(['python3','-c',warmed]), flush=True)
        print(f'Warm workload: {time.monotonic() - started:.2f}s', flush=True)
    # Retain the journal for the first restart: prove local fsync recovery.
    stop_test()
    start_storage(False)
    start_vm()  # fresh processes/device; local overlay only in data-disk mode
    if not a.indexed_root:
        rpc(['/bin/sh','-c','mkdir -p /mnt/durable && mount /dev/vdb /mnt/durable'])
    print(rpc(['python3','-c',verify]), flush=True)
    if a.eventual:
        remote_sync()
        stop_test();forget_journal()
        start_storage(False);start_vm()
        print(rpc(['python3','-c',verify]),flush=True)
        print('REMOTE-ONLY-RECOVERY',flush=True)
        if a.block_operations:
            print(rpc(['python3','-c',block_setup+block_verify]),flush=True)
    for cycle in range(a.stress_cycles):
        # Rewrite the same 4 MiB with a distinct deterministic pattern each cycle.
        # A partial or stale recovery cannot pass the byte-for-byte check.
        script = '''
import hashlib, os
path='/mnt/durable/stress'
f=open(path,'wb',buffering=0)
for block in range(64):
 data=hashlib.sha256(('CYCLE:'+str(block)).encode()).digest()*2048
 assert f.write(data)==len(data)
os.fsync(f.fileno());f.close()
fd=os.open('/mnt/durable',os.O_RDONLY);os.fsync(fd);os.close(fd)
'''.replace('CYCLE', str(cycle))
        rpc(['python3','-c',script])
        if a.eventual: remote_sync()
        stop_test()
        if a.eventual: forget_journal()
        start_storage(False)
        start_vm()
        if not a.indexed_root:
            rpc(['/bin/sh','-c','mkdir -p /mnt/durable && mount /dev/vdb /mnt/durable'])
        script = '''
import hashlib
with open('/mnt/durable/stress','rb') as f:
 for block in range(64):
  assert f.read(65536)==hashlib.sha256(('CYCLE:'+str(block)).encode()).digest()*2048
 assert not f.read(1)
print('STRESS-ROUND-CYCLE-RECOVERED')
'''.replace('CYCLE', str(cycle))
        print(rpc(['python3','-c',script]), flush=True)
    # Deny only this test sidecar's TLS network access, including existing flows.
    for firewall in ['iptables', 'ip6tables']:
        command([firewall,'-I',*rule]); blocked.append(firewall)
    if a.eventual:
        print(rpc(['python3','-c',"import os; f=open('/mnt/durable/outage','wb',buffering=0);f.write(b'local-only');os.fsync(f.fileno());f.close();print('OUTAGE-LOCAL-FSYNC-OK')"]),flush=True)
        assert 'error' in control('sync'), 'remote barrier falsely succeeded during outage'
        print('OUTAGE-REMOTE-SYNC-FAILED',flush=True)
    else:
        print(rpc(['python3','-c',textwrap.dedent(r'''
    import os
    with open('/mnt/durable/outage','wb',buffering=0) as f:
     f.write(b'not-durable'*100)
     try:
      os.fsync(f.fileno())
     except OSError:
      print('OUTAGE-FSYNC-FAILED')
     else:
      raise RuntimeError('fsync falsely acknowledged durability during outage')
    ''')]), flush=True)
    denied = 0
    for firewall in blocked:
        counters = subprocess.check_output([firewall + '-save', '-c'], text=True)
        for line in counters.splitlines():
            if volume in line:
                denied += int(line.split(':', 1)[0].lstrip('['))
    assert denied > 0, 'fault injection blocked no packets'
    print(f'Confirmed {denied} denied sidecar packets', flush=True)
    if a.eventual:
        # Keep both processes alive: restored connectivity must drain the same
        # journal without requiring a restart or operator repair.
        for firewall in blocked:
            command([firewall,'-D',*rule])
        blocked.clear()
        remote_sync()
        status=control('status')
        assert not status['replication_failed'] and not status['local_failed'], status
        stop_test();forget_journal()
        start_storage(False);start_vm()
        print(rpc(['python3','-c',"from pathlib import Path;assert Path('/mnt/durable/outage').read_bytes()==b'local-only';print('CONNECTIVITY-RECOVERY-REMOTE-VERIFIED')"]),flush=True)
        # Separately prove the accepted host-loss contract with a new write.
        for firewall in ['iptables', 'ip6tables']:
            command([firewall,'-I',*rule]);blocked.append(firewall)
        print(rpc(['python3','-c',"import os;f=open('/mnt/durable/lost','wb',buffering=0);f.write(b'pending');os.fsync(f.fileno());f.close()"]),flush=True)
        assert 'error' in control('sync')
        stop_test();forget_journal()
    for firewall in blocked:
        command([firewall,'-D',*rule])
    blocked.clear()
    if a.eventual:
        # No remote barrier succeeded after the outage write. Losing this local
        # disk is allowed to lose that write; demonstrate the declared contract.
        # Stop before unblocking so background replication cannot rescue it.
        start_storage(False);start_vm()
        print(rpc(['python3','-c',"import os;assert not os.path.exists('/mnt/durable/lost');print('HOST-LOSS-PENDING-WRITE-LOST-AS-EXPECTED')"]),flush=True)
    print('PASS guest ext4/R2 recovery and declared durability contract', flush=True)
finally:
    # Disconnect before waiting: a killed VMM can still be blocked in device I/O.
    try:
        for firewall in blocked:
            subprocess.run([firewall,'-D',*rule],check=True,timeout=30)
    finally:
        try:
            for proc in [vm, storage]:
                if proc is not None and proc.poll() is None:
                    proc.kill()
            if attached:
                subprocess.run(['nbd-client','-d',a.device],check=True,timeout=30)
            for proc in [vm, storage]:
                if proc is not None:
                    proc.wait(timeout=30)
        finally:
            config.unlink(missing_ok=True)
            (work/'root.qcow2').unlink(missing_ok=True)
            (work/'import.ext4').unlink(missing_ok=True)
            if (work/'journal').exists(): shutil.rmtree(work/'journal')
            prefix = json.loads(Path(a.config).read_text())['prefix']
            print(f'Private credentials/root copies removed. R2 fixture prefix: {prefix}/{volume}/',flush=True)
