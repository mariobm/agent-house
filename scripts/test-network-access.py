#!/usr/bin/env python3
"""Opt-in Linux/KVM gate for preview and private access; uses only stdlib.
Requires AHVM_DAEMON_BIN, AHVM_VMM_BIN, AHVM_NETD_BIN, AHVM_BASE_IMAGE,
AHVM_LIB, AHVM_DNS_RESOLVER and a fresh AHVM_ACCESS_TEST_DIR. The guest image
must contain the current Rust forge, Python 3 and curl. No host policy changes.
"""
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import threading
import time

GUEST_SERVER = r'''
import socket,threading,json,hashlib,base64,time
s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind(('127.0.0.1',18080));s.listen()
def serve(c):
 try:
  f=c.makefile('rb');first=f.readline().decode();headers={}
  while True:
   line=f.readline().decode().strip()
   if not line:break
   k,v=line.split(':',1);headers[k.lower()]=v.strip()
  path=first.split()[1]
  if path=='/ws':
   accept=base64.b64encode(hashlib.sha1((headers['sec-websocket-key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
   c.sendall(b'HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: '+accept+b'\r\n\r\n')
   while True:
    h=f.read(2)
    if not h:break
    n=h[1]&127;mask=f.read(4);data=f.read(n);data=bytes(v^mask[i%4] for i,v in enumerate(data))
    c.sendall(bytes([0x81,n])+data)
  elif path=='/stream':
   c.sendall(b'HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nA\r\n');time.sleep(1)
   c.sendall(b'1\r\nB\r\n0\r\n\r\n')
  else:
   data=json.dumps({'path':path,'headers':headers}).encode()
   c.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: '+str(len(data)).encode()+b'\r\n\r\n'+data)
 except (OSError,ValueError):pass
 finally:c.close()
print('PREVIEW-READY',flush=True)
while True:
 c,_=s.accept();threading.Thread(target=serve,args=(c,),daemon=True).start()
'''


def terminate_record(worker):
    """Pin the process before identity checking; never signal a recycled PID."""
    pid = worker['pid']
    fd = os.pidfd_open(pid)
    try:
        stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        if int(stat[19]) != worker.get('starttime'):
            return False
        signal.pidfd_send_signal(fd, signal.SIGTERM)
        return True
    finally:
        os.close(fd)


