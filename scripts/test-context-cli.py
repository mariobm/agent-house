#!/usr/bin/env python3
"""Connection routing contracts with fake credentials and loopback HTTP only."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading

binary = str(Path(sys.argv[1]).resolve())
seen = []
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_): pass
    def do_GET(self):
        seen.append((self.path, self.headers.get('Authorization')))
        data = b'{"sandboxes":[]}'
        self.send_response(200)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        endpoint = f'http://127.0.0.1:{server.server_port}'
        env = {k:v for k,v in os.environ.items() if not k.startswith('AHVM_')}
        env.update(AHVM_CONFIG_DIR=temp, AHVM_NO_UPDATE_CHECK='1')
        def run(*args, ok=True):
            p = subprocess.run([binary, *args], env=env, capture_output=True, text=True, timeout=10)
            assert (p.returncode == 0) == ok, (args, p.stdout, p.stderr)
            return p
        assert 'no default connection' in run('list', ok=False).stderr
        cloud = root / 'cloud'
        cloud.mkdir(mode=0o700)
        for name, value in {
            'login.json': {'endpoint': endpoint, 'kind': 'file'},
            'credentials.json': {'access_token':'fake-cloud', 'refresh_token':'fake-refresh', 'expires_at':4102444800},
        }.items():
            path = cloud / name
            path.write_text(json.dumps(value))
            path.chmod(0o600)
        run('use', 'cloud')
        run('list')
        assert seen[-1][1] == 'Bearer fake-cloud'
        before = len(seen)
        run('create', 'dev', '--cpus', '2', ok=False)
        run('create', 'dev', '--storage', 'local', ok=False)
        assert len(seen) == before
        env['AHVM_TOKEN'] = 'fake-direct'
        run('--endpoint', endpoint, 'list')
        assert seen[-1][1] == 'Bearer fake-direct'
        run('list')
        assert seen[-1][1] == 'Bearer fake-cloud'
        assert json.loads(run('contexts', '--json').stdout)['default'] == 'cloud'
        assert 'fake-' not in run('contexts').stdout
        run('--context', 'missing', 'list', ok=False)
        run('--context', 'cloud', '--endpoint', endpoint, 'list', ok=False)
    print('Cloud defaults, destination overrides, limits and credential separation passed')
finally:
    server.shutdown()
    server.server_close()
    thread.join()
