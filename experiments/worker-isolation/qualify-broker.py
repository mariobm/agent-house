#!/usr/bin/env python3
"""Opt-in root Linux gate for the actual fixed broker launcher, no guest/NBD.

Uses an existing nonroot TEST user, a fresh /var/tmp/ahvm-worker-isolation-* root,
fixed root-owned probe ELF and bounded temporary systemd units. Never chooses a
production cgroup, service or device. The only device ioctl is KVM API version.
"""
import argparse
import ctypes
import grp
import json
import os
from pathlib import Path
import pwd
import secrets
import shutil
import signal
import socket
import subprocess
import time


def main():
    if not __debug__:
        raise RuntimeError('qualification requires safety assertions; do not use -O')
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--broker', type=Path, required=True)
    parser.add_argument('--probe', type=Path, required=True)
    parser.add_argument('--user', required=True)
    parser.add_argument('--uid-base', type=int, default=1450000000)
    args = parser.parse_args()
    assert os.geteuid() == 0 and os.environ.get('AHVM_WORKER_BROKER_TEST') == '1'
    assert args.root.parent == Path('/var/tmp') and args.root.name.startswith('ahvm-worker-isolation-') and not args.root.exists()
    assert len(str(args.root)) < 70 and args.uid_base > 65536
    user = pwd.getpwnam(args.user)
    assert user.pw_uid != 0 and user.pw_gid != 0
    root = args.root
    root.mkdir(mode=0o755)
    for name, mode in [('bin', 0o755), ('b', 0o755), ('state', 0o700), ('jails', 0o700), ('images', 0o755), ('data', 0o700)]:
        (root / name).mkdir(mode=mode)
    for name, source in [('broker', args.broker), ('probe', args.probe)]:
        shutil.copyfile(source, root / 'bin' / name)
        (root / 'bin' / name).chmod(0o755)
    data = root / 'data'
    os.chown(data, user.pw_uid, user.pw_gid)
    base = root / 'images' / 'base'
    base.write_bytes(b'readonly-base')
    base.chmod(0o644)
    os.link(base, root / 'images' / 'immutable-base-alias')
    peer = data / 'peer'
    peer.mkdir(mode=0o700)
    (peer / 'disk').write_bytes(b'private-peer')
    (peer / 'disk').chmod(0o600)
    (data / 'daemon.db').write_bytes(b'private-daemon')
    (data / 'daemon.db').chmod(0o600)
    for path in [peer, peer / 'disk', data / 'daemon.db']:
        os.chown(path, user.pw_uid, user.pw_gid)
    peer_socket = socket.socket(socket.AF_UNIX)
    peer_socket.bind(str(peer / 'control.sock'))
    os.chown(peer / 'control.sock', user.pw_uid, user.pw_gid)
    os.chmod(peer / 'control.sock', 0o600)
    peer_socket.listen(4)
    tcp = socket.socket()
    tcp.bind(('127.0.0.1', 0))
    tcp.listen(16)
    synthetic = ctypes.create_string_buffer(b'isolated-buffer')
    token = secrets.token_hex(4)
    keeper = 'ahvm-broker-proof-' + token + '-keeper'
    broker_unit = 'ahvm-broker-proof-' + token
    sock = root / 'b' / 'broker.sock'
    units = []
    records = []
    result = {'checks': {}, 'workers': []}

    def run(argv):
        return subprocess.run(list(map(str, argv)), check=True, capture_output=True, text=True, timeout=20)

    def wait(condition):
        deadline = time.monotonic() + 15
        while not condition():
            assert time.monotonic() < deadline, 'fixture deadline'
            time.sleep(0.02)

    def rpc(action, name='', role='vmm', worker=None):
        # SO_PEERCRED authorization is real: request in a process with only the
        # configured daemon identity, never a root-selected UID inside payload.
        incoming, outgoing = os.pipe()
        child = os.fork()
        if child == 0:
            try:
                os.close(incoming)
                os.setgroups([])
                os.setresgid(user.pw_gid, user.pw_gid, user.pw_gid)
                os.setresuid(user.pw_uid, user.pw_uid, user.pw_uid)
                conn = socket.socket(socket.AF_UNIX)
                conn.settimeout(15)
                conn.connect(str(sock))
                conn.sendall((json.dumps(dict(action=action, id=name, role=role, worker=worker)) + '\n').encode())
                data = bytearray()
                while not data.endswith(b'\n'):
                    chunk = conn.recv(16385)
                    assert chunk and len(data) + len(chunk) <= 16384
                    data.extend(chunk)
                os.write(outgoing, data)
                os._exit(0)
            except BaseException as error:
                os.write(outgoing, (json.dumps({'transport_error': str(error)}) + '\n').encode())
                os._exit(1)
        os.close(outgoing)
        with os.fdopen(incoming) as stream:
            reply = json.loads(stream.readline())
        status = os.waitpid(child, 0)[1]
        assert status == 0, reply
        return reply

    def ctl(path, value):
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(5)
            conn.connect(str(path))
            conn.sendall(value.encode())
            return conn.recv(4096).decode()

    def save(path, value):
        path.write_text(json.dumps(value))
        path.chmod(0o600)
        os.chown(path, user.pw_uid, user.pw_gid)

    def fixture(name):
        directory = data / name
        directory.mkdir(mode=0o700)
        os.chown(directory, user.pw_uid, user.pw_gid)
        for part in ['sock', 'net', 'tmp', 'runtime']:
            (directory / part).mkdir(mode=0o700)
            os.chown(directory / part, user.pw_uid, user.pw_gid)
        (directory / 'root.qcow2').write_bytes(b'own-disk')
        os.chown(directory / 'root.qcow2', user.pw_uid, user.pw_gid)
        common = [str(path) for path in map(Path, ['/lib', '/lib64', '/usr/lib', '/usr/lib64', '/etc/localtime', '/proc/cpuinfo', '/proc/self/fd']) if path.exists()]
        settings = dict(daemon_uid=user.pw_uid, peer_file=str(peer / 'disk'), peer_socket=str(peer / 'control.sock'), daemon_file=str(data / 'daemon.db'),
                        target_pid=os.getpid(), target_memory=ctypes.addressof(synthetic), host_tcp=f'127.0.0.1:{tcp.getsockname()[1]}')
        vmm = dict(worker_sandbox=dict(read_only=[str(directory), str(base), str(directory / 'net'), *common],
            read_write=[str(directory / 'root.qcow2'), *(str(directory / part) for part in ['sock', 'tmp', 'runtime']), '/dev/kvm', '/dev/null', '/dev/urandom'],
            unix_connect=[str(directory / 'net')]), trusted_host_socket_access=False, vcpus=1, mem_mib=128,
            root_disk=str(directory / 'root.qcow2'), root_disk_format='qcow2',
            control_socket_uds=str(directory / 'sock/control.sock'), net_uds=str(directory / 'net/net.sock'), isolation_probe=settings)
        netd = dict(ethernet_contract=1, socket=str(directory / 'net/net.sock'), resolver='1.1.1.1',
            worker_sandbox=dict(read_only=[str(directory / 'net.json'), *common], read_write=[str(directory / 'net'), '/dev/urandom'], unix_connect=[]), isolation_probe=settings)
        save(directory / 'spec.json', vmm)
        save(directory / 'net.json', netd)
        parent = cgroups / ('vm-' + name)
        parent.mkdir()
        for key, value in [('memory.max', '536870912'), ('memory.swap.max', '0'), ('pids.max', '128'), ('cpu.max', '100000 100000')]:
            (parent / key).write_text(value)
        return directory, vmm

    try:
        run(['systemd-run', '--quiet', '--collect', '--unit=' + keeper, '--property=Delegate=cpu memory pids', '--property=DelegateSubgroup=keeper', '--property=MemoryMax=1G', '--property=TasksMax=512', '/bin/sleep', 'infinity'])
        units.append(keeper)
        relative = run(['systemctl', 'show', keeper, '-p', 'ControlGroup', '--value']).stdout.strip()
        assert relative.startswith('/system.slice/ahvm-broker-proof-')
        cgroups = Path('/sys/fs/cgroup') / relative.lstrip('/')
        (cgroups / 'cgroup.subtree_control').write_text('+cpu +memory +pids')
        config = dict(socket=str(sock), state_dir=str(root / 'state'), jail_dir=str(root / 'jails'), data_dir=str(data), cgroup_root=str(cgroups),
            daemon_uid=user.pw_uid, daemon_gid=user.pw_gid, uid_base=args.uid_base, gid_base=args.uid_base, identity_count=8,
            vmm_bin=str(root / 'bin/probe'), netd_bin=str(root / 'bin/probe'), gpu_bin=None, lib_path='/usr/lib', image_roots=[str(root / 'images')], devices=[])
        cfg = root / 'broker.json'
        cfg.write_text(json.dumps(config))
        cfg.chmod(0o600)
        run(['systemd-run', '--quiet', '--collect', '--unit=' + broker_unit, '--property=Type=notify', '--property=NotifyAccess=main', '--property=NoNewPrivileges=yes',
            '--property=UMask=0077', '--property=LimitCORE=0', '--property=User=root', '--property=Group=root',
            '--property=AmbientCapabilities=CAP_SETUID CAP_SETGID', '--property=CapabilityBoundingSet=CAP_SYS_ADMIN CAP_SYS_CHROOT CAP_SETUID CAP_SETGID CAP_SETPCAP CAP_MKNOD CAP_CHOWN CAP_DAC_OVERRIDE CAP_DAC_READ_SEARCH CAP_KILL',
            '--property=RestrictNamespaces=user mnt', '--property=KillMode=process', root / 'bin/broker', cfg])
        units.append(broker_unit)
        wait(sock.exists)
        assert rpc('check')['error'] is None
        directory, spec = fixture('own')
        for role in ['netd', 'vmm']:
            reply = rpc('launch', 'own', role)
            assert reply['error'] is None, reply
            records.append(reply['worker'])
            path = directory / ('net' if role == 'netd' else 'runtime') / 'proof.json'
            wait(path.exists)
            proof = json.loads(path.read_text())
            result['workers'].append(proof)
            assert path.stat().st_uid == user.pw_uid
            status = Path(f'/proc/{reply["worker"]["pid"]}/status').read_text()
            for field in ['CapEff', 'CapPrm', 'CapInh', 'CapBnd', 'CapAmb']:
                assert f'{field}:\t0000000000000000' in status, status
            status_fields = dict(line.split(':', 1) for line in status.splitlines() if ':' in line)
            assert status_fields['NoNewPrivs'].strip() == '1' and not status_fields['Groups'].split()
        assert records[0]['isolation']['uid'] != records[1]['isolation']['uid']
        assert ctl(directory / 'sock/control.sock', 'CONNECT') == 'OK'
        result['checks']['host_and_vmm_connect_own_0600_gateway'] = True
        staged = directory / 'runtime/bundle.new'
        published = directory / 'bundle.pending'
        published.mkdir(mode=0o700)
        os.chown(published, user.pw_uid, user.pw_gid)
        for name in ['memory.img', 'checkpoint.bin', 'manifest.json']:
            assert (staged / name).stat().st_uid == user.pw_uid
            (staged / name).rename(published / name)
        staged.rmdir()
        save(published / 'backing-image.json', str(base))
        assert ctl(directory / 'sock/control.sock', 'PUBLISH').startswith('PASS')
        assert (published / 'backing-image.json').stat().st_mode & 0o777 == 0o600
        result['checks']['fresh_snapshot_directory_protects_metadata_from_retained_fd'] = True
        forged = dict(records[1], pid=os.getpid(), starttime=1)
        assert rpc('stop', 'own', 'vmm', forged)['error']
        result['checks']['forged_record_not_signalled'] = True
        child = int(ctl(directory / 'sock/control.sock', 'FORK'))
        assert rpc('stop', 'own', 'vmm', records[1])['error'] is None
        wait(lambda: not Path(f'/proc/{child}/fd').exists() or not list(Path(f'/proc/{child}/fd').iterdir()))
        assert 'populated 0' in (cgroups / 'vm-own/vmm/cgroup.events').read_text()
        assert ctl(directory / 'net/net.sock', 'PING') == 'OK'
        result['checks']['vmm_descendants_drained_gateway_survives'] = True
        # Canonical read-root symlink and linked writable inode are rejected
        # before allocating any identity or starting an untrusted child.
        bad, badspec = fixture('bad')
        escape = root / 'images/escape'
        escape.symlink_to(peer / 'disk')
        before = json.loads((root / 'state/registry.json').read_text())['next_identity']
        badspec['worker_sandbox']['read_only'].append(str(escape))
        save(bad / 'spec.json', badspec)
        assert rpc('launch', 'bad')['error']
        assert json.loads((root / 'state/registry.json').read_text())['next_identity'] == before
        badspec['worker_sandbox']['read_only'].remove(str(escape))
        save(bad / 'spec.json', badspec)
        (bad / 'root.qcow2').unlink()
        os.link(peer / 'disk', bad / 'root.qcow2')
        assert rpc('launch', 'bad')['error']
        result['checks']['readonly_symlink_and_writable_hardlink_rejected_pre_spawn'] = True
        assert rpc('stop', 'own', 'netd', records[0])['error'] is None
        assert all(not entry.name.startswith('own-') for entry in (root / 'jails').iterdir())
        result['checks']['exact_empty_jail_backings_removed'] = True
        # Consume the remaining small fixture-only identity capacity. Exhaustion
        # must not spawn a child or create a role/jail, and no counter resets.
        for index in range(6):
            name = 'capacity-' + str(index)
            directory, _ = fixture(name)
            reply = rpc('launch', name, 'netd')
            assert reply['error'] is None, reply
            records.append(reply['worker'])
            wait((directory / 'net/proof.json').exists)
            assert rpc('stop', name, 'netd', reply['worker'])['error'] is None
        exhausted, _ = fixture('exhausted')
        reply = rpc('launch', 'exhausted', 'netd')
        assert 'identities exhausted' in reply['error']
        assert not (cgroups / 'vm-exhausted/netd').exists()
        assert json.loads((root / 'state/registry.json').read_text())['next_identity'] == 8
        result['checks']['identity_exhaustion_prevents_spawn_and_preserves_counter'] = True
        active_compact = subprocess.run([str(root / 'bin/broker'), '--compact', str(cfg)], capture_output=True, text=True, timeout=15)
        assert active_compact.returncode != 0, 'maintenance must not race a live broker'
        run(['systemctl', 'stop', broker_unit])
        # Stopped existing VM directories must retain their latest records.
        run([root / 'bin/broker', '--compact', cfg])
        assert len(json.loads((root / 'state/registry.json').read_text())['entries']) == 8
        for name in ['own', *(f'capacity-{index}' for index in range(6))]:
            shutil.rmtree(data / name)
        run([root / 'bin/broker', '--compact', cfg])
        compacted = json.loads((root / 'state/registry.json').read_text())
        assert compacted['entries'] == {} and compacted['next_identity'] == 8
        result['checks']['offline_compaction_retains_existing_vms_and_never_reuses_identities'] = True
        records.clear()  # already authenticated/drained; broker is now stopped
        result['result'] = 'PASS'
        (root / 'evidence.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result, indent=2))
    finally:
        for record in reversed(records):
            try: rpc('stop', record['id'], record['isolation']['role'], record)
            except Exception: pass
        for unit in reversed(units):
            subprocess.run(['systemctl', 'stop', unit], capture_output=True, timeout=15)
        peer_socket.close()
        tcp.close()


if __name__ == '__main__':
    main()
