#!/usr/bin/env python3
"""Opt-in image gate against a disposable daemon; one 4 GiB sandbox, cleaned up.

AHVM_ENDPOINT and AHVM_TOKEN_FILE select an empty test installation with the
development image. Usage: test-dev-image.py /path/to/ahvm
No provider credentials or paid model requests are used.
"""
import json
import subprocess
import sys
import time
import uuid

binary = sys.argv[1]
sandbox = 'dev-image-' + uuid.uuid4().hex[:10]


def cli(*args):
    result = subprocess.run([binary, *args], capture_output=True, text=True, timeout=360)
    assert result.returncode == 0, (args, result.stdout, result.stderr)
    return result.stdout


def obj(*args):
    return json.loads(cli('--json', *args))


assert obj('list')['sandboxes'] == [], 'Use an empty disposable installation'
started = time.monotonic()
created = False
try:
    obj('create', sandbox, '--cpus', '2', '--memory', '4096')
    created = True
    versions = cli('exec', sandbox, '--', 'ahvm-dev', 'bash', '-lc', '''
set -euo pipefail
test "$(id -un)" = developer
test "$HOME" = /home/developer
test "$PWD" = /workspace
sudo -n true
node --version; bun --version; python --version
claude --version; codex --version; opencode --version; pi --version
git --version; gcc --version | head -1
''')
    print(versions, flush=True)
    cli('exec', sandbox, '--', 'bash', '-ec',
        'export DEBIAN_FRONTEND=noninteractive; apt-get update -qq; '
        'apt-get install -y -qq --no-install-recommends hello; hello >/dev/null')
    cli('exec', sandbox, '--', 'ahvm-dev', 'bash', '-lc', '''
set -euo pipefail
python -m venv .venv
.venv/bin/pip --disable-pip-version-check -q install requests==2.32.5
.venv/bin/python -c 'import requests; assert requests.get("https://example.com",timeout=20).status_code == 200'
npm init -y >/dev/null
npm install --no-audit --no-fund lodash@4.17.21
node -e 'if(require("lodash").sum([1,2,3])!==6)process.exit(1)'
bun -e 'if([1,2,3].reduce((a,b)=>a+b)!==6)process.exit(1)'
printf '#include <stdio.h>\\nint main(void){puts("C-OK");}\\n' > hello.c
cc hello.c -o hello
test "$(./hello)" = C-OK
printf persistent > marker
''')
    session = obj('session', 'create', sandbox, '--pty', '--', 'ahvm-dev',
                  'bash', '-lc', 'test -t 0 && printf PTY-OK')['session_id']
    output = cli('session', 'read', sandbox, session, '--follow')
    assert 'PTY-OK' in output, output
    cli('stop', sandbox)
    cli('start', sandbox)
    assert cli('exec', sandbox, '--', 'ahvm-dev', 'cat', '/workspace/marker') == 'persistent'
    cli('exec', sandbox, '--', 'ahvm-dev', 'bash', '-lc',
        'test "$(./hello)" = C-OK && .venv/bin/python -c "import requests" && codex --version')
    print(f'PASS: Ubuntu tools, apt/npm/pip, HTTPS, PTY, stop/start persistence ({time.monotonic()-started:.2f}s)', flush=True)
finally:
    if created:
        cli('delete', sandbox)