def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def run():
    root = Path(os.environ['AHVM_ACCESS_TEST_DIR'])
    root.mkdir()  # Refuse existing state.
    api_port, preview_port = free_port(), free_port()
    token = secrets.token_hex(32)
    # A genuinely reachable host endpoint, not an absent listener negative test.
    route = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    route.connect(('1.1.1.1', 53))
    host_ip = route.getsockname()[0]
    route.close()
    listener = socket.socket()
    listener.bind((host_ip, 0))
    listener.listen()
    endpoint = listener.getsockname()
    forbidden = socket.socket()
    forbidden.bind((host_ip, 0)); forbidden.listen(); forbidden.setblocking(False)
    with socket.create_connection(forbidden.getsockname(), timeout=2):
        control, _ = forbidden.accept(); control.close()
    stop = threading.Event()
    def host_service():
        listener.settimeout(0.2)
        while not stop.is_set():
            try:
                c, _ = listener.accept()
            except TimeoutError:
                continue
            with c:
                c.settimeout(2)
                try:
                    c.recv(4096)
                    c.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nPRIVATE-OK')
                except OSError:
                    pass
    thread = threading.Thread(target=host_service)
    thread.start()
    policy = root/'policy.json'
    policy.write_text(json.dumps({'access-a': {'owner_user_id':'admin','destinations':[f'{endpoint[0]}:{endpoint[1]}']}}))
    env = dict(os.environ, AHVM_DATA_DIR=str(root/'data'), AHVM_ADMIN_TOKEN=token,
               AHVM_LISTEN=f'127.0.0.1:{api_port}', AHVM_PREVIEW_LISTEN=f'127.0.0.1:{preview_port}',
               AHVM_PREVIEW_DOMAIN='preview.localhost', AHVM_PRIVATE_ACCESS_FILE=str(policy),
               AHVM_IDLE_SECS='3600')
    log = (root/'daemon.log').open('ab')
    daemon = None
    def request(port, method, path, body=None, headers=None):
        c = http.client.HTTPConnection('127.0.0.1', port, timeout=90)
        payload = None if body is None else json.dumps(body)
        c.request(method, path, payload, headers or {})
        response = c.getresponse()
        data = response.read()
        result = response.status, data, dict(response.getheaders())
        c.close()
        return result
    def api(method, path, body=None, expected=200):
        status, data, _ = request(api_port, method, '/v1'+path, body,
                                  {'Authorization': 'Bearer '+token, 'Content-Type': 'application/json'})
        assert status == expected, (method, path, status, data)
        return json.loads(data) if data else None
    def launch():
        p = subprocess.Popen([os.environ['AHVM_DAEMON_BIN']], env=env, stdout=log, stderr=log)
        for _ in range(100):
            assert p.poll() is None, (root/'daemon.log').read_text()
            try:
                if request(api_port, 'GET', '/v1/healthz')[0] == 200:
                    return p
            except OSError:
                pass
            time.sleep(0.1)
        raise AssertionError('daemon readiness timeout')
    def preview(id='access-a', path='/', auth=True, cookie=None, extra=None):
        h = {'Host': id.encode().hex()+f'--18080.preview.localhost:{preview_port}'}
        if auth: h['Authorization'] = 'Bearer '+token
        if cookie: h['Cookie'] = cookie
        if extra: h.update(extra)
        return request(preview_port,'GET',path,headers=h)
    def ws(cookie=None):
        s=socket.create_connection(('127.0.0.1',preview_port), timeout=5)
        host='access-a'.encode().hex()+f'--18080.preview.localhost:{preview_port}'
        credential = 'Cookie: '+cookie if cookie else 'Authorization: Bearer '+token
        key=base64.b64encode(secrets.token_bytes(16)).decode()
        s.sendall(f'GET /ws HTTP/1.1\r\nHost: {host}\r\n{credential}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n'.encode())
        head=b''
        while not head.endswith(b'\r\n\r\n'):
            chunk=s.recv(1); assert chunk,head; head+=chunk
        assert head.startswith(b'HTTP/1.1 101'),head
        mask=b'abcd'; data=b'echo'
        s.sendall(b'\x81\x84'+mask+bytes(v^mask[i%4] for i,v in enumerate(data)))
        received=b''
        while len(received)<6:
            chunk=s.recv(6-len(received));assert chunk,received;received+=chunk
        assert received==b'\x81\x04echo',received
        return s
    started=time.monotonic()
    try:
        daemon=launch()
        for id in ['access-a','access-b']:
            api('POST','/sandboxes',{'name':id,'cpus':1,'memory_mb':256},201)
        # Independently verify the positive host control.
        with socket.create_connection(endpoint,timeout=2) as c:
            c.sendall(b'GET / HTTP/1.0\r\n\r\n'); assert b'PRIVATE-OK' in c.recv(4096)
        for id, allowed in [('access-a',True),('access-b',False)]:
            out=api('POST',f'/sandboxes/{id}/exec',{'argv':['curl','-fsS','--connect-timeout','1','--max-time','2',f'http://{endpoint[0]}:{endpoint[1]}/']})
            assert (out['exit_code']==0)==allowed,out
        out=api('POST','/sandboxes/access-a/exec',{'argv':['curl','-fsS','--connect-timeout','1','--max-time','2',f'http://{host_ip}:{forbidden.getsockname()[1]}/']})
        assert out['exit_code']!=0,out
        try:
            unexpected,_=forbidden.accept();unexpected.close()
        except BlockingIOError: pass
        else: raise AssertionError('private grant allowed another port')
        api('POST','/sandboxes/access-a/sessions',{'argv':['python3','-u','-c',GUEST_SERVER]})
        for _ in range(30):
            out=api('POST','/sandboxes/access-a/exec',{'argv':['curl','-fsS','http://127.0.0.1:18080/']})
            if out['exit_code']==0:break
            time.sleep(0.1)
        else:raise AssertionError('guest preview service not ready')
        assert preview(auth=False)[0]==401
        assert preview()[0]==404
        api('PUT','/sandboxes/access-a/previews/18080',expected=204)
        status,data,_=preview(path='/headers?x=1')
        assert status==200,(status,data)
        data=json.loads(data)
        assert data['path']=='/headers?x=1' and 'authorization' not in data['headers'],data
        grant=api('POST','/sandboxes/access-a/previews/18080/access')
        status,_,headers=preview(path='/?ahvm_token='+grant['token'],auth=False)
        assert status==303 and headers['location']=='/' and 'HttpOnly' in headers['set-cookie'],headers
        cookie=headers['set-cookie'].split(';')[0]
        status,data,_=preview(auth=False,cookie=cookie)
        assert status==200,(status,data)
        assert 'ahvm_preview' not in json.loads(data)['headers'].get('cookie',''),data
        assert preview('access-b',auth=False,cookie=cookie)[0]==401
        assert preview(auth=False,cookie=cookie,extra={'Origin':'http://evil.preview.localhost'})[0]==403
        # API credentials and preview credentials are not interchangeable.
        assert request(api_port,'GET','/v1/sandboxes',headers={'Cookie':cookie})[0]==401
        host='access-a'.encode().hex()+f'--18080.preview.localhost:{preview_port}'
        c=http.client.HTTPConnection('127.0.0.1',preview_port,timeout=5)
        start=time.monotonic();c.request('GET','/stream',headers={'Host':host,'Cookie':cookie})
        r=c.getresponse();assert r.read(1)==b'A' and time.monotonic()-start<0.8
        assert r.read()==b'B';c.close()
        channel=ws(cookie)
        api('DELETE','/sandboxes/access-a/previews/18080',expected=204)
        # Re-enabling immediately must not revive the old forwarding lease.
        api('PUT','/sandboxes/access-a/previews/18080',expected=204)
        assert channel.recv(1)==b'';channel.close()
        assert preview(auth=False,cookie=cookie)[0]==401
        api('PUT','/sandboxes/access-a/previews/18080',expected=204)
        api('POST','/sandboxes/access-a/stop')
        assert preview()[0]!=200
        api('POST','/sandboxes/access-a/start')
        assert preview()[0]==200
        snapshot=api('POST','/sandboxes/access-a/snapshots',{'name':'access-snapshot'},201)
        api('POST',f'/snapshots/{snapshot["id"]}/restore',{'new_id':'access-copy'},201)
        assert api('GET','/sandboxes/access-copy/previews')==[]
        out=api('POST','/sandboxes/access-copy/exec',{'argv':['curl','-fsS','--connect-timeout','1','--max-time','2',f'http://{endpoint[0]}:{endpoint[1]}/']})
        assert out['exit_code']!=0,out
        api('DELETE','/sandboxes/access-copy',expected=204)
        # Daemon restart must preserve registrations and adopt gateway workers.
        daemon.terminate();daemon.wait(timeout=10);daemon=launch()
        assert preview()[0]==200
        channel=ws();channel.close()
        # Policy changes cannot be silently applied to adopted live gateways.
        daemon.terminate();daemon.wait(timeout=10)
        policy.write_text('{}')
        refused=subprocess.Popen([os.environ['AHVM_DAEMON_BIN']],env=env,stdout=log,stderr=log)
        assert refused.wait(timeout=10)!=0,'live private policy change was accepted'
        policy.write_text(json.dumps({'access-a': {'owner_user_id':'admin','destinations':[f'{endpoint[0]}:{endpoint[1]}']}}))
        daemon=launch()
        for id in ['access-a','access-b']: api('DELETE',f'/sandboxes/{id}',expected=204)
        assert api('GET','/sandboxes')['sandboxes']==[]
        print(json.dumps({'result':'pass','seconds':round(time.monotonic()-started,2),
                          'checks':['private allow/peer deny with positive listener','exact private port boundary','restore-as-new inherits no grants','preview registration/auth','credential stripping','scoped browser cookie and cross-origin denial','streaming','WebSocket echo/revoke','stop/start','daemon adoption','policy-change refusal','cleanup']}))
    finally:
        if daemon and daemon.poll() is None:
            for id in ['access-a','access-b','access-copy']:
                try: api('DELETE',f'/sandboxes/{id}',expected=204)
                except Exception: pass
            daemon.terminate();daemon.wait(timeout=10)
        # Verified fallback cleanup if startup/restart itself failed.
        for record in (root/'data'/'sandboxes').glob('*/**/*state.json'):
            try:
                terminate_record(json.loads(record.read_text()))
            except (OSError,ValueError,KeyError): pass
        stop.set();thread.join(timeout=3);listener.close();forbidden.close();log.close()

if __name__=='__main__':
    run()
