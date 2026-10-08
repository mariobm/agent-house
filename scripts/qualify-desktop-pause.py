#!/usr/bin/env python3
"""Disposable Linux/KVM GPU resident-pause and boot-readiness qualification.

Requires root, systemd 254+, a nonroot test account and an existing immutable
Omarchy image. Uses a fresh /var/tmp directory, hardened private worker broker,
persistently reserved identities and its own delegated cgroups. Production
services, VM state and images are never written. Only exact owned units are
cleaned; evidence and the identity reservation remain even on failure.
"""
import argparse
import base64
import concurrent.futures
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import pwd
import re
import secrets
import shutil
import signal
import socket
import stat
import struct
import subprocess
import threading
import time
import urllib.error
import urllib.request
import zlib


def exact_read(stream, size):
    data = bytearray()
    while len(data) < size:
        part = stream.recv(size - len(data))
        if not part:
            raise EOFError('desktop transport closed')
        data.extend(part)
    return bytes(data)


class WebSocket:
    """Bounded binary WebSocket transport, including quiet-shell ping replies."""
    def __init__(self, port, path, token):
        self.stream = socket.create_connection(('127.0.0.1', port), timeout=10)
        key = base64.b64encode(secrets.token_bytes(16)).decode()
        self.stream.sendall((f'GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n'
            'Upgrade: websocket\r\nConnection: Upgrade\r\n'
            f'Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n'
            f'Authorization: Bearer {token}\r\n\r\n').encode())
        header = b''
        while not header.endswith(b'\r\n\r\n'):
            header += exact_read(self.stream, 1)
            assert len(header) <= 16384
        assert header.startswith(b'HTTP/1.1 101 '), 'WebSocket upgrade refused'
        accept = base64.b64encode(hashlib.sha1((key +
            '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
        assert accept.lower() in header.lower()
        self.pending = bytearray()

    def frame(self, payload, kind=2):
        mask = secrets.token_bytes(4)
        length = len(payload)
        assert length <= 256 * 1024
        prefix = bytes([0x80 | kind])
        if length < 126:
            prefix += bytes([0x80 | length])
        elif length < 65536:
            prefix += b'\xfe' + struct.pack('!H', length)
        else:
            prefix += b'\xff' + struct.pack('!Q', length)
        self.stream.sendall(prefix + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def sendall(self, payload):
        self.frame(payload)

    def recv_frame(self):
        first, second = exact_read(self.stream, 2)
        assert not second & 0x80, 'server frames must not be masked'
        length = second & 127
        if length == 126:
            length = struct.unpack('!H', exact_read(self.stream, 2))[0]
        elif length == 127:
            length = struct.unpack('!Q', exact_read(self.stream, 8))[0]
        assert length <= 256 * 1024
        payload = exact_read(self.stream, length)
        kind = first & 15
        if kind == 9:
            self.frame(payload, 10)
        elif kind == 8:
            raise EOFError('WebSocket closed')
        return kind, payload

    def recv(self, size):
        while not self.pending:
            kind, payload = self.recv_frame()
            if kind in (0, 2):
                self.pending.extend(payload)
            else:
                assert kind in (9, 10)
        data = bytes(self.pending[:size])
        del self.pending[:size]
        return data

    def close(self):
        try:
            self.frame(b'', 8)
        except OSError:
            pass
        self.stream.close()


class Rfb:
    def __init__(self, stream):
        self.stream = stream
        assert exact_read(stream, 12) == b'RFB 003.008\n'
        self.banner_received_at = time.monotonic()
        if isinstance(stream, socket.socket):
            stream.settimeout(10)
        stream.sendall(b'RFB 003.008\n')
        assert 1 in exact_read(stream, exact_read(stream, 1)[0])
        stream.sendall(b'\x01')
        assert exact_read(stream, 4) == bytes(4)
        stream.sendall(b'\x01')
        self.width, self.height = struct.unpack('!HH', exact_read(stream, 4))
        assert 0 < self.width <= 1920 and 0 < self.height <= 1080
        exact_read(stream, 16)
        length = struct.unpack('!I', exact_read(stream, 4))[0]
        assert length <= 4096
        exact_read(stream, length)
        stream.sendall(bytes(4) + struct.pack('!BBBBHHHBBBxxx', 32, 24, 0, 1,
            255, 255, 255, 16, 8, 0))
        stream.sendall(struct.pack('!BBHi', 2, 0, 1, 0))

    def capture(self, output=None):
        w, h = self.width, self.height
        self.stream.sendall(struct.pack('!BBHHHH', 3, 0, 0, 0, w, h))
        pixels, covered = bytearray(w * h * 4), 0
        end = time.monotonic() + 15
        while covered < w * h:
            assert time.monotonic() < end, 'framebuffer deadline'
            kind = exact_read(self.stream, 1)[0]
            if kind == 2:
                continue
            if kind == 3:
                exact_read(self.stream, 3)
                length = struct.unpack('!I', exact_read(self.stream, 4))[0]
                assert length <= 1024 * 1024
                exact_read(self.stream, length)
                continue
            assert kind == 0
            exact_read(self.stream, 1)
            count = struct.unpack('!H', exact_read(self.stream, 2))[0]
            assert count <= 4096
            for _ in range(count):
                x, y, rw, rh, encoding = struct.unpack('!HHHHi', exact_read(self.stream, 12))
                assert encoding == 0 and x + rw <= w and y + rh <= h and rw * rh > 0
                data = exact_read(self.stream, rw * rh * 4)
                for row in range(rh):
                    target = ((y + row) * w + x) * 4
                    pixels[target:target + rw * 4] = data[row * rw * 4:(row + 1) * rw * 4]
                covered += rw * rh
        self.frame_received_at = time.monotonic()
        assert len(set(pixels)) > 16, 'blank framebuffer'
        if output:
            rgb = bytearray()
            for row in range(h):
                rgb.append(0)
                for col in range(w):
                    i = (row * w + col) * 4
                    rgb.extend((pixels[i + 2], pixels[i + 1], pixels[i]))
            def chunk(tag, data):
                return struct.pack('!I', len(data)) + tag + data + struct.pack('!I', zlib.crc32(tag + data))
            output.write_bytes(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR',
                struct.pack('!IIBBBBB', w, h, 8, 2, 0, 0, 0)) + chunk(b'IDAT',
                zlib.compress(rgb)) + chunk(b'IEND', b''))
        return hashlib.sha256(pixels).hexdigest()

    def type(self, text):
        self.stream.sendall(struct.pack('!BBHH', 5, 0, 320, 240) +
            struct.pack('!BBHH', 5, 1, 320, 240) + struct.pack('!BBHH', 5, 0, 320, 240))
        for key in [ord(char) for char in text] + [0xff0d]:
            self.stream.sendall(struct.pack('!BBHI', 4, 1, 0, key) + struct.pack('!BBHI', 4, 0, 0, key))
            time.sleep(.005)

    def close(self):
        self.stream.close()


def reserve_identities(root, unit, base):
    count = 4096
    assert 65536 <= base < 2**31 - count
    def overlaps(start, length):
        return start < base + count and start + length > base
    for database in ('passwd', 'group'):
        for line in subprocess.check_output(['getent', database], text=True).splitlines():
            assert not overlaps(int(line.split(':')[2]), 1), 'identity overlaps account'
    for file in (Path('/etc/subuid'), Path('/etc/subgid')):
        if file.exists():
            for line in file.read_text().splitlines():
                if line.strip() and not line.startswith('#'):
                    _, start, length = line.split(':')
                    assert not overlaps(int(start), int(length)), 'identity overlaps subordinate range'
    registry = Path('/etc/ahvm-worker-ranges')
    registry.mkdir(mode=0o700, exist_ok=True)
    assert registry.lstat().st_uid == 0 and registry.lstat().st_mode & 0o077 == 0
    fd = os.open(registry / '.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'r+') as lock:
        meta = os.fstat(lock.fileno())
        assert stat.S_ISREG(meta.st_mode) and meta.st_uid == 0 and meta.st_nlink == 1
        fcntl.flock(lock, fcntl.LOCK_EX)
        for file in registry.glob('*.json'):
            meta = file.lstat()
            assert stat.S_ISREG(meta.st_mode) and meta.st_uid == 0 and meta.st_mode & 0o077 == 0
            record = json.loads(file.read_text())
            assert not any(overlaps(record[key], record['identity_count']) for key in ('uid_base', 'gid_base'))
        with (registry / (unit + '.json')).open('x') as out:
            os.fchmod(out.fileno(), 0o600)
            json.dump(dict(unit=unit, broker_state=str(root / 'broker'), uid_base=base,
                gid_base=base, identity_count=count), out)
            out.flush()
            os.fsync(out.fileno())
        descriptor = os.open(registry, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(descriptor)
        finally:
            os.close(descriptor)


class ReplicatedFixture:
    def __init__(self, args, private, data, sockets, evidence, command, run_unit, name, slice_name, account):
        self.args, self.private, self.data, self.sockets = args, private, data, sockets
        self.evidence, self.command, self.run_unit = evidence, command, run_unit
        self.name, self.slice_name, self.account = name, slice_name, account
        self.store = args.root / 'store'
        self.device = args.device
        self.loop, self.mounted, self.volume_id, self.proxy = None, False, None, None
        self.lock = threading.Lock()
        self.proxy_errors = []
        import boto3
        from botocore.config import Config
        meta = args.volume_credentials.stat()
        assert meta.st_uid == 0 and meta.st_mode & 0o077 == 0 and meta.st_size <= 16384
        credentials = json.loads(args.volume_credentials.read_text())
        self.bucket, self.remote_prefix = credentials['bucket'], credentials['prefix']
        self.remote = boto3.client('s3', endpoint_url=credentials['endpoint'], region_name=credentials['region'],
            aws_access_key_id=credentials['access_key_id'], aws_secret_access_key=credentials['secret_access_key'],
            aws_session_token=credentials.get('session_token'), config=Config(connect_timeout=3, read_timeout=5,
                proxies={}, retries={'total_max_attempts': 1}, s3={'addressing_style': 'path'}))
        production = json.loads(args.production_volume_config.read_text())
        assert str(self.device) not in production['devices'], 'NBD belongs to production pool'
        assert re.fullmatch(r'/dev/nbd[0-9]+', str(self.device))
        assert stat.S_ISBLK(self.device.stat().st_mode)
        self.sysdev = Path('/sys/block') / self.device.name
        self.unused()

    def unused(self):
        assert not (self.sysdev / 'pid').exists(), 'NBD already attached'
        assert int((self.sysdev / 'size').read_text()) == 0, 'NBD has capacity'
        for fd in Path('/proc').glob('[0-9]*/fd/*'):
            try:
                meta = fd.stat()
                assert not (stat.S_ISBLK(meta.st_mode) and meta.st_rdev == self.device.stat().st_rdev), 'NBD has consumer'
            except (FileNotFoundError, ProcessLookupError):
                pass

    def keys(self):
        assert self.volume_id and re.fullmatch(r'[a-f0-9]{64}', self.volume_id)
        prefix = self.remote_prefix + '/' + self.volume_id + '/'
        values, token = [], None
        while True:
            kwargs = dict(Bucket=self.bucket, Prefix=prefix, MaxKeys=1000)
            if token:
                kwargs['ContinuationToken'] = token
            reply = self.remote.list_objects_v2(**kwargs)
            values.extend(item['Key'] for item in reply.get('Contents', []))
            assert len(values) <= 20000
            assert all(key.startswith(prefix) for key in values)
            token = reply.get('NextContinuationToken')
            if not token:
                return values

    def capture_identity(self, request):
        with self.lock:
            assert re.fullmatch(r'[a-f0-9]{64}', request['volume_id'])
            assert request['sandbox_dir'] == str(self.data / 'sandboxes/desktop-pause')
            if self.volume_id:
                assert self.volume_id == request['volume_id'], 'unexpected second volume'
                return
            assert request['operation'] == 'resources', 'identity must be recorded before prepare'
            self.volume_id = request['volume_id']
            assert self.keys() == [], 'fresh native volume ID required'
            with (self.private / 'owned-volume.json').open('x') as out:
                os.fchmod(out.fileno(), 0o600)
                json.dump(dict(volume_id=self.volume_id, remote_prefix=self.remote_prefix + '/' + self.volume_id + '/',
                    device=str(self.device), sandbox_dir=request['sandbox_dir']), out)
                out.flush()
                os.fsync(out.fileno())

    def start_proxy(self):
        path = self.sockets / 'volume-admission.sock'
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(path))
        path.chmod(0o600)
        os.chown(path, self.account.pw_uid, self.account.pw_gid)
        listener.listen(8)
        self.proxy = listener
        limit = threading.BoundedSemaphore(8)
        def handle(connection):
            try:
                with connection:
                    _, uid, _ = struct.unpack('3i', connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                    assert uid == self.account.pw_uid, 'wrong admission client'
                    connection.settimeout(600)
                    line = connection.makefile('rb').readline(4097)
                    assert len(line) <= 4096 and line.endswith(b'\n')
                    request = json.loads(line)
                    assert request['operation'] in ('resources', 'prepare', 'attach', 'bind', 'inspect',
                        'status', 'sync', 'detach', 'usage', 'delete', 'retire')
                    self.capture_identity(request)
                    with socket.socket(socket.AF_UNIX) as upstream:
                        upstream.settimeout(600)
                        upstream.connect(str(self.sockets / 'service.sock'))
                        upstream.sendall(line)
                        reply = upstream.makefile('rb').readline(4097)
                        assert len(reply) <= 4096 and reply.endswith(b'\n')
                        connection.sendall(reply)
            except Exception as error:
                self.proxy_errors.append(type(error).__name__)
            finally:
                limit.release()
        def accept():
            while True:
                try:
                    connection, _ = listener.accept()
                except OSError:
                    return
                if not limit.acquire(blocking=False):
                    connection.close()
                    continue
                threading.Thread(target=handle, args=(connection,), daemon=True).start()
        threading.Thread(target=accept, daemon=True).start()
        return path

    def setup(self, broker_socket):
        self.store.mkdir(mode=0o700)
        pool = self.private / 'store.xfs'
        self.command(['fallocate', '--length', str(2 * 1024**3), pool])
        assert pool.is_file() and not pool.is_symlink()
        self.command(['mkfs.xfs', '-f', pool])
        self.loop = self.command(['losetup', '--find', '--show', pool]).stdout.strip()
        assert re.fullmatch(r'/dev/loop[0-9]+', self.loop)
        self.command(['mount', '-o', 'nodev,nosuid,noexec', self.loop, self.store])
        self.mounted = True
        self.store.chmod(0o700)
        assert self.store.stat().st_dev != self.args.root.stat().st_dev
        source = self.args.shared_base_cache
        assert source.is_dir() and source.stat().st_uid == 0
        if (source / 'image-digests.json').is_file():
            shutil.copy2(source / 'image-digests.json', self.store / 'image-digests.json')
        shutil.copytree(source / 'base-metadata', self.store / 'base-metadata', symlinks=False)
        disk_keeper = self.name + '-disk-workers'
        self.run_unit(disk_keeper, ['/bin/sleep', 'infinity'], ['Delegate=cpu memory pids',
            'DelegateSubgroup=keeper', 'CPUQuota=100%', 'MemoryMax=768M', 'MemorySwapMax=0', 'TasksMax=256'])
        relative = self.command(['systemctl', 'show', disk_keeper, '-p', 'ControlGroup', '--value']).stdout.strip()
        assert relative.startswith('/' + self.slice_name + '/')
        disk_group = Path('/sys/fs/cgroup') / relative.lstrip('/')
        config = self.private / 'volume.json'
        config.write_text(json.dumps(dict(root=str(self.store), engine_root=str(self.data / 'sandboxes'),
            credentials=str(self.args.volume_credentials), nbd_client='/usr/sbin/nbd-client', devices=[str(self.device)],
            client_uid=self.account.pw_uid, socket_dir=str(self.sockets), image_roots=[str(self.args.image.parent)],
            worker_broker_socket=str(broker_socket), local_base_reads=True,
            limits=dict(max_volume_bytes=40 * 1024**3, max_logical_bytes=40 * 1024**3,
                max_journal_bytes=512 * 1024**2, max_cache_bytes=64 * 1024**2),
            resources=dict(root=str(disk_group), memory_bytes=512 * 1024**2, cpu_quota_us=100000, tasks=128))))
        config.chmod(0o600)
        with open('/run/lock/ahvm-volume-' + self.device.name + '.lock', 'a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.unused()
        self.run_unit(self.name + '-volume', [self.args.bundle / 'bin/ahvm-volumed', config],
            ['KillMode=process', 'NoNewPrivileges=yes', 'CPUQuota=100%', 'MemoryMax=768M', 'MemorySwapMax=0',
                'TasksMax=256', 'Environment=AHVM_TIMINGS=1'])
        end = time.monotonic() + 20
        while not (self.sockets / 'service.sock').exists() and time.monotonic() < end:
            time.sleep(.05)
        assert (self.sockets / 'service.sock').exists(), 'isolated volume service did not start'
        return self.start_proxy()

    def record(self):
        assert self.volume_id
        record = json.loads((self.store / 'volumes' / self.volume_id / 'record.json').read_text())
        assert record['id'] == self.volume_id and record['device'] == str(self.device)
        assert record['sandbox'] == str(self.data / 'sandboxes/desktop-pause')
        return record

    def cleanup(self, result):
        errors = []
        try:
            self.unused()
            if self.volume_id:
                record = self.record()
                assert record['reclaimed'], 'native ownership retirement must complete before remote cleanup'
                keys = self.keys()
                for key in keys:
                    self.remote.delete_object(Bucket=self.bucket, Key=key)
                assert self.keys() == []
                result['replicated_cleanup'] = dict(volume_id=self.volume_id, remote_objects_removed=len(keys),
                    native_reclaimed=True, nbd_unused=True)
        except Exception as error:
            errors.append('owned volume cleanup: ' + type(error).__name__)
        try:
            if self.proxy:
                self.proxy.close()
            if self.mounted:
                self.command(['umount', self.store])
                self.mounted = False
            if self.loop:
                self.command(['losetup', '--detach', self.loop])
                self.loop = None
            pool = self.private / 'store.xfs'
            if pool.exists():
                assert not self.command(['losetup', '--associated', pool]).stdout.strip()
                assert pool.is_file() and not pool.is_symlink() and pool.stat().st_size == 2 * 1024**3
                pool.unlink()
        except Exception as error:
            errors.append('owned loop cleanup: ' + type(error).__name__)
        if errors:
            result['replicated_cleanup_errors'] = errors
            raise RuntimeError('replicated fixture cleanup incomplete')
        result['checks']['replicated_fixture_reclaimed_and_detached'] = True


def main():
    assert __debug__, 'do not disable qualification safety assertions'
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--bundle', type=Path, required=True)
    parser.add_argument('--daemon', type=Path, required=True)
    parser.add_argument('--gpu-worker', type=Path, required=True)
    parser.add_argument('--image', type=Path, required=True)
    parser.add_argument('--resolver', type=ipaddress.IPv4Address, required=True, help='host reachable IPv4 DNS resolver')
    parser.add_argument('--user', default='ahvm_test_security')
    parser.add_argument('--uid-base', type=int, required=True)
    parser.add_argument('--cycles', type=int, default=10)
    parser.add_argument('--mode', choices=('local', 'replicated'), default='local')
    parser.add_argument('--volume-credentials', type=Path)
    parser.add_argument('--shared-base-cache', type=Path)
    parser.add_argument('--device', type=Path)
    parser.add_argument('--production-volume-config', type=Path, default=Path('/etc/ahvm-cloud/volume-service.json'))
    parser.add_argument('--raw-only', action='store_true', help='qualify GPU control while automatic desktop pause remains gated')
    parser.add_argument('--execute', action='store_true')
    args = parser.parse_args()
    assert args.execute and os.geteuid() == 0
    assert args.root.parent == Path('/var/tmp') and args.root.name.startswith('ahvm-omarchy-pause-')
    assert not args.root.exists() and len(str(args.root)) < 70
    assert 1 <= args.cycles <= 20
    assert args.mode != 'replicated' or (args.device and args.volume_credentials and args.shared_base_cache)
    assert all(path.is_file() for path in (args.daemon, args.gpu_worker, args.image))
    assert not args.image.is_symlink(), 'use immutable digest path'
    account = pwd.getpwnam(args.user)
    assert account.pw_uid != 0 and account.pw_gid != 0
    args.root.mkdir(mode=0o711)
    private = args.root / 'private'
    private.mkdir(mode=0o700)
    evidence = args.root / 'evidence'
    evidence.mkdir(mode=0o700)
    data = args.root / 'data'
    data.mkdir(mode=0o700)
    os.chown(data, account.pw_uid, account.pw_gid)
    (data / 'sandboxes').mkdir(mode=0o700)
    os.chown(data / 'sandboxes', account.pw_uid, account.pw_gid)
    bundle = args.root / 'bundle'
    (bundle / 'bin').mkdir(parents=True)
    shutil.copytree(args.bundle / 'gpu', bundle / 'gpu')
    for name in ('ahvm-vmm', 'ahvm-netd', 'ahvm-worker-broker'):
        shutil.copy2(args.bundle / 'bin' / name, bundle / 'bin' / name)
    shutil.copy2(args.daemon, bundle / 'bin/ahvm-daemon')
    shutil.copy2(args.gpu_worker, bundle / 'bin/ahvm-vmm-gpu')
    nonce = secrets.token_hex(5)
    name = 'ahvm_omarchy_pause_' + nonce
    slice_name = name + '.slice'
    keeper, broker, daemon = name + '-workers', name + '-broker', name + '-daemon'
    reserve_identities(args.root, name, args.uid_base)
    result = dict(mode=args.mode, raw_only=args.raw_only, cpus=2, memory_mb=8192,
        cycles=[], checks={}, boot={}, binary_hashes={path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (args.daemon, args.gpu_worker)}, identity_base=args.uid_base)
    units = []
    slice_file = Path('/run/systemd/system') / slice_name
    envfile = private / 'daemon.env'
    token = secrets.token_hex(32)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        port = listener.getsockname()[1]
    endpoint = f'http://127.0.0.1:{port}'
    vm = 'desktop-pause'
    prefix = '/v1/sandboxes/' + vm
    vm_dir = data / 'sandboxes' / vm

    def command(argv, check=True):
        run = subprocess.run([str(item) for item in argv], capture_output=True, text=True, timeout=60)
        with (evidence / 'commands.log').open('a') as log:
            log.write(run.stdout + run.stderr)
        if check:
            assert run.returncode == 0, 'command failed: ' + str(argv[0])
        return run

    def run_unit(unit, argv, properties):
        units.append(unit)
        command(['systemd-run', '--quiet', '--collect', '--unit=' + unit, '--slice=' + slice_name] +
            ['--property=' + prop for prop in properties] + [str(item) for item in argv])

    def call(method, path, body=None, expected=(200, 201, 204)):
        request = urllib.request.Request(endpoint + path, method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
        try:
            response = urllib.request.urlopen(request, timeout=180)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            raw = response.read()
            assert response.status in expected, f'{method} {path}: HTTP {response.status}'
            return json.loads(raw) if raw else None

    def guest(argv):
        reply = call('POST', prefix + '/exec', {'argv': argv})
        if reply['exit_code'] != 0:
            (evidence / 'guest-command-failure.json').write_text(json.dumps(reply, indent=2) + '\n')
        assert reply['exit_code'] == 0, 'guest command failed: ' + argv[0]
        return reply['stdout']

    def wait(predicate, seconds=60):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            try:
                value = predicate()
                if value:
                    return value
            except (OSError, ValueError, AssertionError, EOFError):
                pass
            time.sleep(.2)
        raise TimeoutError('qualification deadline')

    def state():
        return call('GET', prefix)['state']

    def worker():
        return json.loads((vm_dir / 'state.json').read_text())

    def control(value):
        with socket.socket(socket.AF_UNIX) as stream:
            stream.settimeout(10)
            stream.connect(str(vm_dir / 'sock/control.sock'))
            stream.sendall((value + '\n').encode())
            return stream.recv(4096).decode().strip()

    def rfb(ws=True, banner_budget=10):
        if ws:
            stream = WebSocket(port, prefix + '/desktop/stream', token)
        else:
            stream = socket.socket(socket.AF_UNIX)
            stream.settimeout(banner_budget)
            stream.connect(str(vm_dir / 'sock/f.sock'))
        try:
            return Rfb(stream)
        except BaseException:
            stream.close()
            raise

    def https():
        code = guest(['curl', '--ipv4', '--silent', '--show-error', '--fail', '--max-time', '20',
            '--output', '/dev/null', '--write-out', '%{http_code}', 'https://example.com/'])
        assert code == '200', 'guest HTTPS failed'

    def same_worker():
        current = worker()
        assert (current['pid'], current['starttime']) == (original['pid'], original['starttime'])

    def app_identity():
        return guest(['sh', '-ec', 'printf "%s " "$(cat /proc/sys/kernel/random/boot_id)"; '
            'for p in $(pgrep -x Hyprland) $(pgrep -x wayvnc) $(pgrep -x foot); do '
            'printf "%s:" "$p"; cut -d " " -f 22 /proc/$p/stat; done'])

    connections = []
    replica = None
    try:
        assert not slice_file.exists()
        slice_file.write_text('[Unit]\nDescription=Disposable Omarchy pause qualification\n[Slice]\n'
            'CPUQuota=400%\nMemoryMax=20G\nMemorySwapMax=0\nTasksMax=2048\n')
        command(['systemctl', 'daemon-reload'])
        run_unit(keeper, ['/bin/sleep', 'infinity'], ['User=' + args.user, 'Delegate=cpu memory pids',
            'DelegateSubgroup=keeper', 'MemoryMax=18G', 'MemorySwapMax=0', 'TasksMax=1024'])
        relative = command(['systemctl', 'show', keeper, '-p', 'ControlGroup', '--value']).stdout.strip()
        assert relative.startswith('/' + slice_name + '/')
        cgroup = Path('/sys/fs/cgroup') / relative.lstrip('/')
        os.chown(Path('/sys/fs/cgroup') / slice_name / 'cgroup.procs', account.pw_uid, account.pw_gid)
        broker_root = args.root / 'broker'
        for path in (broker_root, broker_root / 'state', broker_root / 'jails'):
            path.mkdir(mode=0o700)
        socket_dir = args.root / 'sockets'
        socket_dir.mkdir(mode=0o755)
        broker_socket = socket_dir / 'worker.sock'
        config = private / 'broker.json'
        config.write_text(json.dumps(dict(socket=str(broker_socket), state_dir=str(broker_root / 'state'),
            jail_dir=str(broker_root / 'jails'), data_dir=str(data / 'sandboxes'), cgroup_root=str(cgroup),
            daemon_uid=account.pw_uid, daemon_gid=account.pw_gid, uid_base=args.uid_base, gid_base=args.uid_base,
            identity_count=4096, vmm_bin=str(bundle / 'bin/ahvm-vmm'),
            gpu_bin=str(bundle / 'bin/ahvm-vmm-gpu'), netd_bin=str(bundle / 'bin/ahvm-netd'),
            lib_path=str(args.bundle / 'lib'), image_roots=[str(args.image)],
            devices=['/dev/dri/renderD128'] + ([str(args.device)] if args.mode == 'replicated' else []))))
        config.chmod(0o600)
        run_unit(broker, [bundle / 'bin/ahvm-worker-broker', config], ['Type=notify', 'NotifyAccess=main',
            'User=root', 'LimitCORE=0', 'NoNewPrivileges=yes', 'AmbientCapabilities=CAP_SETUID CAP_SETGID',
            'KillMode=process', 'PrivateMounts=yes', 'ProtectSystem=strict', 'ProtectHome=yes',
            'ProtectKernelTunables=yes', 'ProtectKernelModules=yes', 'RestrictNamespaces=user mnt',
            'CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_CHROOT CAP_SETUID CAP_SETGID CAP_SETPCAP CAP_MKNOD CAP_CHOWN CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH CAP_KILL',
            'ReadWritePaths=' + str(data) + ' ' + str(broker_root) + ' ' + str(socket_dir) + ' ' + str(cgroup)])
        env = dict(AHVM_DATA_DIR=str(data), AHVM_ADMIN_TOKEN=token, AHVM_LISTEN=f'127.0.0.1:{port}',
            AHVM_VMM_BIN=str(bundle / 'bin/ahvm-vmm'), AHVM_NETD_BIN=str(bundle / 'bin/ahvm-netd'),
            AHVM_LIB=str(args.bundle / 'lib'), AHVM_BASE_IMAGE=str(args.image), AHVM_DESKTOP_IMAGE=str(args.image),
            AHVM_DESKTOP_GPU='1', AHVM_DNS_RESOLVER=str(args.resolver), AHVM_CGROUP_ROOT=str(cgroup),
            AHVM_WORKER_BROKER_SOCKET=str(broker_socket), AHVM_TIMINGS='1', AHVM_SWEEP_SECS='1',
            AHVM_PAUSE_SECS='0' if args.raw_only else '5', AHVM_IDLE_SECS='86400' if args.raw_only else '35')
        if args.mode == 'replicated':
            replica = ReplicatedFixture(args, private, data, socket_dir, evidence, command, run_unit,
                name, slice_name, account)
            env['AHVM_VOLUME_SOCKET'] = str(replica.setup(broker_socket))
        envfile.write_text(''.join(key + '=' + value + '\n' for key, value in env.items()))
        envfile.chmod(0o600)
        run_unit(daemon, [bundle / 'bin/ahvm-daemon'], ['User=' + args.user, 'SupplementaryGroups=kvm render',
            'NoNewPrivileges=yes', 'KillMode=process', 'EnvironmentFile=' + str(envfile), 'MemoryMax=768M',
            'MemorySwapMax=0', 'TasksMax=256', 'ProtectSystem=strict', 'ProtectHome=yes',
            'ReadWritePaths=' + str(data) + ' ' + str(cgroup)])
        wait(lambda: call('GET', '/v1/healthz'))

        boot_started = time.monotonic()
        def first_pixels():
            def probe():
                desktop = rfb(False, .25)
                try:
                    desktop.capture(evidence / 'boot-first-pixels.png')
                    result['boot']['first_rfb_banner_seconds'] = desktop.banner_received_at - boot_started
                    return desktop.frame_received_at - boot_started
                finally:
                    desktop.close()
            return wait(probe, 120)
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            pixels = executor.submit(first_pixels)
            call('POST', '/v1/sandboxes', dict(name=vm, cpus=2, memory_mb=8192, desktop=True, storage_mode=args.mode))
            result['boot']['create_api_seconds'] = time.monotonic() - boot_started
            result['boot']['first_pixels_seconds'] = pixels.result(timeout=120)
        result['boot']['systemd'] = guest(['sh', '-c', 'systemd-analyze; systemd-analyze critical-chain ahvm-forge.service'])
        result['boot']['dmesg'] = guest(['dmesg', '--color=never'])
        original = worker()
        assert args.uid_base <= original['isolation']['uid'] < args.uid_base + 4096
        status = Path('/proc') / str(original['pid']) / 'status'
        credentials = status.read_text()
        assert '\nCapEff:\t0000000000000000\n' in credentials
        assert '\nNoNewPrivs:\t1\n' in credentials
        result['checks']['isolated_worker'] = True
        desktop = rfb(not args.raw_only)
        connections.append(desktop)
        marker = secrets.token_hex(12)
        desktop.type(f'AHVM_UNSAVED={marker}; printf "%s" "$$" > /workspace/desktop-app-pid')
        app_pid = wait(lambda: guest(['cat', '/workspace/desktop-app-pid']).strip())
        initial_apps = app_identity()
        https()
        desktop.capture(evidence / 'before-pause.png')
        desktop.close()
        connections.remove(desktop)
        if not args.raw_only:
            refused = vm_dir / 'tmp/gpu-snapshot-forbidden'
            assert control('SNAPSHOT ' + str(refused)) == 'ERR snapshot: GPU checkpoints are not supported'
            assert control('STATUS') == 'OK running' and not refused.exists()
            result['checks']['running_gpu_snapshot_refused_without_side_effects'] = True

        for cycle in range(args.cycles):
            if args.raw_only:
                started = time.monotonic()
                assert control('PAUSE') == 'OK paused'
                pause_wait = time.monotonic() - started
                time.sleep(2)
            else:
                started = time.monotonic()
                wait(lambda: state() == 'paused', 20)
                pause_wait = time.monotonic() - started
                assert control('STATUS') == 'OK paused'
            same_worker()
            if cycle == 0:
                def cpu_ticks():
                    fields = (Path('/proc') / str(original['pid']) / 'stat').read_text().rsplit(')', 1)[1].split()
                    assert int(fields[19]) == original['starttime']
                    return int(fields[11]) + int(fields[12])
                ticks, sample_started = cpu_ticks(), time.monotonic()
                time.sleep(5)
                seconds = time.monotonic() - sample_started
                cpu_seconds = (cpu_ticks() - ticks) / os.sysconf('SC_CLK_TCK')
                result['paused_cpu'] = dict(sample_seconds=seconds, worker_cpu_seconds=cpu_seconds,
                    single_core_percent=cpu_seconds / seconds * 100)
                assert cpu_seconds < seconds * .1, 'paused GPU worker is consuming CPU'
                if not args.raw_only:
                    assert control('SNAPSHOT ' + str(refused)) == 'ERR snapshot: GPU checkpoints are not supported'
                    assert control('STATUS') == 'OK paused' and not refused.exists()
                    result['checks']['paused_gpu_snapshot_refused_without_side_effects'] = True
            resumed = time.monotonic()
            if args.raw_only:
                assert control('RESUME') == 'OK running'
            desktop = rfb(not args.raw_only)
            connections.append(desktop)
            desktop.capture(evidence / f'cycle-{cycle:02}-frame.png')
            wake_pixels = desktop.frame_received_at - resumed
            desktop.type(f'printf "%s" "$AHVM_UNSAVED" > /workspace/desktop-ram-proof-{cycle}')
            wait(lambda: guest(['cat', f'/workspace/desktop-ram-proof-{cycle}']).strip() == marker)
            assert guest(['sh', '-ec', f'kill -0 {int(app_pid)}; cat /workspace/desktop-app-pid']).strip() == app_pid
            assert app_identity() == initial_apps, 'desktop application identity changed'
            https()
            same_worker()
            desktop.close()
            connections.remove(desktop)
            result['cycles'].append(dict(cycle=cycle, pause_wait_seconds=pause_wait, wake_first_pixels_seconds=wake_pixels))
            print(f'PASS cycle {cycle + 1}: same worker, graphical apps, unsaved shell state, framebuffer/input, HTTPS', flush=True)
        result['checks']['resident_ram_graphics_network'] = True

        if not args.raw_only:
            desktop = rfb()
            connections.append(desktop)
            end = time.monotonic() + 40
            while time.monotonic() < end:
                assert state() == 'running', 'quiet connected viewer idled'
                time.sleep(1)
            desktop.close()
            connections.remove(desktop)
            result['checks']['quiet_viewer_blocks_pause_and_stop'] = True
            wait(lambda: state() == 'paused', 20)
            same_worker()
            command(['systemctl', 'restart', daemon])
            wait(lambda: call('GET', '/v1/healthz'))
            assert state() == 'paused', 'daemon adoption lost paused state'
            same_worker()
            desktop = rfb()
            connections.append(desktop)
            desktop.capture(evidence / 'adopted-resumed.png')
            assert app_identity() == initial_apps
            desktop.close()
            connections.remove(desktop)
            result['checks']['paused_daemon_adoption'] = True

            session = call('POST', prefix + '/sessions', {'argv': ['/bin/bash', '-i'], 'pty': True})
            sid = session['session_id']
            shell = WebSocket(port, prefix + '/sessions/' + sid + '/stream', token)
            connections.append(shell)
            end = time.monotonic() + 40
            shell.stream.settimeout(1)
            while time.monotonic() < end:
                assert state() == 'running', 'quiet connected shell idled'
                try:
                    shell.recv_frame()
                except socket.timeout:
                    pass
            shell.close()
            connections.remove(shell)
            result['checks']['quiet_shell_blocks_pause_and_stop'] = True
            wait(lambda: state() == 'paused', 20)
            actual_control = vm_dir / 'sock/control.sock'
            hidden_control = vm_dir / 'sock/control.hidden'
            actual_control.rename(hidden_control)
            try:
                failure = call('POST', prefix + '/exec', {'argv': ['/bin/true']}, expected=(502,))
                assert failure['code'] == 'backend_unavailable'
                assert state() == 'paused'
                same_worker()
            finally:
                hidden_control.rename(actual_control)
            call('POST', prefix + '/start', {})
            same_worker()
            assert app_identity() == initial_apps
            result['checks']['failed_resume_preserves_worker_and_recovers'] = True
            guest(['sh', '-ec', 'printf DISK-PERSISTED > /workspace/pause-persist; sync'])
            previous_boot = guest(['cat', '/proc/sys/kernel/random/boot_id']).strip()
            wait(lambda: state() == 'paused', 20)
            wait(lambda: state() == 'stopped', 50)
            assert not (Path('/proc') / str(original['pid'])).exists()
            assert not (vm_dir / 'bundle').exists(), 'desktop cold stop wrote a RAM snapshot'
            started = time.monotonic()
            call('POST', prefix + '/start', {})
            result['boot']['idle_cold_start_api_seconds'] = time.monotonic() - started
            assert guest(['cat', '/workspace/pause-persist']) == 'DISK-PERSISTED'
            assert guest(['cat', '/proc/sys/kernel/random/boot_id']).strip() != previous_boot
            assert worker()['starttime'] != original['starttime']
            desktop = rfb()
            connections.append(desktop)
            wait(lambda: desktop.capture(evidence / 'cold-start.png'), 20)
            desktop.close()
            connections.remove(desktop)
            result['checks']['long_idle_cold_stop_disk_only'] = True
            failed_worker = worker()
            wait(lambda: state() == 'paused', 20)
            descriptor = os.pidfd_open(failed_worker['pid'])
            try:
                proc = Path('/proc') / str(failed_worker['pid'])
                fields = (proc / 'stat').read_text().rsplit(')', 1)[1].split()
                assert int(fields[19]) == failed_worker['starttime']
                assert args.uid_base <= failed_worker['isolation']['uid'] < args.uid_base + 4096
                assert (proc / 'cgroup').read_text().strip() == '0::' + str(cgroup / ('vm-' + vm) / 'vmm').removeprefix('/sys/fs/cgroup')
                signal.pidfd_send_signal(descriptor, signal.SIGKILL)
            finally:
                os.close(descriptor)
            wait(lambda: state() == 'failed', 15)
            started = time.monotonic()
            call('POST', prefix + '/start', {})
            result['boot']['failed_worker_cold_start_api_seconds'] = time.monotonic() - started
            assert guest(['cat', '/workspace/pause-persist']) == 'DISK-PERSISTED'
            assert worker()['starttime'] != failed_worker['starttime']
            https()
            desktop = rfb()
            connections.append(desktop)
            wait(lambda: desktop.capture(evidence / 'failed-worker-recovered.png'), 20)
            desktop.close()
            connections.remove(desktop)
            result['checks']['dead_paused_worker_explicit_cold_recovery'] = True
            call('PUT', '/v1/admin/idle-policy', {'pause_after_secs': 30})
            guest(['/bin/true'])
            started = time.monotonic()
            wait(lambda: state() == 'paused', 34)
            result['default_pause_idle_seconds'] = time.monotonic() - started
            desktop = rfb()
            connections.append(desktop)
            desktop.capture(evidence / 'default-pause-resumed.png')
            desktop.close()
            connections.remove(desktop)
            result['checks']['default_30_second_idle_pause'] = True
        call('DELETE', prefix)
        assert call('GET', '/v1/sandboxes')['sandboxes'] == []
        if replica:
            wait(lambda: replica.record()['reclaimed'], 180)
        result['ok'] = True
    except BaseException as error:
        result['ok'] = False
        result['error'] = type(error).__name__ + ': ' + str(error)
        raise
    finally:
        cleanup_errors = []
        def cleanup(label, work):
            try:
                return work()
            except Exception as error:
                cleanup_errors.append(label + ': ' + type(error).__name__)
                return None
        for connection in connections:
            cleanup('connection close', connection.close)
        for unit in units:
            logs = cleanup('journal ' + unit, lambda: command(['journalctl', '--no-pager', '-u', unit, '-o', 'cat'], False))
            if logs:
                cleanup('write journal ' + unit, lambda: (evidence / (unit + '.log')).write_text(logs.stdout))
        for unit in reversed(units):
            cleanup('kill ' + unit, lambda: command(['systemctl', 'kill', '--signal=KILL', unit], False))
            cleanup('stop ' + unit, lambda: command(['systemctl', 'stop', unit], False))
        cleanup('stop slice', lambda: command(['systemctl', 'stop', slice_name], False))
        cleanup('remove slice file', lambda: slice_file.unlink(missing_ok=True))
        cleanup('daemon reload', lambda: command(['systemctl', 'daemon-reload'], False))
        cleanup('remove env', lambda: envfile.unlink(missing_ok=True))
        if replica:
            try:
                replica.cleanup(result)
            except Exception as error:
                cleanup_errors.append('replicated cleanup: ' + type(error).__name__)
        def remove_auth_database():
            assert not (Path('/sys/fs/cgroup') / slice_name).exists(), 'owned processes must finish before auth cleanup'
            for suffix in ('', '-wal', '-shm', '-journal'):
                (data / ('daemon.db' + suffix)).unlink(missing_ok=True)
        cleanup('remove fixture auth database', remove_auth_database)
        result['cleanup_errors'] = cleanup_errors
        if cleanup_errors:
            result['ok'] = False
        (args.root / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(dict(ok=result.get('ok'), checks=result['checks'], boot={key: value for key, value in result['boot'].items() if key.endswith('seconds')}, cycles=result['cycles']), indent=2), flush=True)
        assert not cleanup_errors, 'fixture cleanup incomplete; inspect retained evidence'


if __name__ == '__main__':
    main()
