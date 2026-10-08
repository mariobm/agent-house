#!/usr/bin/env python3
"""Opt-in, isolated Linux/KVM + native NBD/R2 worker-policy recovery gate.

Requires root, systemd 254+, an existing nonroot test user, boto3, static BusyBox,
nbd-client, e2fsprogs and explicitly built binaries. It uses a fresh directory,
one unused NBD device outside the production pool and the dedicated private R2
qualification bucket. No production config, services, image or VM are changed.
Results and phase-separated logs remain; owned units, loop mount, credentials
and the exact fresh R2 prefix are cleaned even on failure.
"""
import argparse
import errno
import fcntl
import grp
import hashlib
import json
import mmap
import os
from pathlib import Path
import pwd
import re
import secrets
import shutil
import socket
import stat
import subprocess
import sys
import time
import traceback
import urllib.error
import urllib.request

MIB = 1024 * 1024


def digest(data):
    return hashlib.sha256(data).hexdigest()


def durable(path, data, mode=0o600):
    with path.open('wb') as file:
        os.fchmod(file.fileno(), mode)
        file.write(data)
        file.flush()
        os.fsync(file.fileno())
    fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def main():
    if not __debug__:
        raise RuntimeError('qualification safety checks require Python assertions; do not use -O')
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--credentials', type=Path, required=True)
    parser.add_argument('--daemon', type=Path, required=True)
    parser.add_argument('--vmm', type=Path, required=True)
    parser.add_argument('--volumed', type=Path, required=True)
    parser.add_argument('--worker-broker', type=Path, required=True)
    parser.add_argument('--forge', type=Path, required=True)
    parser.add_argument('--policy-probe', type=Path, required=True)
    parser.add_argument('--lib', type=Path, required=True)
    parser.add_argument('--device', type=Path, required=True)
    parser.add_argument('--user', required=True)
    parser.add_argument('--source-head', required=True)
    parser.add_argument('--uid-base', type=int, required=True,
                        help='fresh explicit reserved range; never reuse a range from a fixture that launched workers')
    parser.add_argument('--production-config', type=Path,
                        default=Path('/etc/ahvm-cloud/volume-service.json'))
    parser.add_argument('--execute', action='store_true')
    args = parser.parse_args()
    assert args.execute and os.environ.get('AHVM_WORKER_REPLICATED_TEST') == '1'
    assert os.geteuid() == 0 and args.root.parent == Path('/var/tmp')
    assert args.root.name.startswith('ahvm-nbd-r2-') and not args.root.exists()
    assert len(str(args.root / 'store')) <= 70
    assert re.fullmatch(r'[a-f0-9]{40}', args.source_head)
    assert 65536 <= args.uid_base < 2**31 - 4096
    for key in ('daemon', 'vmm', 'volumed', 'forge', 'policy_probe', 'worker_broker'):
        assert getattr(args, key).is_file() and os.access(getattr(args, key), os.X_OK)
    user = pwd.getpwnam(args.user)
    assert user.pw_uid != 0
    group_name = grp.getgrgid(user.pw_gid).gr_name
    kvm_check = subprocess.run(['runuser', '-u', args.user, '-g', group_name, '-G', 'kvm', '--',
        'python3', '-c', 'import os; fd=os.open("/dev/kvm", os.O_RDWR); os.close(fd)'],
        capture_output=True, timeout=5)
    assert kvm_check.returncode == 0, 'test UID with kvm supplementary group cannot open /dev/kvm'
    device = args.device
    assert re.fullmatch(r'/dev/nbd[0-9]+', str(device))
    assert stat.S_ISBLK(device.stat().st_mode)
    if args.production_config.exists():
        production = json.loads(args.production_config.read_text())
        assert str(device) not in production['devices'], 'device is in production pool'
    sysdev = Path('/sys/block') / device.name

    def unused():
        assert not (sysdev / 'pid').exists(), 'NBD already attached'
        assert int((sysdev / 'size').read_text()) == 0
        for fd in Path('/proc').glob('[0-9]*/fd/*'):
            try:
                meta = fd.stat()
                assert not (stat.S_ISBLK(meta.st_mode) and meta.st_rdev == device.stat().st_rdev), 'NBD has consumer'
            except (FileNotFoundError, ProcessLookupError):
                pass

    unused()
    # Native volumed holds this same per-device lock for its whole lifetime.
    with open('/run/lock/ahvm-volume-' + device.name + '.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        unused()
    credential_meta = args.credentials.stat()
    assert credential_meta.st_mode & 0o077 == 0 and credential_meta.st_size <= 16384
    credentials = json.loads(args.credentials.read_text())
    assert credentials['bucket'] == 'ahvm-volume-qualification', 'dedicated test bucket required'
    assert credentials['endpoint'].startswith('https://')
    credentials['prefix'] = 'worker-isolation-' + secrets.token_hex(12)
    prefix = credentials['prefix'] + '/'
    # Explicit credentials; bounded calls, no metadata discovery or HTTP proxy.
    import boto3
    from botocore.config import Config
    from botocore.exceptions import ClientError
    remote = boto3.client('s3', endpoint_url=credentials['endpoint'],
        region_name=credentials['region'], aws_access_key_id=credentials['access_key_id'],
        aws_secret_access_key=credentials['secret_access_key'],
        aws_session_token=credentials.get('session_token'),
        config=Config(connect_timeout=5, read_timeout=20, proxies={},
            retries={'total_max_attempts': 1}, s3={'addressing_style': 'path'}))
    bucket = credentials['bucket']

    def keys():
        out, token = [], None
        while True:
            kwargs = dict(Bucket=bucket, Prefix=prefix, MaxKeys=1000)
            if token:
                kwargs['ContinuationToken'] = token
            result = remote.list_objects_v2(**kwargs)
            out.extend(item['Key'] for item in result.get('Contents', []))
            assert len(out) <= 20000, 'unexpected fixture size'
            token = result.get('NextContinuationToken')
            if not token:
                return out

    assert keys() == [], 'fresh prefix required'
    args.root.mkdir(mode=0o711)
    # Launch only root-protected copies. A build cache/source checkout may have
    # user-writable ancestors, which the production broker correctly rejects.
    binaries = args.root / 'bin'
    binaries.mkdir(mode=0o755)
    for key in ('daemon', 'vmm', 'volumed', 'forge', 'policy_probe', 'worker_broker'):
        target = binaries / key
        shutil.copyfile(getattr(args, key), target)
        target.chmod(0o755)
        setattr(args, key, target)
    private, evidence = args.root / 'private', args.root / 'evidence'
    private.mkdir(mode=0o700)
    evidence.mkdir(mode=0o700)
    creds = private / 'r2.json'
    data, images, store, sockets = (args.root / name for name in ('data', 'images', 'store', 'sockets'))
    for path, mode in ((data, 0o700), (images, 0o755), (store, 0o700), (sockets, 0o711)):
        path.mkdir(mode=mode)
    os.chown(data, user.pw_uid, user.pw_gid)
    (data / 'sandboxes').mkdir(mode=0o700)
    os.chown(data / 'sandboxes', user.pw_uid, user.pw_gid)
    result = {'source_head': args.source_head, 'prefix': credentials['prefix'],
        'client_uid': user.pw_uid, 'device': str(device), 'checks': {}, 'timings_seconds': {}}
    result['binaries'] = {key: digest(getattr(args, key).read_bytes())
        for key in ('vmm', 'daemon', 'volumed', 'forge', 'policy_probe', 'worker_broker')}
    units, unit_files, loop, mounted = [], [], None, False
    removed_key, removed_bytes = None, None
    phase = 'setup'
    phase_time = time.strftime('%Y-%m-%d %H:%M:%S UTC', time.gmtime())
    vm = 'replica-proof'
    vm_dir = data / 'sandboxes' / vm

    def begin_phase(value):
        nonlocal phase, phase_time
        phase = value
        phase_time = time.strftime('%Y-%m-%d %H:%M:%S UTC', time.gmtime())
    token = secrets.token_hex(32)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        port = listener.getsockname()[1]
    endpoint = f'http://127.0.0.1:{port}'
    nonce = secrets.token_hex(5)
    name = 'ahvm-nbd-r2-' + nonce
    slice_name = 'ahvm_nbd_r2_' + nonce + '.slice'
    volume_unit, daemon_unit = name + '-volume', name + '-daemon'
    broker_unit = name + '-broker'
    vm_keeper, disk_keeper = name + '-vm-workers', name + '-disk-workers'

    def command(argv, check=True, timeout=90):
        run = subprocess.run([str(x) for x in argv], capture_output=True, text=True, timeout=timeout)
        with (evidence / (phase + '-commands.log')).open('a') as log:
            log.write(run.stdout + run.stderr)
        if check:
            assert run.returncode == 0, f'command failed: {Path(str(argv[0])).name} (phase {phase})'
        return run

    def run_unit(unit, argv, properties=(), environment=None):
        units.append(unit)
        cli = ['systemd-run', '--quiet', '--collect', '--unit=' + unit, '--slice=' + slice_name]
        for prop in properties:
            cli += ['--property=' + prop]
        if environment:
            cli += ['--property=EnvironmentFile=' + str(environment)]
        command(cli + [str(x) for x in argv])

    def group(unit):
        relative = command(['systemctl', 'show', unit, '-p', 'ControlGroup', '--value']).stdout.strip()
        assert relative.startswith('/' + slice_name + '/')
        return Path('/sys/fs/cgroup') / relative.lstrip('/')

    def wait(function, timeout=120):
        start = time.monotonic()
        last = None
        while time.monotonic() - start < timeout:
            try:
                value = function()
                if value:
                    return value
            except (AssertionError, OSError, ValueError) as error:
                last = type(error).__name__
            time.sleep(0.1)
        raise AssertionError(f'wait failed in {phase}: {last}')

    def call(method, path, body=None, binary=False):
        headers = {'Authorization': 'Bearer ' + token,
                   'Content-Type': 'application/octet-stream' if binary else 'application/json'}
        request = urllib.request.Request(endpoint + path, method=method, headers=headers,
            data=body if binary else None if body is None else json.dumps(body).encode())
        try:
            response = urllib.request.urlopen(request, timeout=350)
        except urllib.error.HTTPError as error:
            response = error
        return response.status, json.loads(response.read())

    def operation(action, good=True):
        start = time.monotonic()
        key = 'isolation-' + secrets.token_hex(12)
        body = dict(action=action, sandbox_id=vm)
        if action == 'create':
            body.update(cpus=1, memory_mb=256, storage_mode='replicated', image='ubuntu-dev')
        status, value = call('POST', '/v1/operations/' + key, body)
        deadline = start + 600
        while status == 202 and time.monotonic() < deadline:
            time.sleep(0.1)
            status, value = call('GET', '/v1/operations/' + key)
        durable(evidence / (phase + '-' + action + '.json'), json.dumps(value, indent=2).encode())
        result['timings_seconds'][phase + '_' + action] = round(time.monotonic() - start, 3)
        success = status == 200 and value.get('state') == 'done' and 200 <= value.get('status', 0) < 300
        print(f'{phase} {action}: success={success} seconds={result["timings_seconds"][phase + "_" + action]}', flush=True)
        if good:
            assert success, f'{action} failed in {phase}; see operation evidence'
        return success

    def execute(script, good=True):
        status, value = call('POST', '/v1/sandboxes/' + vm + '/exec', {'argv': ['/bin/sh', '-ec', script]})
        if good:
            assert status == 200 and value['exit_code'] == 0, (status, value)
        return status, value

    def verify_marker(label, expected):
        status, value = execute('sync; sha256sum /workspace/proof.data')
        observed = value['stdout'].split()[0]
        durable(evidence / (label + '-guest-hash.json'), json.dumps(dict(
            status=status, expected=expected, observed=observed, response=value), indent=2).encode())
        assert observed == expected
        result.setdefault('observed_guest_sha256', {})[label] = observed

    def probe_policy():
        spec = json.loads((vm_dir / 'spec.json').read_text())
        assert spec['root_disk'] == str(device) and spec['root_disk_format'] == 'raw'
        assert spec['trusted_host_socket_access'] is False and spec.get('net_uds', '') == ''
        assert str(device) in spec['worker_sandbox']['read_write']
        assert str(images) not in spec['worker_sandbox']['read_write']
        durable(evidence / (phase + '-spec.json'), json.dumps(spec, indent=2).encode())
        with socket.socket() as listener:
            listener.bind(('127.0.0.1', 0))
            listener.listen()
            probe = command(['runuser', '-u', args.user, '-g', group_name, '-G', 'kvm', '--',
                'env', 'AHVM_WORKER_REPLICATED_TEST=1', str(args.policy_probe),
                str(vm_dir / 'spec.json'), str(device), str(peer),
                '127.0.0.1:' + str(listener.getsockname()[1])])
            assert 'peer contents and TCP bind/connect denied' in probe.stdout
        assert peer.read_bytes() == b'peer contents'
        assert 'worker filesystem/signal sandbox enforced' in (vm_dir / 'vmm.log').read_text()
        worker = json.loads((vm_dir / 'state.json').read_text())
        identity = worker['isolation']
        assert identity['role'] == 'vmm' and identity['uid'] != user.pw_uid
        # Inspect the actual emitted jail, not a reconstructed allowlist. The
        # separate actual-launch adversarial gate exercises denied syscalls.
        jail = Path(f'/proc/{worker["pid"]}/root')
        assert not (jail / str(peer).lstrip('/')).exists()
        assert not (jail / str(data / 'daemon.db').lstrip('/')).exists()
        private_device = jail / str(device).lstrip('/')
        metadata = private_device.stat()
        assert stat.S_ISBLK(metadata.st_mode) and metadata.st_rdev == device.stat().st_rdev
        assert metadata.st_uid == identity['uid'] and device.stat().st_uid == user.pw_uid
        assert json.loads((jail / str(vm_dir / 'spec.json').lstrip('/')).read_text()) == spec
        # Volume integration may inspect only exact root-ledger identity/device.
        with socket.socket(socket.AF_UNIX) as inspect:
            inspect.settimeout(15)
            inspect.connect(str(sockets / 'worker-broker.sock'))
            inspect.sendall((json.dumps(dict(action='inspect', id=vm, role='vmm', worker=worker)) + '\n').encode())
            reply = json.loads(inspect.makefile().readline())
            assert reply['error'] is None and reply['alive'] and reply['root_disk'] == str(device)
        result['checks']['actual_nbd_jail_private_inode_and_root_ledger_' + phase] = True
        result['checks']['exact_nbd_policy_nonroot_and_tcp_' + phase] = True

    def record():
        paths = list((store / 'volumes').glob('*/record.json'))
        assert len(paths) == 1
        return json.loads(paths[0].read_text())

    def snapshot(label):
        state = record()
        assert state['sandbox'] == str(vm_dir) and state['device'] == str(device)
        owner = store / 'volumes' / state['id'] / 'owner'
        state['local_files'] = sorted(str(p.relative_to(owner)) for p in owner.rglob('*'))
        durable(evidence / (label + '-record.json'), json.dumps(state, indent=2).encode())
        return state

    def evicted(label):
        start = time.monotonic()
        wait(lambda: record()['evicted'], 240)
        state = snapshot(label)
        assert all(state[key] is None for key in ('worker', 'client', 'vm'))
        assert not (store / 'volumes' / state['id'] / 'owner' / 'journal').exists()
        assert not (store / 'volumes' / state['id'] / 'owner' / 'cache').exists()
        unused()
        result['timings_seconds'][label + '_eviction'] = round(time.monotonic() - start, 3)
        return state

    def logs(label):
        for unit in units:
            run = command(['journalctl', '--no-pager', '-u', unit, '-o', 'cat', '--since=' + phase_time], check=False)
            durable(evidence / (label + '-' + unit + '.log'), run.stdout.encode())
        if (vm_dir / 'vmm.log').exists():
            # Each VMM boot truncates this log; it is already a per-boot record.
            durable(evidence / (label + '-vmm.log'), (vm_dir / 'vmm.log').read_bytes())

    def read_remote(key, bound=2 * MIB):
        assert key.startswith(prefix) and len(key) < 512
        response = remote.get_object(Bucket=bucket, Key=key)
        with response['Body'] as stream:
            value = stream.read(bound + 1)
        assert len(value) <= bound
        return value

    def restore_object():
        nonlocal removed_key
        if removed_key:
            remote.put_object(Bucket=bucket, Key=removed_key, Body=removed_bytes)
            assert digest(read_remote(removed_key)) == digest(removed_bytes)
            result['checks']['negative_object_restored_and_verified'] = True
            removed_key = None

    def volume_request(volume, action):
        request = dict(version=1, operation=action, volume_id=volume, sandbox_dir=str(vm_dir))
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(90)
            connection.connect(str(sockets / 'service.sock'))
            connection.sendall(json.dumps(request).encode() + b'\n')
            value = connection.makefile('rb').readline(4097)
        assert len(value) <= 4096 and json.loads(value)['ok']

    def direct_read(offset):
        fd = os.open(device, os.O_RDONLY | os.O_DIRECT)
        try:
            # Anonymous mmap provides page alignment required by O_DIRECT.
            with mmap.mmap(-1, 65536) as buffer:
                try:
                    count = os.preadv(fd, [buffer], offset)
                    return {'bytes': count, 'sha256': digest(buffer[:count]), 'errno': 0}
                except OSError as error:
                    return {'bytes': 0, 'errno': error.errno}
        finally:
            os.close(fd)

    try:
        durable(creds, json.dumps(credentials).encode())
        slice_file = Path('/run/systemd/system') / slice_name
        assert not slice_file.exists()
        durable(slice_file, b'[Unit]\nDescription=Isolated AHVM NBD/R2 qualification\n[Slice]\nCPUQuota=200%\nMemoryMax=3G\nMemorySwapMax=0\nTasksMax=1536\n')
        unit_files.append(slice_file)
        command(['systemctl', 'daemon-reload'])
        run_unit(vm_keeper, ['/bin/sleep', 'infinity'], [
            'User=' + args.user, 'Delegate=cpu memory pids', 'DelegateSubgroup=keeper',
            'CPUQuota=100%', 'MemoryMax=1G', 'MemorySwapMax=0', 'TasksMax=512'])
        run_unit(disk_keeper, ['/bin/sleep', 'infinity'], [
            'Delegate=cpu memory pids', 'DelegateSubgroup=keeper',
            'CPUQuota=100%', 'MemoryMax=768M', 'MemorySwapMax=0', 'TasksMax=256'])
        vm_group, disk_group = group(vm_keeper), group(disk_keeper)
        broker_state, broker_jails = args.root / 'broker-state', args.root / 'broker-jails'
        broker_state.mkdir(mode=0o700)
        broker_jails.mkdir(mode=0o700)
        broker_config = private / 'worker-broker.json'
        durable(broker_config, json.dumps(dict(socket=str(sockets / 'worker-broker.sock'),
            state_dir=str(broker_state), jail_dir=str(broker_jails), data_dir=str(data / 'sandboxes'),
            cgroup_root=str(vm_group), daemon_uid=user.pw_uid, daemon_gid=user.pw_gid,
            uid_base=args.uid_base, gid_base=args.uid_base, identity_count=4096,
            vmm_bin=str(args.vmm), netd_bin=None, gpu_bin=None, lib_path=str(args.lib),
            image_roots=[str(images)], devices=[str(device)])).encode())
        run_unit(broker_unit, [args.worker_broker, broker_config], ['Type=notify', 'NotifyAccess=main',
            'KillMode=process', 'User=root', 'Group=root', 'NoNewPrivileges=yes', 'UMask=0077', 'LimitCORE=0',
            'AmbientCapabilities=CAP_SETUID CAP_SETGID',
            'CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_CHROOT CAP_SETUID CAP_SETGID CAP_SETPCAP CAP_MKNOD CAP_CHOWN CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH CAP_KILL',
            'RestrictNamespaces=user mnt', 'CPUQuota=100%', 'MemoryMax=128M', 'TasksMax=64'])
        wait(lambda: (sockets / 'worker-broker.sock').exists())
        # Move only new fixture VMMs across siblings in this private slice.
        os.chown(Path('/sys/fs/cgroup') / slice_name / 'cgroup.procs', user.pw_uid, user.pw_gid)
        store_image = private / 'store.ext4'
        with store_image.open('wb') as file:
            file.truncate(2 * 1024 * MIB)
        command(['mkfs.ext4', '-q', '-F', store_image])
        loop = command(['losetup', '--find', '--show', store_image]).stdout.strip()
        assert re.fullmatch(r'/dev/loop[0-9]+', loop)
        command(['mount', '-o', 'nodev,nosuid,noexec', loop, store])
        mounted = True
        os.chmod(store, 0o700)
        rootfs = private / 'rootfs'
        for path in ('bin', 'dev', 'proc', 'sys', 'tmp', 'run', 'workspace', 'root', 'usr/local/bin'):
            (rootfs / path).mkdir(parents=True, exist_ok=True)
        shutil.copy2('/usr/bin/busybox', rootfs / 'bin/busybox')
        for applet in command(['/usr/bin/busybox', '--list']).stdout.splitlines():
            if applet != 'busybox':
                (rootfs / 'bin' / applet).symlink_to('busybox')
        shutil.copy2(args.forge, rootfs / 'usr/local/bin/ahvm-forge')
        library_list = command(['ldd', args.forge]).stdout
        for library in re.findall(r'(/[^\s]+)', library_list):
            origin = Path(library)
            if origin.is_file():
                target = rootfs / origin.relative_to('/')
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(origin, target)
        durable(rootfs / 'init.krun', b'#!/bin/sh\nexport PATH=/bin:/usr/local/bin HOME=/root\ngrep -q " /proc " /proc/mounts 2>/dev/null || mount -t proc proc /proc\ngrep -q " /sys " /proc/mounts || mount -t sysfs sysfs /sys\ngrep -q " /dev " /proc/mounts || mount -t devtmpfs devtmpfs /dev\nmkdir -p /dev/pts\ngrep -q " /dev/pts " /proc/mounts || mount -t devpts devpts /dev/pts\nip link set lo up\ncd /workspace\nexec /usr/local/bin/ahvm-forge\n', 0o755)
        image = images / 'fixture.ext4'
        with image.open('wb') as file:
            file.truncate(128 * MIB)
        command(['mkfs.ext4', '-q', '-F', '-d', rootfs, image])
        image_hash = digest(image.read_bytes())
        image = image.rename(images / (image_hash + '.ext4'))
        image.chmod(0o644)
        durable(images / 'ubuntu-dev.json', json.dumps({'sha256': image_hash, 'guest_abi': 1}).encode(), 0o644)
        # The configured default passes general daemon preflight but cannot boot.
        # The actual VM uses the separate pinned image, removed before recovery.
        placeholder = images / 'default-placeholder.ext4'
        durable(placeholder, bytes(65536), 0o644)
        peer = data / 'peer-sentinel'
        durable(peer, b'peer contents')
        os.chown(peer, user.pw_uid, user.pw_gid)
        config = private / 'volume.json'
        durable(config, json.dumps(dict(root=str(store), engine_root=str(data / 'sandboxes'),
            credentials=str(creds), nbd_client='/usr/sbin/nbd-client', devices=[str(device)],
            client_uid=user.pw_uid, socket_dir=str(sockets), image_roots=[str(images)],
            worker_broker_socket=str(sockets / 'worker-broker.sock'),
            local_base_reads=True, limits=dict(max_volume_bytes=128 * MIB, max_logical_bytes=128 * MIB,
                max_journal_bytes=512 * MIB, max_cache_bytes=64 * MIB),
            resources=dict(root=str(disk_group), memory_bytes=512 * MIB,
                cpu_quota_us=100000, tasks=128))).encode())
        envfile = private / 'daemon.env'
        env = dict(AHVM_LISTEN=f'127.0.0.1:{port}', AHVM_DATA_DIR=str(data),
            AHVM_VMM_BIN=str(args.vmm), AHVM_BASE_IMAGE=str(placeholder), AHVM_IMAGE_DIR=str(images),
            AHVM_LIB=str(args.lib), AHVM_VOLUME_SOCKET=str(sockets / 'service.sock'),
            AHVM_CGROUP_ROOT=str(vm_group), AHVM_ADMIN_TOKEN=token, AHVM_TIMINGS='1')
        env['AHVM_WORKER_BROKER_SOCKET'] = str(sockets / 'worker-broker.sock')
        durable(envfile, ''.join(f'{key}={value}\n' for key, value in env.items()).encode())
        unused()
        run_unit(volume_unit, [args.volumed, config], ['KillMode=process', 'CPUQuota=100%',
            'MemoryMax=768M', 'MemorySwapMax=0', 'TasksMax=256', 'Environment=AHVM_TIMINGS=1'])
        wait(lambda: (sockets / 'service.sock').exists())
        run_unit(daemon_unit, [args.daemon], ['User=' + args.user, 'SupplementaryGroups=kvm', 'KillMode=process',
            'CPUQuota=100%', 'MemoryMax=512M', 'MemorySwapMax=0', 'TasksMax=256'], envfile)
        wait(lambda: call('GET', '/v1/healthz')[0] == 200)
        assert call('GET', '/v1/sandboxes')[1]['sandboxes'] == []
        begin_phase('initial')
        operation('create')
        assert (vm_dir / 'root.qcow2').exists() is False
        state = snapshot('initial-running')
        original_worker = state['worker']['pid']
        original_vmm = state['vm']['pid']
        assert Path(f'/proc/{original_vmm}').stat().st_uid != user.pw_uid
        result['resource_limits'] = {}
        for category, pid, expected_group, limits in (
            ('vmm', original_vmm, vm_group / ('vm-' + vm),
             {'cpu.max': '100000 100000', 'memory.max': str(768 * MIB), 'memory.swap.max': '0', 'pids.max': '256'}),
            ('storage', original_worker, disk_group / ('vol-' + state['id']),
             {'cpu.max': '100000 100000', 'memory.max': str(512 * MIB), 'memory.swap.max': '0', 'pids.max': '128'})):
            member = Path(f'/proc/{pid}/cgroup').read_text().strip().split('::', 1)[1]
            assert Path('/sys/fs/cgroup') / member.lstrip('/') == (expected_group / 'vmm' if category == 'vmm' else expected_group)
            actual = {key: (expected_group / key).read_text().strip() for key in limits}
            assert actual == limits
            result['resource_limits'][category] = actual
        payload = b'Z' * MIB
        expected = digest(payload)
        status, value = call('PUT', '/v1/sandboxes/' + vm + '/files/upload?path=/workspace/proof.data', payload, True)
        assert status == 200 and value['bytes'] == len(payload)
        result['guest_sha256'] = expected
        verify_marker('initial', expected)
        probe_policy()
        operation('stop')
        assert call('POST', '/v1/sandboxes/' + vm + '/storage/sync')[0] == 200
        first_cold = evicted('initial-cold')
        logs('initial')
        begin_phase('recovery')
        image.unlink()
        shutil.rmtree(store / 'base-metadata')
        (store / 'base-metadata').mkdir(mode=0o700)
        assert not image.exists() and placeholder.read_bytes() == bytes(65536)
        command(['systemctl', 'restart', volume_unit])
        command(['systemctl', 'restart', daemon_unit])
        wait(lambda: call('GET', '/v1/healthz')[0] == 200)
        assert record()['evicted'] and not image.exists()
        operation('start')
        new = snapshot('recovered-running')
        assert new['id'] == first_cold['id'] and new['worker']['pid'] != original_worker
        assert new['vm']['pid'] != original_vmm and not image.exists()
        verify_marker('recovery', expected)
        probe_policy()
        assert str(image) not in json.loads((vm_dir / 'spec.json').read_text())['worker_sandbox']['read_only']
        result['checks']['evicted_journal_fresh_storage_absent_base_daemon_restart'] = True
        result['storage_pids'] = [original_worker, new['worker']['pid']]
        result['checks']['default_is_unrelated_nonbootable_placeholder'] = True
        logs('recovery')
        operation('stop')
        assert call('POST', '/v1/sandboxes/' + vm + '/storage/sync')[0] == 200
        cold = evicted('recovery-cold')
        begin_phase('negative')
        # Locate a referenced object containing a complete known marker block.
        head = json.loads(read_remote(prefix + cold['id'] + '/head.json'))
        manifest = head['manifest']
        assert manifest['base']['image'] == image_hash
        result['shared_base_sha256'] = image_hash
        marker_chunk = digest(b'Z' * 65536)
        candidate = None
        for page_index, page_hash in manifest['pages'].items():
            page = read_remote(prefix + cold['id'] + '/chunks/' + page_hash)
            assert digest(page) == page_hash and page[:8] in (b'AHVMPG02', b'AHVMPG08')
            stride = 72 if page[:8] == b'AHVMPG08' else 32
            for slot in range(1024):
                at = 16 + slot * stride
                if page[at:at + 32].hex() == marker_chunk:
                    pack = page[at + 32:at + 64] if stride == 72 else bytes(32)
                    obj = pack.hex() if any(pack) else marker_chunk
                    offset = int.from_bytes(page[at + 64:at + 68], 'big') if any(pack) else 0
                    candidate = (prefix + cold['id'] + '/chunks/' + obj, offset,
                                 (int(page_index) * 1024 + slot) * 65536)
                    break
            if candidate:
                break
        assert candidate, 'known marker block is not referenced by private map'
        key, offset, disk_offset = candidate
        backup = read_remote(key)
        assert digest(backup) == key.rsplit('/', 1)[1]
        assert digest(backup[offset:offset + 65536]) == marker_chunk
        durable(evidence / 'negative-object-backup.bin', backup)
        result['negative_object'] = dict(key=key, sha256=digest(backup),
                                        marker_offset=offset, disk_offset=disk_offset)
        volume_request(cold['id'], 'attach')
        positive = direct_read(disk_offset)
        durable(evidence / 'direct-read-baseline.json', json.dumps(positive, indent=2).encode())
        assert positive == {'bytes': 65536, 'sha256': marker_chunk, 'errno': 0}
        volume_request(cold['id'], 'sync')
        volume_request(cold['id'], 'detach')
        evicted('negative-before-delete')
        assert json.loads(read_remote(prefix + cold['id'] + '/head.json'))['manifest']['pages'] == manifest['pages']
        removed_key, removed_bytes = key, backup
        remote.delete_object(Bucket=bucket, Key=key)
        try:
            read_remote(key)
            raise AssertionError('deleted object still readable')
        except ClientError as error:
            assert error.response['ResponseMetadata']['HTTPStatusCode'] == 404
        if operation('start', good=False):
            status, value = execute('sha256sum /workspace/proof.data', good=False)
            durable(evidence / 'negative-demand.json', json.dumps(value, indent=2).encode())
            assert status != 200 or value['exit_code'] != 0
            assert status != 200 or 'Input/output error' in value['stderr']
            result['checks']['missing_private_object'] = 'guest demanded marker and received EIO'
        else:
            result['checks']['missing_private_object'] = 'start failed closed before readiness'
        if not (sysdev / 'pid').exists():
            volume_request(cold['id'], 'attach')
        # Require actual I/O failure at the marker's known LBA, even when a
        # failed boot did not reach the guest agent. An unrelated start error
        # cannot satisfy this assertion. O_DIRECT bypasses host block caching.
        missing = direct_read(disk_offset)
        durable(evidence / 'negative-nbd-eio.json', json.dumps(missing, indent=2).encode())
        assert missing == {'bytes': 0, 'errno': errno.EIO}
        result['checks']['known_marker_nbd_read_baseline'] = True
        result['checks']['known_marker_nbd_read_eio'] = True
        logs('negative')
        restore_object()
        # Guest may be running with a failed read, or boot readiness may have failed.
        operation('stop')
        assert call('POST', '/v1/sandboxes/' + vm + '/storage/sync')[0] == 200
        evicted('negative-cold')
        begin_phase('restored')
        operation('start')
        verify_marker('restored', expected)
        probe_policy()
        assert not image.exists()
        logs('restored')
        result['checks']['post_negative_remote_recovery_sha256'] = True
        operation('delete')
        wait(lambda: record()['reclaimed'], 240)
        assert call('GET', '/v1/sandboxes')[1]['sandboxes'] == []
        snapshot('deleted-reclaimed')
        result['checks']['native_delete_reclamation'] = True
        result['passed'] = True
    except Exception as error:
        # Do not print SDK messages, HTTP bodies or credential-bearing configs.
        result['passed'] = False
        result['failure'] = {'phase': phase, 'type': type(error).__name__,
                             'line': traceback.extract_tb(error.__traceback__)[-1].lineno}
        print('FAIL phase=' + phase + ' type=' + type(error).__name__, flush=True)
        raise
    finally:
        phase = 'cleanup'
        cleanup_errors = []

        def cleanup_command(argv, label):
            try:
                run = command(argv, check=False)
                if run.returncode:
                    cleanup_errors.append(label + ': exit ' + str(run.returncode))
                return run
            except Exception as error:
                cleanup_errors.append(label + ': ' + type(error).__name__)
                return None

        try:
            restore_object()
        except Exception as error:
            cleanup_errors.append('remote restore: ' + type(error).__name__)
        try:
            if vm_dir.exists():
                operation('delete', good=False)
        except Exception as error:
            cleanup_errors.append('native delete: ' + type(error).__name__)
        try:
            logs('final')
        except Exception as error:
            cleanup_errors.append('evidence: ' + type(error).__name__)
        for unit in (daemon_unit, broker_unit, vm_keeper, volume_unit, disk_keeper):
            if unit in units:
                cleanup_command(['systemctl', 'stop', unit], 'stop ' + unit)
        # Only detach a device whose recorded identity belongs to this fixture.
        try:
            if (sysdev / 'pid').exists():
                state = record()
                assert state['device'] == str(device) and state['sandbox'] == str(vm_dir)
                client = state['client']
                pid = int((sysdev / 'pid').read_text())
                assert client and client['pid'] == pid
                fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
                assert fields[0] != 'Z' and int(fields[19]) == client['start']
                assert Path('/proc/sys/kernel/random/boot_id').read_text().strip() == client['boot']
                command(['/usr/sbin/nbd-client', '-d', device])
            unused()
            assert device.stat().st_uid in (0, user.pw_uid)
            os.chown(device, 0, -1)
            result['checks']['owned_nbd_detached'] = True
        except Exception as error:
            cleanup_errors.append('NBD cleanup: ' + type(error).__name__)
        cleanup_command(['systemctl', 'stop', slice_name], 'stop slice')
        for unit in units:
            run = cleanup_command(['systemctl', 'show', unit, '-p', 'ActiveState', '--value'], 'check ' + unit)
            if run and run.stdout.strip() not in ('inactive', 'failed'):
                cleanup_errors.append('unit remains active: ' + unit)
        try:
            wait(lambda: not (Path('/sys/fs/cgroup') / slice_name).exists(), 10)
            result['checks']['owned_units_and_cgroups_removed'] = True
        except Exception as error:
            cleanup_errors.append('cgroup retained: ' + type(error).__name__)
        for path in unit_files:
            path.unlink(missing_ok=True)
        cleanup_command(['systemctl', 'daemon-reload'], 'daemon reload')
        if mounted:
            run = cleanup_command(['umount', store], 'unmount')
            if run is None or run.returncode:
                cleanup_errors.append('mount retained')
            else:
                mounted = False
        if loop and not mounted:
            run = cleanup_command(['losetup', '-d', loop], 'loop detach')
            if run is None or run.returncode:
                cleanup_errors.append('loop retained')
        try:
            remaining = keys()
            result['remote_cleanup_objects'] = len(remaining)
            for key in remaining:
                assert key.startswith(prefix)
                remote.delete_object(Bucket=bucket, Key=key)
            assert keys() == []
            result['checks']['exact_fresh_prefix_removed'] = True
        except Exception as error:
            cleanup_errors.append('remote cleanup: ' + type(error).__name__)
        creds.unlink(missing_ok=True)
        envfile = private / 'daemon.env'
        envfile.unlink(missing_ok=True)
        if not mounted:
            shutil.rmtree(private, ignore_errors=True)
        result['cleanup_errors'] = cleanup_errors
        durable(evidence / 'result.json', json.dumps(result, indent=2).encode())
        print(json.dumps(result, indent=2), flush=True)
        assert not cleanup_errors, 'fixture cleanup incomplete; inspect retained result'


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print('Qualification failed: ' + type(error).__name__, file=sys.stderr)
        sys.exit(1)
