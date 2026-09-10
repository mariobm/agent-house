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
        import fcntl, pty, termios
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
            _,error=p.communicate(timeout=10)
            self.assertEqual(p.returncode,1,error)
            self.assertEqual(termios.tcgetattr(master),before)
        finally:
            os.close(master);os.close(slave)

    def test_bad_preview_origin_does_not_rotate(self):
        p=self.run_cli('preview','access','box','80','--base-url','http://example.com')
        self.assertEqual(p.returncode,1); self.assertEqual(self.requests,[])

if __name__ == '__main__': unittest.main()
