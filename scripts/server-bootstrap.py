#!/usr/bin/env python3
"""Server install/upgrade transport, embedded in the trusted CLI.
Requires root, Linux x86_64, Python 3, OpenSSL 3, GNU tar and systemd.
"""
import base64
import fcntl
import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

PUBLIC_KEY = r"""@PUBLIC_KEY@"""
CATALOG = 'https://images.ahvm.app/catalog.json'
PREFIX = Path('/opt/ahvm-rust')
CONFIG = Path('/etc/ahvm-rust')
DATA = Path('/var/lib/ahvm-rust')


def fetch(url, target, maximum, expected=None):
    if not url.startswith('https://'):
        raise ValueError('Downloads require HTTPS')
    digest = hashlib.sha256()
    total = 0
    with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "AHVM/0.2.0"}), timeout=60) as response, target.open('wb') as out:
        if not response.url.startswith('https://'):
            raise ValueError('Insecure download redirect')
        while True:
            chunk = response.read(1024 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > maximum:
                raise ValueError('Download exceeds signed size')
            digest.update(chunk)
            out.write(chunk)
    if expected and (total != maximum or digest.hexdigest() != expected):
        raise ValueError('Download checksum/size mismatch')


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def health():
    token = (CONFIG / 'admin.token').read_text().strip()
    request = urllib.request.Request('http://127.0.0.1:8080/v1/sandboxes?limit=1', headers={'Authorization': 'Bearer ' + token})
    for _ in range(30):
        try:
            with urllib.request.urlopen(request, timeout=2) as response:
                if response.status == 200:
                    return
        except OSError:
            time.sleep(1)
    raise RuntimeError('Upgraded daemon failed health check')


def upgrade_runtime(stage, temp):
    backup = PREFIX.with_name(PREFIX.name + '.previous')
    older = temp / 'older-rollback'
    # Stabilize legacy bundled images before rotating runtime directories.
    image = PREFIX / 'share/base.ext4'
    link = Path(os.readlink(image)) if image.is_symlink() else None
    if link is None or not link.is_absolute() or PREFIX in link.parents or backup in link.parents:
        cache = Path('/var/lib/ahvm-images')
        cache.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(prefix='retained-', suffix='.ext4', dir=cache, delete=False) as saved:
            retained = Path(saved.name)
        try:
            run('cp', '--reflink=auto', '--sparse=always', str(image.resolve()), str(retained))
            retained.chmod(0o644)
        except BaseException:
            retained.unlink(missing_ok=True)
            raise
        link = retained
    run('systemctl', 'stop', 'ahvm-rust')
    db_backup = temp / 'database'
    saved_database = False
    swapped = False
    try:
        db_backup.mkdir()
        for p in DATA.glob('daemon.db*'):
            shutil.copy2(p, db_backup / p.name)
            os.chown(db_backup / p.name, p.stat().st_uid, p.stat().st_gid)
        saved_database = True
        if backup.exists():
            backup.rename(older)
        PREFIX.rename(backup)
        swapped = True
        stage.rename(PREFIX)
        (PREFIX / 'share/base.ext4').unlink()
        (PREFIX / 'share/base.ext4').symlink_to(link)
        # Existing services also need render-node access after a GPU-capable upgrade.
        if (PREFIX / 'bin/ahvm-vmm-gpu').is_file():
            import grp
            user = subprocess.check_output(['systemctl', 'show', 'ahvm-rust', '-p', 'User', '--value'], text=True).strip()
            for node in Path('/dev/dri').glob('renderD*'):
                group = grp.getgrgid(node.stat().st_gid).gr_name
                if group != 'root' and user:
                    run('usermod', '-a', '-G', group, user)
        run('systemctl', 'start', 'ahvm-rust')
        health()
    except BaseException:
        run('systemctl', 'stop', 'ahvm-rust')
        if swapped:
            if PREFIX.exists():
                PREFIX.rename(temp / 'failed-runtime')
            backup.rename(PREFIX)
        if older.exists():
            older.rename(backup)
        if saved_database:
            for p in DATA.glob('daemon.db*'):
                p.unlink()
            for p in db_backup.iterdir():
                shutil.copy2(p, DATA / p.name)
                os.chown(DATA / p.name, p.stat().st_uid, p.stat().st_gid)
        run('systemctl', 'start', 'ahvm-rust')
        raise
    shutil.copytree(db_backup, backup / 'rollback-database')


def main():
    upgrade = sys.argv[1:] == ['--upgrade']
    if sys.argv[1:] and not upgrade:
        raise ValueError('Usage: server-bootstrap.py [--upgrade]')
    if os.geteuid() != 0 or os.uname().sysname != 'Linux' or os.uname().machine != 'x86_64':
        raise ValueError('Requires root on Linux x86_64')
    if not Path('/dev/kvm').exists():
        raise ValueError('KVM is required')
    for tool in ['openssl', 'tar', 'systemctl']:
        if not shutil.which(tool):
            raise ValueError('Missing prerequisite: ' + tool)
    with open('/var/lib/ahvm-upgrade.lock', 'w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if upgrade:
            if not (CONFIG / 'daemon.env').is_file() or not PREFIX.is_dir():
                raise ValueError('No standard AHVM installation found')
            # Do not silently operate on a custom installation or data location.
            settings = dict(line.split('=', 1) for line in (CONFIG / 'daemon.env').read_text().splitlines() if '=' in line)
            if settings.get('AHVM_DATA_DIR') != str(DATA) or settings.get('AHVM_LISTEN') != '127.0.0.1:8080':
                raise ValueError('Custom installations require manual upgrade')
        elif any(p.exists() for p in [PREFIX, CONFIG, DATA, Path('/usr/local/bin/ahvm')]):
            raise ValueError('Installation exists; use ahvm host upgrade')
        with tempfile.TemporaryDirectory(prefix='.ahvm-release-', dir='/opt') as temp:
            temp = Path(temp)
            fetch(CATALOG, temp / 'catalog.json', 262144)
            envelope = json.loads((temp / 'catalog.json').read_bytes())
            (temp / 'payload').write_bytes(base64.b64decode(envelope['payload'], validate=True))
            (temp / 'signature').write_bytes(base64.b64decode(envelope['signature'], validate=True))
            (temp / 'public.pem').write_text(PUBLIC_KEY)
            run('openssl', 'pkeyutl', '-verify', '-pubin', '-inkey', str(temp / 'public.pem'), '-rawin', '-in', str(temp / 'payload'), '-sigfile', str(temp / 'signature'), stdout=subprocess.DEVNULL)
            catalog = json.loads((temp / 'payload').read_bytes())
            if catalog['expires'] <= time.time():
                raise ValueError('Release catalog expired')
            artifact = catalog['server']['linux-x86_64']
            if artifact['guest_abi'] != 1 or artifact.get('state_abi') != 1 or not 0 < artifact['size'] <= 4 * 1024**3:
                raise ValueError('Incompatible server release')
            if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', artifact['version']):
                raise ValueError('Invalid stable server version')
            if upgrade:
                installed = subprocess.check_output([PREFIX / 'bin/ahvm', '--version'], text=True).strip().removeprefix('ahvm ')
                if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', installed):
                    raise ValueError('Unknown installed server version; manual upgrade required')
                abi_file = PREFIX / 'state-abi'
                current_abi = int(abi_file.read_text()) if abi_file.exists() else (1 if installed in ['0.1.0', '0.2.0'] else 0)
                if current_abi != artifact['state_abi']:
                    raise ValueError('State format change requires an explicit migration')
                if tuple(map(int, installed.split('.'))) >= tuple(map(int, artifact['version'].split('.'))):
                    print('Server is up to date (' + installed + ')')
                    return
            print('Downloading AHVM server ' + artifact['version'], flush=True)
            fetch(artifact['url'], temp / 'server.tar.gz', artifact['size'], artifact['sha256'])
            stage = temp / 'runtime'
            stage.mkdir()
            total = 0
            with tarfile.open(temp / 'server.tar.gz') as archive:
                for member in archive:
                    p = Path(member.name)
                    total += member.size
                    if p.is_absolute() or '..' in p.parts or not (member.isfile() or member.isdir()) or total > artifact['unpacked_size']:
                        raise ValueError('Unsafe server archive')
            run('tar', '-xzf', str(temp / 'server.tar.gz'), '-C', str(stage), '--no-same-owner', '--no-same-permissions')
            version = subprocess.check_output([stage / 'bin/ahvm', '--version'], text=True).strip()
            if version != 'ahvm ' + artifact['version']:
                raise ValueError('Server bundle version mismatch')
            (stage / 'state-abi').write_text(str(artifact['state_abi']) + '\n')
            image = Path('/var/lib/ahvm-images/default.ext4')
            if not image.is_file():
                run(str(stage / 'bin/ahvm'), 'image', 'pull', 'ubuntu-dev', env={**os.environ, 'AHVM_CONFIG_DIR': str(temp / 'cli-config')})
            (stage / 'share').mkdir(exist_ok=True)
            (stage / 'share/base.ext4').symlink_to(image)
            if not upgrade:
                run('bash', str(stage / 'install.sh'), str(stage))
                Path('/usr/local/bin').mkdir(parents=True, exist_ok=True)
                Path('/usr/local/bin/ahvm').symlink_to(PREFIX / 'bin/ahvm')
                health()
            else:
                upgrade_runtime(stage, temp)
            print('AHVM server ready.', flush=True)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print('ahvm server: ' + str(error), file=sys.stderr)
        sys.exit(1)
