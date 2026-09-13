#!/usr/bin/env python3
"""Restartable single-volume qualification supervisor, not an installed daemon.
Private root and one reserved NBD device. Never reconnect underneath a consumer.
Remote ownership and journals survive shutdown/deletion for later explicit GC.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import select
import signal
import socket
import stat
import struct
import subprocess
import time


def identity(pid):
    try:
        fields = Path('/proc', str(pid), 'stat').read_text().rsplit(')', 1)[1].split()
        if fields[0] == 'Z':
            return None
        return dict(pid=pid, start=fields[19], boot=Path('/proc/sys/kernel/random/boot_id').read_text().strip())
    except FileNotFoundError:
        return None


def alive(saved):
    return saved is not None and identity(saved['pid']) == saved


def terminate(saved):
    if not saved:
        return
    try:
        fd = os.pidfd_open(saved['pid'])
    except ProcessLookupError:
        return
    try:
        if not alive(saved):
            return
        signal.pidfd_send_signal(fd, signal.SIGTERM)
        if not select.select([fd], [], [], 5)[0]:
            signal.pidfd_send_signal(fd, signal.SIGKILL)
            assert select.select([fd], [], [], 10)[0], 'worker did not exit'
    finally:
        os.close(fd)


def consumers(device):
    """Fail closed on inaccessible procfs. Exclude only the kernel NBD client.
    A root-owned private socket serializes the cooperating engine's lifecycle;
    this scan is an additional check, not a boundary against malicious host root.
    """
    dev = os.stat(device).st_rdev
    found = set()
    for proc in Path('/proc').iterdir():
        if not proc.name.isdecimal():
            continue
        try:
            for fd in (proc/'fd').iterdir():
                try:
                    metadata = fd.stat()
                    if stat.S_ISBLK(metadata.st_mode) and metadata.st_rdev == dev:
                        found.add(int(proc.name))
                except FileNotFoundError:
                    pass
        except FileNotFoundError:
            pass
    return found


class Service:
    def __init__(self, args):
        self.a = args
        self.root = Path(args.root)
        assert self.root.is_absolute()
        self.root.mkdir(mode=0o700, exist_ok=True)
        metadata = self.root.lstat()
        assert stat.S_ISDIR(metadata.st_mode) and metadata.st_uid == 0 and metadata.st_mode & 0o077 == 0
        self.locks = []
        for path in [self.root/'service.lock', Path('/run/lock/ahvm-volume-' + Path(args.device).name + '.lock')]:
            fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
            self.locks.append(fd)
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.record = dict(volume_id=None, prepared=False, deleted=False, status=None,
                           worker=None, starting=False, nbd_pid=None, device=args.device)
        if (self.root/'record.json').exists():
            self.record = json.loads((self.root/'record.json').read_text())
            assert self.record['device'] == args.device
        else:
            assert self.nbd_pid() is None, 'device already attached'
            self.persist()
        # Adoption never attaches or starts a worker. An interrupted mutation
        # remains accounted and is repaired only after proving no VM uses it.
        self.child = None

    def persist(self):
        with (self.root/'record.tmp').open('w') as f:
            json.dump(self.record, f)
            f.flush(); os.fsync(f.fileno())
        (self.root/'record.tmp').replace(self.root/'record.json')
        for directory in [self.root, self.root.parent]:
            fd = os.open(directory, os.O_RDONLY)
            try: os.fsync(fd)
            finally: os.close(fd)

    def nbd_pid(self):
        try:
            return int(Path('/sys/block', Path(self.a.device).name, 'pid').read_text())
        except FileNotFoundError:
            return None

    def control(self, op):
        assert alive(self.record['worker']), 'storage worker unavailable'
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(300 if op == 'sync' else 2)
            s.connect(str(self.root/'disk.control'))
            pid, uid, _ = struct.unpack('3i', s.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            assert uid == 0 and pid == self.record['worker']['pid'] and alive(self.record['worker'])
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

    def unused(self):
        # nbd-client itself holds the block device while servicing requests.
        # nbd-client/udev can briefly retain an extra descriptor after the
        # attach command returns. Wait boundedly; never exclude arbitrary PIDs.
        deadline = time.monotonic()+1
        while consumers(self.a.device) - {self.nbd_pid()}:
            assert time.monotonic() < deadline, 'VM still uses disk'
            time.sleep(.02)

    def inspect(self):
        assert self.nbd_pid() is not None and self.nbd_pid() == self.record['nbd_pid'], 'attachment unavailable'
        return self.control('status')

    def detach(self):
        assert not (self.record['starting'] and self.record['worker'] is None), 'unrecorded spawn requires manual fencing'
        self.unused()
        pid = self.nbd_pid()
        if pid is not None:
            assert pid == self.record['nbd_pid'], 'unrecorded attachment: manual fencing required'
            subprocess.run(['nbd-client', '-d', self.a.device], check=True, timeout=30)
            deadline = time.monotonic()+10
            while self.nbd_pid() is not None:
                assert time.monotonic() < deadline, 'NBD disconnect did not complete'
                time.sleep(.02)
        terminate(self.record['worker'])
        if self.child is not None:
            self.child.wait(timeout=10)
            self.child = None
        self.record.update(worker=None, nbd_pid=None, starting=False, status=None)
        self.persist()

    def attach(self):
        if alive(self.record['worker']) and self.nbd_pid() is not None:
            return self.inspect()
        self.unused()
        # A crash between spawn and recording its identity cannot be repaired
        # by guessing a PID or deleting its owner lock. Preserve and refuse.
        assert not (self.record['starting'] and self.record['worker'] is None), 'unrecorded spawn requires manual fencing'
        self.detach()
        for name in ['disk.sock', 'disk.control']:
            (self.root/name).unlink(missing_ok=True)
        (self.root/'owner').mkdir(mode=0o700, exist_ok=True)
        self.record['starting'] = True
        self.persist()
        with (self.root/'storage.log').open('ab', buffering=0) as log:
            self.child = subprocess.Popen([self.a.owned_server, 'serve', self.a.config, self.record['volume_id'],
                                           str(self.root/'disk.sock'), str(self.root/'owner')], stdout=log, stderr=log)
        self.record['worker'] = identity(self.child.pid)
        assert self.record['worker'] is not None
        self.persist()
        deadline = time.monotonic()+30
        while not (self.root/'disk.control').exists():
            assert alive(self.record['worker']) and time.monotonic() < deadline
            time.sleep(.05)
        status = self.control('status')
        subprocess.run(['nbd-client', '-unix', str(self.root/'disk.sock'), self.a.device, '-timeout', '120'], check=True, timeout=30)
        self.record.update(nbd_pid=self.nbd_pid(), starting=False)
        assert self.record['nbd_pid'] is not None
        self.persist()
        Path('/sys/block', Path(self.a.device).name, 'queue/max_sectors_kb').write_text('1024')
        return status

    def request(self, req):
        assert req['version'] == 1
        ident = req['volume_id']
        assert isinstance(ident, str) and re.fullmatch('[a-f0-9]{64}', ident)
        operation = req['operation']
        reply_status = None
        if self.record['volume_id'] is None:
            assert operation == 'prepare'
            self.record['volume_id'] = ident
            self.persist()
        assert self.record['volume_id'] == ident, 'one volume per service'
        if operation == 'delete':
            # Durable intent prevents reopening after a failed/lost detach reply.
            self.record['deleted'] = True
            self.persist()
            self.detach()
        else:
            assert not self.record['deleted']
            if operation == 'prepare':
                if not self.record['prepared']:
                    subprocess.run([self.a.server, 'import', self.a.config, ident, req['image']], check=True, timeout=600)
                    subprocess.run([self.a.owned_server, 'enroll', self.a.config, ident], check=True, timeout=30)
                    self.record['prepared'] = True
                    self.persist()
            elif operation == 'attach':
                assert self.record['prepared']
                self.record['status'] = self.attach()
            elif operation == 'inspect':
                self.record['status'] = self.inspect()
            elif operation == 'status':
                # Never return stale healthy counters after a worker failure.
                if self.record['worker'] is not None:
                    self.record['status'] = self.inspect()
                assert self.record['status'] is not None
            elif operation == 'sync':
                assert self.record['prepared']
                was_attached = self.nbd_pid() is not None
                self.attach()
                self.record['status'] = self.control('sync')
                reply_status = self.record['status']
                self.persist()
                if not was_attached: self.detach()
            elif operation == 'detach':
                self.detach()
            else:
                raise ValueError('unknown operation')
        return dict(ok=True, volume_id=ident, device=self.a.device if self.nbd_pid() else None, status=reply_status if reply_status is not None else self.record['status'])


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ['root', 'config', 'server', 'owned-server']:
        p.add_argument('--'+name, required=True)
    p.add_argument('--device', default='/dev/nbd0')
    args = p.parse_args()
    assert os.geteuid() == 0 and re.fullmatch(r'/dev/nbd[0-9]+', args.device)
    assert Path(args.device).is_block_device() and not Path(args.device).is_symlink()
    os.umask(0o077)
    service = Service(args)
    path = service.root/'service.sock'
    path.unlink(missing_ok=True)  # protected by the lifetime root/device locks
    def stop(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    with socket.socket(socket.AF_UNIX) as server:
        server.bind(str(path)); server.listen(4)
        print('Engine volume qualification service ready', flush=True)
        try:
            while True:
                conn, _ = server.accept()
                with conn:
                    conn.settimeout(5)
                    req = {}
                    try:
                        with conn.makefile('rb') as f: line = f.readline(4097)
                        assert len(line) <= 4096 and line.endswith(b'\n')
                        req = json.loads(line)
                        response = service.request(req)
                    except Exception as error:
                        print('Qualification operation failed:', type(error).__name__, flush=True)
                        response = dict(ok=False, volume_id=req.get('volume_id', '') if isinstance(req, dict) else '')
                    try: conn.sendall(json.dumps(response).encode()+b'\n')
                    except OSError: pass
        except KeyboardInterrupt:
            # Like SIGKILL, stopping the supervisor preserves its worker/device.
            # Engine detach/delete is the explicit, consumer-checked cleanup path.
            pass


if __name__ == '__main__':
    main()
