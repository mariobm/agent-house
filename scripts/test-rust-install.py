#!/usr/bin/env python3
"""Opt-in fresh-install KVM gate, max two guests. Runs CLI from an installed bundle.
Usage: sudo test-rust-install.py PREFIX CONFIG_DIR DATA_DIR TEST_UNIT
Only use a disposable installation: this restarts its unit and edits its policy.
"""
import http.client
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

prefix, config, data = map(Path, sys.argv[1:4])
unit = sys.argv[4]
assert unit.startswith('ahvm-test-'), 'Requires a dedicated ahvm-test-* unit'
binary = prefix/'bin/ahvm'
env = os.environ.copy()
env['AHVM_TOKEN_FILE'] = str(config/'admin.token')
original_env = (config/'daemon.env').read_text()
settings = dict(line.split('=',1) for line in original_env.splitlines() if '=' in line)
env['AHVM_ENDPOINT'] = 'http://'+settings['AHVM_LISTEN']
preview_port = int(settings['AHVM_PREVIEW_LISTEN'].rsplit(':',1)[1])
ids = ['phase6-a','phase6-copy']

def cli(*args, expected=0, input=None):
    p = subprocess.run([str(binary), *args],env=env,input=input,capture_output=True,timeout=650)
    assert p.returncode == expected, (args,p.returncode,p.stdout,p.stderr)
    return p.stdout

def obj(*args): return json.loads(cli('--json',*args))
def execute(id, script, expected=0): return cli('exec',id,'--','sh','-ec',script,expected=expected)
def wait_ready():
    end = time.monotonic()+15
    while time.monotonic()<end:
        try:
            if obj('health')['status']=='ok': return
        except (AssertionError,OSError): pass
        time.sleep(.1)
    raise AssertionError('daemon readiness')
def restart():
    subprocess.run(['systemctl','restart',unit],check=True); wait_ready()
def preview(host, path="/index.html", cookie=None):
    c=http.client.HTTPConnection('127.0.0.1',preview_port,timeout=5)
    headers={'Host':host}
    if cookie: headers['Cookie']=cookie
    c.request('GET',path,headers=headers)
    r=c.getresponse(); result=(r.status,r.read(),dict(r.getheaders())); c.close(); return result

def terminal_command(args, action):
    import fcntl, pty, select, termios
    master, slave=pty.openpty()
    def setup():
        os.setsid();fcntl.ioctl(0,termios.TIOCSCTTY,0)
    p=subprocess.Popen([str(binary),*args],env=env,stdin=slave,stdout=slave,stderr=slave,preexec_fn=setup)
    output=bytearray()
    def until(marker):
        end=time.monotonic()+10
        while marker not in output:
            assert time.monotonic()<end, (marker,bytes(output))
            if select.select([master],[],[],.1)[0]: output.extend(os.read(master,4096))
    try:
        action(master,slave,p,until)
    finally:
        if p.poll() is None: p.kill();p.wait()
        os.close(master);os.close(slave)

# Use a real reachable private endpoint so denial is not an absent listener.
route=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);route.connect(('1.1.1.1',53))
host_ip=route.getsockname()[0];route.close()
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self,*_): pass
    def do_GET(self):
        body=b'PRIVATE-OK';self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
