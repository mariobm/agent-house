#!/usr/bin/env python3
"""No-VM CLI contract tests. Usage: test-rust-cli.py /path/to/ahvm."""
import base64
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest

BINARY = str(Path(sys.argv.pop(1)).resolve())

class ClientTests(unittest.TestCase):
    def setUp(self):
        self.requests = []
        self.replies = []
        outer = self
        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_GET(self): self.handle_request()
            def do_POST(self): self.handle_request()
            def do_PUT(self): self.handle_request()
            def handle_request(self):
                if self.headers.get('Transfer-Encoding')=='chunked':
                    body=bytearray()
                    while True:
                        size=int(self.rfile.readline().split(b';')[0],16)
                        if not size: self.rfile.readline();break
                        body.extend(self.rfile.read(size));assert self.rfile.read(2)==b'\r\n'
                    body=bytes(body)
                else: body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
                parsed=body if self.headers.get('Content-Type')=='application/octet-stream' else json.loads(body) if body else None
                outer.requests.append((self.command, self.path, self.headers, parsed))
                status, body, headers = outer.replies.pop(0)
                data = json.dumps(body).encode()
                self.send_response(status)
                for key, value in headers.items(): self.send_header(key, value)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers(); self.wfile.write(data)
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever); self.thread.start()
        self.endpoint = 'http://127.0.0.1:'+str(self.server.server_port)
    def tearDown(self):
        self.server.shutdown(); self.server.server_close(); self.thread.join()
    def run_cli(self, *args, **kw):
        env = {k:v for k,v in os.environ.items() if not k.startswith('AHVM_')}
        env['AHVM_TOKEN'] = 'secret-for-test'
        return subprocess.run([BINARY, '--endpoint', self.endpoint, *args], env=env, capture_output=True, timeout=10, **kw)
    def reply(self, body, status=200, **headers): self.replies.append((status, body, headers))
    def test_storage_create_preserves_default_and_checks_server(self):
        self.reply({'id':'box'})
        p=self.run_cli('create','box')
        self.assertEqual(p.returncode,0,p.stderr)
        self.assertNotIn('storage_mode',self.requests[-1][3])
        self.reply({'features':['replicated-storage-v1']})
        self.reply({'id':'disk'})
        p=self.run_cli('create','disk','--storage','replicated')
        self.assertEqual(p.returncode,0,p.stderr)
        self.assertEqual(self.requests[-1][3]['storage_mode'],'replicated')
        self.reply({'features':[]})
        p=self.run_cli('create','old','--storage','replicated')
        self.assertNotEqual(p.returncode,0)
        self.assertEqual(self.requests[-1][1],'/v1/healthz')
        self.assertNotIn('old',str([r[3] for r in self.requests]))
        before=len(self.requests)
        self.assertNotEqual(self.run_cli('create','bad','--storage','typo').returncode,0)
        self.assertEqual(len(self.requests),before)

    def test_create_sizing_defaults_and_overrides(self):
        for args, cpus, memory in [
            ([], 1, 2048),
            (['--memory', '1024'], 1, 1024),
            (['--desktop'], 2, 4096),
            (['--image', 'omarchy-desktop'], 4, 8192),
            (['--desktop', '--memory', '6144'], 2, 6144),
        ]:
            with self.subTest(args=args):
                if '--desktop' in args or '--image' in args:
                    self.reply({'features': ['desktop-v1', 'omarchy-desktop-v1'],
                                'desktop_images': {'ubuntu-desktop': True, 'omarchy-desktop': True}})
                self.reply({'id': 'box'})
                p = self.run_cli('create', 'box', *args)
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual(self.requests[-1][3]['memory_mb'], memory)
                self.assertEqual(self.requests[-1][3]['cpus'], cpus)

    def test_storage_status_and_sync_contract(self):
        for command, method, path in [('status','GET','/v1/sandboxes/box/storage'), ('sync','POST','/v1/sandboxes/box/storage/sync')]:
            self.reply({'features':['replicated-storage-v1']})
            self.reply({'mode':'replicated','replication':{'pending_bytes':0}})
            p=self.run_cli('--json','storage',command,'box')
            self.assertEqual(p.returncode,0,p.stderr)
            self.assertEqual(json.loads(p.stdout)['mode'],'replicated')
            self.assertEqual(self.requests[-1][:2],(method,path))
        self.reply({'features':['replicated-storage-v1']})
        self.reply({'message':'remote unavailable'},503)
        self.assertNotEqual(self.run_cli('storage','sync','box').returncode,0)

    def test_exec_argv_and_status(self):
        self.reply({'stdout':'hello\n','stderr':'problem\n','exit_code':42,'truncated':True})
        p=self.run_cli('exec','box','--','sh','-c',"printf '%s' '$HOME'",'--json')
        self.assertEqual(p.returncode,42); self.assertEqual(p.stdout,b'hello\n')
        self.assertIn(b'problem\n',p.stderr); self.assertIn(b'truncated',p.stderr)
        self.assertEqual(self.requests[0][3]['argv'],['sh','-c',"printf '%s' '$HOME'",'--json'])
        self.assertEqual(self.requests[0][2]['Authorization'],'Bearer secret-for-test')
    def test_paginated_list(self):
        self.reply({'sandboxes':[{'id':'a'}],'next_cursor':'1:a'})
        self.reply({'sandboxes':[{'id':'b'}],'next_cursor':None})
        p=self.run_cli('--json','list')
        self.assertEqual(p.returncode,0,p.stderr)
        self.assertEqual([s['id'] for s in json.loads(p.stdout)['sandboxes']],['a','b'])
        self.assertIn('after=1%3Aa',self.requests[1][1])
    def test_session_authoritative_cursor(self):
        self.reply({'data_b64':base64.b64encode(b'abc').decode(),'next_seq':900,'eof':False,'truncated':True})
        self.reply({'data_b64':'ZA==','next_seq':901,'eof':True,'exit_code':7,'truncated':False})
        p=self.run_cli('session','read','box','sid','--follow','--from-seq','100')
        self.assertEqual(p.returncode,7,p.stderr); self.assertEqual(p.stdout,b'abcd')
        self.assertIn('from_seq=900',self.requests[1][1])
    def test_download_does_not_clobber_on_failure(self):
        self.reply({'data_b64':'YWJj','eof':False})
        self.reply({'error':'read failed'},500)
        with tempfile.TemporaryDirectory() as directory:
            dest=Path(directory)/'out'; dest.write_bytes(b'original')
            p=self.run_cli('files','get','box','/a b?x',str(dest))
            self.assertEqual(p.returncode,1); self.assertEqual(dest.read_bytes(),b'original')
            self.assertEqual(len(list(Path(directory).iterdir())),1)
        self.assertIn('path=%2Fa+b%3Fx',self.requests[0][1])
    def test_binary_download_pages(self):
        self.reply({'data_b64':base64.b64encode(b'\0\xff').decode(),'eof':False})
        self.reply({'data_b64':'','eof':True})
        p=self.run_cli('files','get','box','/data','-')
        self.assertEqual(p.returncode,0,p.stderr); self.assertEqual(p.stdout,b'\0\xff')
        self.assertIn('offset=2',self.requests[1][1])
    def test_redirect_not_followed(self):
        self.reply({},302,Location='/v1/healthz')
        p=self.run_cli('get','box')
        self.assertEqual(p.returncode,1); self.assertEqual(len(self.requests),1)
    def test_large_upload_file_and_stdin(self):
        content=bytes(range(256))*16384
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'large';path.write_bytes(content)
            for source in [str(path),'-']:
                self.reply({'bytes':len(content)})
                p=self.run_cli('files','put','box',source,'/large file',input=content if source=='-' else None)
                self.assertEqual(p.returncode,0,p.stderr)
                self.assertEqual(self.requests[-1][3],content)
                self.assertEqual(self.requests[-1][1],'/v1/sandboxes/box/files/upload?path=%2Flarge+file')

    def test_auth_failure_reported(self):
        self.reply({'error':'unauthorized'},401)
        p=self.run_cli('get','box')
        self.assertEqual(p.returncode,1); self.assertIn(b'401',p.stderr)
        self.assertNotIn(b'secret-for-test',p.stderr)
    def test_identifier_cannot_inject_path(self):
        self.reply({'error':'not found'},404)
        self.run_cli('get','other/stop?x=1')
        self.assertEqual(self.requests[0][1],'/v1/sandboxes/other%2Fstop%3Fx=1')
    @unittest.skipUnless(os.name == 'posix', 'requires a terminal')
    def test_pty_restores_terminal_on_handshake_error(self):
        import fcntl, pty, termios, threading
        master, slave = pty.openpty()
        before = termios.tcgetattr(master)
        self.reply({'error':'unauthorized'},401)
        env = {k:v for k,v in os.environ.items() if not k.startswith('AHVM_')}
        env['AHVM_TOKEN']='test'
        def setup():
            os.setsid()
            fcntl.ioctl(0,termios.TIOCSCTTY,0)
        try:
            p=subprocess.Popen([BINARY,'--endpoint',self.endpoint,'session','attach','box','sid'],
                stdin=slave,stdout=slave,stderr=subprocess.PIPE,env=env,preexec_fn=setup)
            # Drain PTY output like a terminal emulator. macOS can wait for
            # pending output before restoring termios.
            output = bytearray()
            done = threading.Event()
            def drain():
                import select
                while not done.is_set():
                    if select.select([master], [], [], 0.05)[0]:
                        try:
                            chunk = os.read(master, 65536)
                        except OSError:
                            break
                        if not chunk:
                            break
                        output.extend(chunk)
            reader = threading.Thread(target=drain, daemon=True)
            reader.start()
            try:
                _,error=p.communicate(timeout=10)
            finally:
                if p.poll() is None:
                    p.kill()
                    p.communicate()
                done.set()
                reader.join(timeout=1)
            self.assertEqual(p.returncode,1,error)
            self.assertEqual(termios.tcgetattr(master),before)
            self.assertIn(b'\x1b[?1003l', output)
            self.assertIn(b'\x1b[?1049l', output)
            self.assertIn(b'\x1b[?25h', output)
        finally:
            os.close(master);os.close(slave)

    @unittest.skipUnless(os.name == 'posix', 'requires a terminal')
    def test_pty_reconnects_from_delivered_cursor(self):
        import fcntl, pty, termios, socketserver, hashlib, select
        paths=[]
        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                line=self.rfile.readline().decode()
                headers={}
                while raw:=self.rfile.readline().strip():
                    k,v=raw.decode().split(':',1);headers[k.lower()]=v.strip()
                if not line.startswith('GET '):
                    self.wfile.write(b'HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}');return
                paths.append(line.split()[1])
                key=base64.b64encode(hashlib.sha1((headers['sec-websocket-key']+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
                self.wfile.write(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: '+key+b'\r\n\r\n')
                last=len(paths)==2
                payload=json.dumps({'data_b64':base64.b64encode(b'AFTER-RESET' if last else b'BEFORE-RESET').decode(),
                    'next_seq':211 if last else 200,'eof':last,'exit_code':0,'truncated':False}).encode()
                self.wfile.write(b'\x81\x7e'+len(payload).to_bytes(2,'big')+payload)
                self.wfile.flush()
                # Return without a close handshake, just like a proxy reset.
        server=socketserver.ThreadingTCPServer(('127.0.0.1',0),Handler)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        master,slave=pty.openpty();before=termios.tcgetattr(master)
        env={k:v for k,v in os.environ.items() if not k.startswith('AHVM_')};env['AHVM_TOKEN']='test'
        def setup():
            os.setsid();fcntl.ioctl(0,termios.TIOCSCTTY,0)
        p=subprocess.Popen([BINARY,'--endpoint',f'http://127.0.0.1:{server.server_address[1]}','session','attach','box','sid'],
            stdin=slave,stdout=slave,stderr=slave,env=env,preexec_fn=setup)
        output=bytearray()
        try:
            import time
            deadline=time.monotonic()+15
            while time.monotonic()<deadline:
                if select.select([master],[],[],.05)[0]:output.extend(os.read(master,65536))
                if p.poll() is not None:break
            self.assertEqual(p.poll(),0,output)
            self.assertEqual(len(paths),2)
            self.assertTrue(paths[1].endswith('from_seq=200'),paths)
            self.assertEqual(output.count(b'BEFORE-RESET'),1)
            self.assertEqual(output.count(b'AFTER-RESET'),1)
            self.assertEqual(termios.tcgetattr(master),before)
        finally:
            if p.poll() is None:p.kill();p.wait()
            os.close(master);os.close(slave);server.shutdown();server.server_close();thread.join()

    def test_bad_preview_origin_does_not_rotate(self):
        p=self.run_cli('preview','access','box','80','--base-url','http://example.com')
        self.assertEqual(p.returncode,1); self.assertEqual(self.requests,[])

if __name__ == '__main__': unittest.main()
