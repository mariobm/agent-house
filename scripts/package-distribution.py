#!/usr/bin/env python3
"""Split a qualified bundle into a server archive and standalone CLI.
Usage: package-distribution.py BUNDLE OUTPUT VERSION PLATFORM
Images are published separately; preserves all bundled third-party notices.
"""
import gzip
import hashlib
import json
import re
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile

bundle, output = map(Path, sys.argv[1:3])
version, platform = sys.argv[3:5]
output.mkdir(parents=True, exist_ok=True)

def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(131072), b''):
            value.update(chunk)
    return value.hexdigest()

def info(path, unpacked):
    sha = digest(path)
    return dict(version=version, url=f'https://github.com/mariobm/agent-house/releases/download/v{version}/{path.name}', sha256=sha, size=path.stat().st_size, unpacked_size=unpacked, guest_abi=1, state_abi=1)

cli = output / f'ahvm-{version}-{platform}.gz'
with (bundle / 'bin/ahvm').open('rb') as src, gzip.open(cli, 'wb', compresslevel=6) as dest:
    shutil.copyfileobj(src, dest)
records = {'cli': {platform: info(cli, (bundle / 'bin/ahvm').stat().st_size)}, 'server': {}}
# Keep the legacy single-binary artifact for existing updaters. New clients
# consume one archive containing the CLI, viewer (when qualified), and notices.
with tempfile.TemporaryDirectory() as temp:
    stage = Path(temp)
    shutil.copy2(bundle / 'bin/ahvm', stage / 'ahvm')
    if platform.startswith('darwin-'):
        viewer = bundle / 'desktop'
        if not (viewer / 'ahvm-desktop').is_file():
            raise SystemExit('macOS client requires the bundled desktop viewer')
        shutil.copy2(viewer / 'ahvm-desktop', stage / 'ahvm-desktop')
        for name in ['AHVM-LICENSE', 'noVNC-LICENSE.txt', 'noVNC-AUTHORS', 'pako-LICENSE']:
            shutil.copy2(viewer / name, stage / ('ahvm-desktop-' + name))
    archive = output / f'ahvm-client-{version}-{platform}.tar.gz'
    with tarfile.open(archive, 'w:gz', format=tarfile.USTAR_FORMAT, compresslevel=6) as tar:
        for path in sorted(stage.iterdir()):
            tar.add(path, arcname=path.name)
    records['client'] = {platform: info(archive, sum(p.stat().st_size for p in stage.iterdir()))}

if platform == 'linux-x86_64':
    for folder in ['bin', 'lib']:
        for binary in (bundle / folder).glob('*'):
            versions = [tuple(map(int, pair)) for pair in re.findall(rb'GLIBC_([0-9]+)\.([0-9]+)', binary.read_bytes())]
            if versions and max(versions) > (2, 35):
                raise SystemExit(f'{binary.name} requires glibc {max(versions)}; rebuild in the qualified glibc 2.35 environment or use a static build')
    with tempfile.TemporaryDirectory() as temp:
        stage = Path(temp) / 'bundle'
        shutil.copytree(bundle, stage, ignore=lambda directory, names: ['share'] if Path(directory) == bundle else [])
        (stage / 'share').mkdir()
        checksums = []
        for path in sorted(stage.rglob('*')):
            if path.is_file() and path.name != 'SHA256SUMS':
                checksums.append(digest(path) + '  ' + str(path.relative_to(stage)))
        (stage / 'SHA256SUMS').write_text('\n'.join(checksums) + '\n')
        archive = output / f'ahvm-server-{version}-{platform}.tar.gz'
        size = sum(p.stat().st_size for p in stage.rglob('*') if p.is_file())
        with tarfile.open(archive, 'w:gz', compresslevel=6) as tar:
            tar.add(stage, arcname='.')
        records['server'][platform] = info(archive, size)
(output / f'{platform}.json').write_text(json.dumps(records, indent=2) + '\n')
