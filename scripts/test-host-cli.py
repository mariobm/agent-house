#!/usr/bin/env python3
"""Deterministic SSH transport contract tests; no server credentials or VMs."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory() as temp:
    root = Path(temp)
    ssh = root / 'ssh'
    ssh.write_text('''#!/usr/bin/env python3
import http.server,json,os,sys,threading
from pathlib import Path
a=sys.argv[1:]
cache=Path(os.environ['AHVM_CONFIG_DIR']).parent/'image-cache'
if '-O' in a: sys.exit(0)
if '-M' in a:
 port=int(a[a.index('-L')+1].split(':')[1])
 class Handler(http.server.BaseHTTPRequestHandler):
  def log_message(self,*a): pass
  def do_GET(self):
   self.send_response(200);self.end_headers()
   body={'features':['named-images-v1','desktop-v1'],'desktop_image':'ubuntu-desktop','desktop_image_installed':cache.exists(), 'desktop_images': {'ubuntu-desktop':cache.exists(),'omarchy-desktop':(cache.parent/'omarchy-cache').exists()}} if self.path.endswith('healthz') else {'sandboxes':[]}
   if (cache.parent/'gpu-feature').exists(): body.setdefault('features',[]).append('omarchy-desktop-v1')
   self.wfile.write(json.dumps(body).encode())
  def do_POST(self):
   body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
   (cache.parent/'last-create').write_text(json.dumps(body))
   self.send_response(200);self.end_headers()
   self.wfile.write(json.dumps({'id':body.get('name','test'),'state':'Running','stdout':'ok','stderr':'','exit_code':0}).encode())
 server=http.server.HTTPServer(('127.0.0.1',port),Handler)
 Path(a[a.index('-S')+1]).touch()
 threading.Thread(target=server.serve_forever,daemon=True).start()
 sys.stdin.read()
elif 'image pull' in a[-1]:
 (cache.parent/'omarchy-cache' if 'omarchy-desktop' in a[-1] else cache).write_text('installed'); print('Installed image')
else: print('a'*64)
''')
    ssh.chmod(0o755)
    env = {**os.environ, 'PATH': str(root) + ':' + os.environ['PATH'], 'AHVM_CONFIG_DIR': str(root / 'config'), 'AHVM_NO_UPDATE_CHECK': '1'}
    for key in ['AHVM_HOST', 'AHVM_ENDPOINT', 'AHVM_TOKEN_FILE', 'AHVM_TOKEN']:
        env.pop(key, None)
    def run(*args, ok=True):
        result = subprocess.run([binary, *args], env=env, capture_output=True, text=True, timeout=15)
        assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
        return result
    run('host', 'add', 'home', '--ssh', 'root@192.168.1.2')
    run('host', 'add', 'other', '--ssh', 'other')
    config = json.loads(run('host', 'list', '--json').stdout)
    assert config['default'] == 'home' and len(config['hosts']) == 2
    generated = json.loads(run('create', '--json').stdout)['id']
    assert generated.startswith('vm-') and len(generated) == 15
    assert json.loads(run('create', 'dev', '--host', 'other', '--json').stdout)['id'] == 'dev'
    assert run('exec', 'dev', '--', 'echo', 'hello').stdout == 'ok'
    assert json.loads(run('create', 'desk', '--desktop', '--json').stdout)['id'] == 'desk'
    assert (root/'image-cache').exists()
    body=json.loads((root/'last-create').read_text())
    assert body['desktop'] and body['cpus']==2 and body['memory_mb']==4096
    before=(root/'image-cache').stat().st_mtime_ns
    run('create', 'desk2', '--desktop')
    assert (root/'image-cache').stat().st_mtime_ns==before

    # Refuse old servers; pull Omarchy even when Ubuntu is already installed.
    run('create', 'omarchy', '--image', 'omarchy-desktop', ok=False)
    assert not (root/'omarchy-cache').exists()
    (root/'gpu-feature').touch()
    run('create', 'omarchy', '--image', 'omarchy-desktop')
    assert (root/'omarchy-cache').exists()
    body=json.loads((root/'last-create').read_text())
    assert body['desktop'] and body['image']=='omarchy-desktop'
    assert body['cpus']==4 and body['memory_mb']==8192
    before=(root/'omarchy-cache').stat().st_mtime_ns
    run('create', 'omarchy2', '--image', 'omarchy-desktop', '--cpus', '2', '--memory', '4096')
    assert (root/'omarchy-cache').stat().st_mtime_ns==before
    body=json.loads((root/'last-create').read_text())
    assert body['cpus']==2 and body['memory_mb']==4096

    run('host', 'use', 'other')
    assert json.loads(run('host', 'list', '--json').stdout)['default'] == 'other'
    run('list', '--host', 'missing', ok=False)
    run('list', '--host', 'home', '--endpoint', 'http://127.0.0.1:1', ok=False)
    run('host', 'add', 'home', '--ssh', 'replacement', ok=False)
    run('host', 'add', 'bad', '--ssh', 'root@host;id', ok=False)
    run('host', 'remove', 'other')
    assert json.loads(run('host', 'list', '--json').stdout)['default'] == 'home'
    assert 'a' * 64 not in (root / 'config/hosts.json').read_text()
    print('Host selection, default, SSH transport, generated names and rejection contracts passed')
