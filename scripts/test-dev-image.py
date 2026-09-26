#!/usr/bin/env python3
"""Opt-in image gate against a disposable daemon; one 4 GiB sandbox, cleaned up.

AHVM_ENDPOINT and AHVM_TOKEN_FILE select an empty test installation with the
development image. Usage: test-dev-image.py /path/to/ahvm
No provider credentials or paid model requests are used.
"""
import json
from pathlib import Path
import subprocess
import sys
import time
import uuid

binary = sys.argv[1]
sandbox = 'dev-image-' + uuid.uuid4().hex[:10]
pins = dict(line.split('=', 1) for line in
            (Path(__file__).resolve().parents[1] / 'images/ubuntu-dev/versions.env').read_text().splitlines()
            if line and not line.startswith('#'))

# Runs inside the disposable guest. All server state is temporary; neither
# provider authentication nor a Session/prompt is created.
OPENCODE_GATE = r'''
import base64, json, os, pwd, secrets, signal, socket, subprocess, sys, tempfile, time
import urllib.error, urllib.request
from pathlib import Path

expected = sys.argv[1]
image_pins = dict(line.split('=', 1) for line in
                  Path('/usr/local/share/ahvm/image-versions.env').read_text().splitlines()
                  if line and not line.startswith('#'))
assert image_pins['OPENCODE_VERSION'] == expected, 'image OpenCode pin mismatch'
assert os.getuid() == pwd.getpwnam('ahvm').pw_uid, 'gate must run as ahvm'
assert os.environ.get('HOME') == '/home/ahvm', 'guest HOME mismatch'
assert os.getcwd() == '/workspace', 'guest workspace mismatch'
package = Path('/opt/ahvm-tools/node_modules/@opencode/cli/package.json')
metadata = json.loads(package.read_text())
assert metadata['name'] == '@opencode/cli' and metadata['version'] == expected, 'released package mismatch'
for path in (Path('/opt/ahvm-tools'), package, Path('/opt/ahvm-tools/package-lock.json'), Path('/usr/local/bin/opencode').resolve()):
    assert path.stat().st_uid == 0, 'installed tool must be root-owned'
version = subprocess.run(['opencode', '--version'], check=True, capture_output=True, text=True, timeout=15).stdout.strip()
assert version == 'opencode v' + expected, 'OpenCode CLI version mismatch'

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        raise AssertionError('server redirected request')

opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
with socket.socket() as reservation:
    reservation.bind(('127.0.0.1', 0))
    port = reservation.getsockname()[1]
password = secrets.token_urlsafe(32)
authorization = 'Basic ' + base64.b64encode(('opencode:' + password).encode()).decode()
origin = 'http://127.0.0.1:' + str(port)

def request(path, auth=authorization):
    headers = {'Authorization': auth} if auth else {}
    response = opener.open(urllib.request.Request(origin + path, headers=headers), timeout=3)
    with response:
        assert 'application/json' in response.headers.get('Content-Type', ''), 'API must return JSON'
        raw = response.read(8 * 1024 * 1024 + 1)
        assert len(raw) <= 8 * 1024 * 1024, 'API response limit'
        return json.loads(raw)

process = None
with tempfile.TemporaryDirectory(prefix='ahvm-opencode-image-gate-') as directory:
    env = os.environ.copy()
    for name in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_STATE_HOME', 'XDG_CACHE_HOME'):
        env[name] = directory + '/' + name.lower()
        Path(env[name]).mkdir()
    env['OPENCODE_PASSWORD'] = password
    env.pop('OPENCODE_SERVER_PASSWORD', None)
    try:
        process = subprocess.Popen(['opencode', 'serve', '--hostname', '127.0.0.1', '--port', str(port)],
                                   cwd='/workspace', env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        deadline = time.monotonic() + 45
        while time.monotonic() < deadline:
            assert process.poll() is None, 'OpenCode server exited before ready'
            try:
                info = request('/api/info')
                break
            except (urllib.error.URLError, TimeoutError):
                time.sleep(0.2)
        else:
            raise AssertionError('OpenCode server startup deadline')
        assert info.get('version') == expected, 'server version mismatch'
        native_pid = info.get('pid')
        assert isinstance(native_pid, int) and os.getpgid(native_pid) == process.pid, 'unexpected server process'
        status = Path('/proc/' + str(native_pid) + '/status').read_text().splitlines()
        uid = next(line.split()[1] for line in status if line.startswith('Uid:'))
        assert int(uid) == os.getuid(), 'server runs under wrong guest user'
        listeners = subprocess.run(['ss', '-H', '-ltn'], check=True, capture_output=True, text=True, timeout=5).stdout.splitlines()
        listeners = [line.split()[3] for line in listeners if len(line.split()) > 3 and line.split()[3].endswith(':' + str(port))]
        assert listeners == ['127.0.0.1:' + str(port)], 'server listener is not loopback-only'
        for auth in ('', 'Basic ' + base64.b64encode(b'opencode:incorrect-image-gate-password').decode()):
            try:
                request('/api/info', auth)
            except urllib.error.HTTPError as error:
                assert error.code == 401, 'unexpected authentication failure status'
                error.close()
            else:
                raise AssertionError('server accepted missing or wrong credentials')
        schema = request('/openapi.json')
        paths = schema.get('paths', {})
        assert schema.get('openapi') and 'get' in paths.get('/api/info', {}), 'released info schema missing'
        assert 'post' in paths.get('/api/session', {}), 'released session schema missing'
        for suffix, method in (('/prompt', 'post'), ('/message', 'get'), ('/interrupt', 'post')):
            assert any(path.startswith('/api/session/{') and path.endswith(suffix) and method in operations
                       for path, operations in paths.items()), 'released session operation missing'
    finally:
        if process is not None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=5)
            # The npm launcher can exit before a child: terminate any remaining
            # process-group members before removing their temporary state.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
print(json.dumps({'opencode_version': expected, 'released_api': True,
                  'authenticated': True, 'loopback_only': True, 'model_calls': 0}))
'''


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
test "$(id -un)" = ahvm
test "$HOME" = /home/ahvm
test "$PWD" = /workspace
sudo -n true
node --version; bun --version; python --version
claude --version; codex --version; opencode --version; pi --version
git --version; gcc --version | head -1
''')
    print(versions, flush=True)
    print(cli('exec', sandbox, '--', 'ahvm-dev', 'python3', '-c', OPENCODE_GATE,
              pins['OPENCODE_VERSION']), flush=True)
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
    cli('exec', sandbox, '--', 'sh', '-c', 'printf uploaded > /workspace/agent-owned')
    cli('exec', sandbox, '--', 'ahvm-dev', 'sh', '-c', 'printf edited >> /workspace/agent-owned')
    shell = obj('session', 'create', sandbox, '--pty', '--', '/usr/local/bin/ahvm-shell')['session_id']
    script = "test $(id -un) = ahvm && test $HOME = /home/ahvm && test $PWD = /workspace && touch shell-owned && sudo -n true && printf '\\nUSER-SHELL-OK\\n'; exit\n"
    subprocess.run([binary, 'session', 'input', sandbox, shell], input=script,
                   text=True, check=True, capture_output=True, timeout=30)
    output = cli('session', 'read', sandbox, shell, '--follow')
    assert '\nUSER-SHELL-OK\n' in output, output
    assert cli('exec', sandbox, '--', 'stat', '-c', '%U', '/workspace/shell-owned').strip() == 'ahvm'
    cli('stop', sandbox)
    cli('start', sandbox)
    assert cli('exec', sandbox, '--', 'ahvm-dev', 'cat', '/workspace/marker') == 'persistent'
    cli('exec', sandbox, '--', 'ahvm-dev', 'bash', '-lc',
        'test "$(./hello)" = C-OK && .venv/bin/python -c "import requests" && codex --version')
    print(f'PASS: Ubuntu tools, released OpenCode authenticated loopback API, apt/npm/pip, HTTPS, PTY, stop/start persistence ({time.monotonic()-started:.2f}s)', flush=True)
finally:
    if created:
        cli('delete', sandbox)
