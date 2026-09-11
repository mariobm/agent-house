#!/usr/bin/env python3
"""Promote qualified assets, signed update catalog and Homebrew formula.
Usage: publish-release.py DIST VERSION FULL_GIT_SHA
Run after PR merge and acceptance qualification. Uses local gh authentication,
the offline signing key and the bucket-scoped R2 publisher credential.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import urllib.request

repo = 'mariobm/agent-house'
dist = Path(sys.argv[1]).resolve()
version, revision = sys.argv[2:4]
if len(revision) != 40 or not all(c in '0123456789abcdef' for c in revision):
    raise SystemExit('Supply the full qualified commit SHA')
tag = 'v' + version
scripts = Path(__file__).resolve().parent

def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)

def gh_json(*args):
    return json.loads(subprocess.check_output(['gh', *args]))

metadata = [dist / (p + '.json') for p in ['darwin-aarch64', 'darwin-x86_64', 'linux-x86_64']]
assets = []
for path in metadata:
    for group in json.loads(path.read_text()).values():
        for artifact in group.values():
            name = artifact['url'].rsplit('/', 1)[-1]
            if artifact['version'] != version or artifact['url'] != f'https://github.com/{repo}/releases/download/{tag}/{name}':
                raise SystemExit('Release metadata targets the wrong version or repository')
            file = dist / name
            with file.open('rb') as source:
                digest = hashlib.sha256()
                for chunk in iter(lambda:source.read(131072), b''):
                    digest.update(chunk)
            if digest.hexdigest() != artifact['sha256'] or file.stat().st_size != artifact['size']:
                raise SystemExit('Artifact does not match metadata: ' + name)
            assets.append(file)
with tempfile.TemporaryDirectory() as temp:
    temp = Path(temp)
    with urllib.request.urlopen(urllib.request.Request('https://images.ahvm.app/catalog.json', headers={'User-Agent':'AHVM/0.2.0'}), timeout=30) as response:
        (temp/'base.json').write_bytes(response.read(262145))
    run('python3', str(scripts/'assemble-catalog.py'), str(temp/'base.json'), str(temp/'payload.json'), *map(str, metadata))
    key = Path(os.environ.get('AHVM_SIGNING_KEY', str(Path.home()/'.config/ahvm-release/signing-key.pem')))
    run('python3', str(scripts/'sign-catalog.py'), str(temp/'payload.json'), str(key), str(temp/'catalog.json'))
    found = subprocess.run(['gh','release','view',tag,'--repo',repo,'--json','assets'],capture_output=True,text=True)
    if found.returncode:
        run('gh','release','create',tag,'--repo',repo,'--target',revision,'--title','AHVM '+tag,'--draft','--generate-notes')
        existing = {}
    else:
        existing = {a['name']:a for a in json.loads(found.stdout)['assets']}
    for file in assets:
        with file.open('rb') as source:
            digest = hashlib.sha256()
            for chunk in iter(lambda:source.read(131072), b''):
                digest.update(chunk)
        if file.name in existing:
            if existing[file.name].get('digest') != 'sha256:' + digest.hexdigest():
                raise SystemExit('Refusing to replace published asset ' + file.name)
        else:
            run('gh','release','upload',tag,str(file),'--repo',repo)
    run('gh','release','edit',tag,'--repo',repo,'--draft=false','--prerelease=false','--latest')
    run('python3',str(scripts/'r2-publish.py'),str(temp/'catalog.json'),'catalog.json')
    # Formula updates use a reviewable PR; no account-wide PAT is stored in CI.
    tap = temp/'tap'
    run('gh','repo','clone','mariobm/homebrew-ahvm',str(tap))
    run('git','switch','-c','feat/release-'+version,cwd=tap)
    (tap/'Formula').mkdir(exist_ok=True)
    run('python3',str(scripts/'homebrew-formula.py'),str(temp/'payload.json'),str(tap/'Formula/ahvm.rb'))
    if subprocess.check_output(['git','status','--porcelain'],cwd=tap).strip():
        run('git','add','Formula/ahvm.rb',cwd=tap)
        run('git','commit','-m','chore(release): Publish AHVM '+version,cwd=tap)
        run('git','push','-u','origin','HEAD',cwd=tap)
        url = subprocess.check_output(['gh','pr','create','--repo','mariobm/homebrew-ahvm','--head','feat/release-'+version,'--title','chore(release): Publish AHVM '+version,'--body','Update the formula to the qualified release assets and verified SHA-256 digests.'],cwd=tap,text=True).strip()
        run('gh','pr','merge',url,'--squash',cwd=tap)
    print('Published release, signed catalog and Homebrew update')