server=http.server.ThreadingHTTPServer((host_ip,0),Handler)
thread=threading.Thread(target=server.serve_forever);thread.start()
policy=config/'private-access.json'; old_policy=policy.read_bytes()
started=time.monotonic()
snapshot=None
try:
    wait_ready()
    assert obj('list')['sandboxes']==[], 'Requires empty disposable installation'
    policy.write_text(json.dumps({'phase6-a':{'owner_user_id':'admin','destinations':[f'{host_ip}:{server.server_port}']}}))
    restart()
    obj('create',ids[0],'--memory','256','--cpus','1')
    assert execute(ids[0],'printf CLI-OK')==b'CLI-OK'
    execute(ids[0],'exit 7',expected=7)
    url=f'http://{host_ip}:{server.server_port}/'
    assert execute(ids[0],f'wget -T 3 -qO- {url}')==b'PRIVATE-OK'
    execute(ids[0],'nslookup example.com >/dev/null')
    print('create, exec, private grant and DNS passed',flush=True)
    with tempfile.TemporaryDirectory() as directory:
        local=Path(directory)/'payload';download=Path(directory)/'download'
        content=bytes(range(256))*1024
        local.write_bytes(content)
        obj('files','put',ids[0],str(local),'/workspace/payload')
        cli('files','get',ids[0],'/workspace/payload',str(download))
        assert download.read_bytes()==content
        assert any(e['name']=='payload' for e in obj('files','list',ids[0],'/workspace')['entries'])
    cli('files','put',ids[0],'-','/workspace/index.html',input=b'PREVIEW-OK')
    sid=obj('session','create',ids[0],'--','sh','-c','printf SESSION-READY; read line; printf "%s" "$line"; sleep 300')['session_id']
    first=obj('session','read',ids[0],sid)
    assert first['next_seq']>0
    cli('session','input',ids[0],sid,input=b'INPUT-OK\n')
    assert cli('session','read',ids[0],sid,'--from-seq',str(first['next_seq']))==b'INPUT-OK'
    # Actual PTY shell over WebSocket: input, output, resize, detach, reattach.
    prior={row['id'] for row in obj('session','list',ids[0])}
    def interact(master,slave,p,until):
        import fcntl,struct,termios
        until(b'Session ')
        # Input may queue during the handshake; the CLI bounds that queue.
        os.write(master,b"printf '%s%s\\n' PTY -OK\r")
        until(b'PTY-OK')
        fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',42,100,0,0))
        time.sleep(.5)
        os.write(master,b'stty size\r');until(b'42 100')
        os.write(master,b'\x1d');assert p.wait(timeout=5)==0
    terminal_command(['shell',ids[0]],interact)
    shell_ids={row['id'] for row in obj('session','list',ids[0])}-prior
    assert len(shell_ids)==1,shell_ids
    shell_id=shell_ids.pop()
    def exit_shell(master,slave,p,until):
        time.sleep(.5);os.write(master,b'exit 13\r');assert p.wait(timeout=10)==13
    terminal_command(['session','attach',ids[0],shell_id],exit_shell)
    cli('session','delete',ids[0],shell_id)
    print('WebSocket PTY input/output, resize, detach and reattach passed',flush=True)
    http_sid=obj('session','create',ids[0],'--','httpd','-f','-p','18080','-h','/workspace')['session_id']
    cli('preview','enable',ids[0],'18080')
    grant=obj('preview','access',ids[0],'18080','--base-url',f'http://preview.localhost:{preview_port}')
    host=grant['host_label']+'.preview.localhost'
    bootstrap=preview(host,'/?ahvm_token='+grant['token'])
    assert bootstrap[0]==303, bootstrap
    cookie=bootstrap[2]['set-cookie'].split(';',1)[0]
    for _ in range(20):
        if preview(host,cookie=cookie)[:2]==(200,b'PREVIEW-OK'): break
        time.sleep(.1)
    else: raise AssertionError('preview did not serve')
    cli('preview','revoke',ids[0],'18080')
    assert preview(host,cookie=cookie)[0]!=200
    print('binary files, sessions and preview grant/revoke passed',flush=True)
    cli('stop',ids[0]);cli('start',ids[0])
    snapshot=obj('snapshot','create',ids[0],f'phase6-{time.time_ns()}')['id']
    execute(ids[0],'printf changed > /workspace/index.html')
    obj('snapshot','restore',snapshot,ids[1])
    assert execute(ids[1],'cat /workspace/index.html')==b'PREVIEW-OK'
    denied=obj('exec',ids[1],'--','sh','-c',f'wget -T 2 -qO- {url}; exit 0')
    assert 'PRIVATE-OK' not in denied['stdout']
    assert len(obj('list')['sandboxes'])==2
    workers={id:json.loads((data/'sandboxes'/id/'state.json').read_text())['pid'] for id in ids}
    restart()
    for id in ids:
        assert execute(id,'printf ADOPTED')==b'ADOPTED'
        assert json.loads((data/'sandboxes'/id/'state.json').read_text())['pid']==workers[id]
    record=json.loads((data/'sandboxes'/ids[0]/'state.json').read_text())
    fd=os.pidfd_open(record['pid'])
    try:
        stat=Path(f'/proc/{record["pid"]}/stat').read_text().rsplit(')',1)[1].split()
        assert int(stat[19])==record['starttime']
        signal.pidfd_send_signal(fd,signal.SIGKILL)
    finally: os.close(fd)
    end=time.monotonic()+10
    while obj('get',ids[0])['state']!='failed':
        assert time.monotonic()<end;time.sleep(.1)
    cli('start',ids[0]);assert execute(ids[0],'printf RECOVERED')==b'RECOVERED'
    print('snapshot isolation, two-worker adoption and SIGKILL recovery passed',flush=True)
    cli('session','kill',ids[1],sid);cli('session','delete',ids[1],sid)
    cli('session','kill',ids[1],http_sid);cli('session','delete',ids[1],http_sid)
    # A forgotten-open interactive terminal must not hold the VM hot forever.
    (config/'daemon.env').write_text(original_env.replace('AHVM_IDLE_SECS=3600','AHVM_IDLE_SECS=2')+'\nAHVM_SWEEP_SECS=1\n')
    restart()
    def idle(master,slave,p,until):
        until(b'Session ')
        end=time.monotonic()+12
        while obj('get',ids[0])['state']!='stopped':
            assert time.monotonic()<end, 'idle attach kept sandbox running'
            time.sleep(.2)
        assert p.wait(timeout=5)==1, 'idle worker stop should disconnect attach'
    terminal_command(['shell',ids[0]],idle)
    print('idle WebSocket terminal permits automatic stop',flush=True)

finally:
    # Always attempt deletion of both; never signal unchecked processes.
    failures=[]
    for id in ids:
        try: cli('delete',id)
        except AssertionError as error:
            try:
                if any(x['id']==id for x in obj('list')['sandboxes']): failures.append(str(error))
            except Exception as error: failures.append(str(error))
    if snapshot:
        try: cli('snapshot','delete',snapshot)
        except Exception as error: failures.append(str(error))
    policy.write_bytes(old_policy)
    (config/'daemon.env').write_text(original_env)
    server.shutdown();server.server_close();thread.join()
    assert not failures, failures
assert obj('list')['sandboxes']==[]
print(f'PASS: packaged CLI/install, max two 256 MiB guests, {time.monotonic()-started:.2f}s; all guests deleted')
