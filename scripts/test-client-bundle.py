#!/usr/bin/env python3
"""Exercise packaging and curl installation without network or an installed host."""
import base64
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

repo = Path(__file__).resolve().parent.parent
with tempfile.TemporaryDirectory() as temp:
    root = Path(temp)
    bundle, output, mocks = root/'bundle', root/'output', root/'mocks'
    (bundle/'bin').mkdir(parents=True)
    (bundle/'desktop').mkdir()
    mocks.mkdir()
    (bundle/'bin/ahvm').write_text('#!/bin/sh\necho "ahvm 0.2.1"\n')
    (bundle/'bin/ahvm').chmod(0o755)
    (bundle/'desktop/ahvm-desktop').write_text('#!/bin/sh\necho viewer\n')
    (bundle/'desktop/ahvm-desktop').chmod(0o755)
    for name in ['AHVM-LICENSE', 'noVNC-LICENSE.txt', 'noVNC-AUTHORS', 'pako-LICENSE']:
        (bundle/'desktop'/name).write_text('test notice\n')
    subprocess.run(['python3', str(repo/'scripts/package-distribution.py'), str(bundle), str(output), '0.2.1', 'darwin-aarch64'], check=True)
    metadata = json.loads((output/'darwin-aarch64.json').read_text())
    archive = output/Path(metadata['client']['darwin-aarch64']['url']).name
    with tarfile.open(archive) as tar:
        assert set(tar.getnames()) == {'ahvm', 'ahvm-desktop', 'ahvm-desktop-AHVM-LICENSE', 'ahvm-desktop-noVNC-LICENSE.txt', 'ahvm-desktop-noVNC-AUTHORS', 'ahvm-desktop-pako-LICENSE'}
    (mocks/'uname').write_text('#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
    (mocks/'curl').write_text('''#!/usr/bin/env python3
import os,sys,shutil
from pathlib import Path
a=sys.argv[1:]; url=next(v for v in a if v.startswith('https://'))
source=Path(os.environ['FIXTURE'])/('catalog.json' if url.endswith('catalog.json') else url.rsplit('/',1)[1])
shutil.copyfile(source,a[a.index('-o')+1])
''')
    for p in mocks.iterdir(): p.chmod(0o755)
    def install(catalog, destination, success=True):
        (output/'catalog.json').write_text(json.dumps({'payload': base64.b64encode(json.dumps(catalog).encode()).decode()}))
        env = {**os.environ, 'PATH': str(mocks)+':'+os.environ['PATH'], 'FIXTURE': str(output), 'AHVM_BIN_DIR': str(destination)}
        result = subprocess.run(['bash', str(repo/'scripts/install-cli.sh')], env=env, capture_output=True, text=True)
        assert (result.returncode == 0) == success, result.stderr
    dest = root/'installed'
    install(metadata, dest)
    assert subprocess.check_output([dest/'ahvm-desktop']).strip() == b'viewer'
    assert (dest/'ahvm-desktop-noVNC-LICENSE.txt').is_file()
    original = (dest/'ahvm').read_bytes()
    archive.write_bytes(b'corrupt')
    install(metadata, dest, success=False)
    assert (dest/'ahvm').read_bytes() == original
    legacy = root/'legacy'
    install({'cli': metadata['cli']}, legacy)
    assert (legacy/'ahvm').is_file() and not (legacy/'ahvm-desktop').exists()
print('client bundle: archive contents, companion install, checksum rejection and legacy install pass')
