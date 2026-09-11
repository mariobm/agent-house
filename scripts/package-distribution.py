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

def info(path, unpacked):
    with path.open('rb') as f:
        sha = hashlib.file_digest(f, 'sha256').hexdigest()
    return dict(version=version, url=f'https://github.com/mariobm/agent-house/releases/download/v{version}/{path.name}', sha256=sha, size=path.stat().st_size, unpacked_size=unpacked, guest_abi=1, state_abi=1)

cli = output / f'ahvm-{version}-{platform}.gz'
with (bundle / 'bin/ahvm').open('rb') as src, gzip.open(cli, 'wb', compresslevel=6) as dest:
    shutil.copyfileobj(src, dest)
records = {'cli': {platform: info(cli, (bundle / 'bin/ahvm').stat().st_size)}, 'server': {}}
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
                with path.open('rb') as f:
                    checksums.append(hashlib.file_digest(f, 'sha256').hexdigest() + '  ' + str(path.relative_to(stage)))
        (stage / 'SHA256SUMS').write_text('\n'.join(checksums) + '\n')
        archive = output / f'ahvm-server-{version}-{platform}.tar.gz'
        size = sum(p.stat().st_size for p in stage.rglob('*') if p.is_file())
        with tarfile.open(archive, 'w:gz', compresslevel=6) as tar:
            tar.add(stage, arcname='.')
        records['server'][platform] = info(archive, size)
(output / f'{platform}.json').write_text(json.dumps(records, indent=2) + '\n')
