#!/usr/bin/env python3
"""Comparative live-VM probe. Run in `unshare --net`; retain failures as results."""
import argparse
import base64
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import socketserver
import struct
import subprocess
import threading
import time
from live import execute
from isolation import setup_namespace

PAYLOAD=b'abcdefgh'*(1024*1024)


class HTTP(http.server.BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*args): pass
    def do_GET(self):
        data=PAYLOAD if self.path=='/bulk' else b'probe-ok'
        try:
            self.send_response(200)
            self.send_header('Content-Length',str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError,ConnectionResetError): pass
    def do_POST(self):
        size=int(self.headers['Content-Length'])
        data=self.rfile.read(size)
        ok=data==PAYLOAD
        self.send_response(200 if ok else 400)
        self.send_header('Content-Length','2')
        self.end_headers()
        self.wfile.write(b'OK' if ok else b'NO')


class DNS(socketserver.BaseRequestHandler):
    def handle(self):
        query, sock = self.request
        # Controlled resolver: one uncompressed question for recovery.test.
        name = b'\x08recovery\x04test\x00'
        end = 12 + len(name)
        if len(query) < end + 4 or query[12:end].lower() != name:
            return
        question = query[12:end + 4]
        qtype, qclass = struct.unpack('!HH', query[end:end + 4])
        answer = b''
        if qtype == 1 and qclass == 1:
            answer = b'\xc0\x0c' + struct.pack('!HHIH', 1, 1, 0, 4) + socket.inet_aton('11.0.0.1')
        header = query[:2] + struct.pack('!HHHHH', 0x8180, 1, bool(answer), 0, 0)
        sock.sendto(header + question + answer, self.client_address)


def stats(pid):
    p=Path('/proc')/str(pid)
    fields=(p/'stat').read_text().rsplit(')',1)[1].split()
    memory={line.split(':')[0]:line.split(':')[1].strip() for line in (p/'status').read_text().splitlines() if ':' in line}
    return dict(cpu_seconds=(int(fields[11])+int(fields[12]))/os.sysconf('SC_CLK_TCK'),
                rss_kib=int(memory['VmRSS'].split()[0]),hwm_kib=int(memory['VmHWM'].split()[0]),
                fds=len(list((p/'fd').iterdir())),threads=int(memory['Threads']))


def ctl(path,command):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(15)
        s.connect(str(path))
        s.sendall((command+'\n').encode())
        return s.makefile('rb').readline(65536).decode().strip()


def stop(child):
    if child.poll() is None: child.terminate()
    try: child.wait(timeout=5)
    except subprocess.TimeoutExpired:
        child.kill(); child.wait()


GUEST_BENCH = '''
import concurrent.futures, hashlib, http.client, json, math, statistics, time
payload=b'abcdefgh'*(1024*1024)
def request(path='/', data=None):
    c=http.client.HTTPConnection('11.0.0.1',18080,timeout=8)
    try:
        c.request('POST' if data else 'GET',path,body=data)
        r=c.getresponse(); content=r.read()
        assert r.status==200,r.status
        expected=b'OK' if data else (payload if path=='/bulk' else b'probe-ok')
        assert hashlib.sha256(content).digest()==hashlib.sha256(expected).digest()
    finally: c.close()
result={}
for name,operation in [('latency',lambda: request()),('download',lambda: request('/bulk')),('upload',lambda: request('/',payload))]:
    times=[]
    try:
        for _ in range(20 if name=='latency' else 3):
            t=time.monotonic(); operation(); times.append(time.monotonic()-t)
        result[name]={'seconds':times,'median_seconds':statistics.median(times),'p95_seconds':sorted(times)[math.ceil(.95*len(times))-1]}
        if name!='latency': result[name]['mib_per_second']=8/statistics.median(times)
    except Exception as e: result[name]={'error':repr(e),'completed':len(times)}
try:
    t=time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(lambda _: request(),range(100)))
    result['churn']={'requests':100,'concurrency':4,'seconds':time.monotonic()-t}
except Exception as e: result['churn']={'error':repr(e)}
print(json.dumps(result))
'''



def qualification_failed(summary):
    for result in summary.values():
        if 'harness_error' in result: return True
        for name in ['setup','benchmark','post-load-health','after-pause','after-cold-restore',
                     'before-netd-restart','after-netd-restart']:
            if result.get(name,{}).get('exit_code') != 0: return True
        for check in result.get('recovery-cycles', []):
            if check.get('exit_code') != 0: return True
        if len(result.get('recovery-cycles', [])) != 6: return True
        for kind in ['header', 'body']:
            if result.get(f'after-partial-{kind}', {}).get('exit_code') != 0: return True
            if not result.get(f'partial-{kind}-snapshot', '').startswith('OK'): return True
        try:
            bench=json.loads(result['benchmark']['stdout'])
            if any('error' in bench.get(name,{'error':'missing'}) for name in ['latency','download','upload','churn']):
                return True
        except (KeyError,ValueError): return True
    return not summary


def main():
    ap=argparse.ArgumentParser()
    ap.add_argument('root',type=Path)
    ap.add_argument('--image',required=True,type=Path)
    ap.add_argument('--vmm',required=True)
    ap.add_argument('--rust',required=True)
    ap.add_argument('--go',required=True)
    ap.add_argument('--order',default='rust,go')
    args=ap.parse_args()
    setup_namespace()
    server=http.server.ThreadingHTTPServer(('11.0.0.1',18080),HTTP)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    dns=socketserver.UDPServer(('127.0.0.1',53),DNS)
    threading.Thread(target=dns.serve_forever,daemon=True).start()
    summary={}
    try:
        for candidate in args.order.split(','):
            work=args.root/candidate
            work.mkdir(parents=True,exist_ok=False)
            children=[]; logs=[]; entry={}; summary[candidate]=entry
            def spawn(argv,logname,env=None):
                log=(work/logname).open('w'); logs.append(log)
                child=subprocess.Popen(argv,stdout=log,stderr=log,stdin=subprocess.DEVNULL,env=env)
                children.append(child); return child
            def netd():
                (work/'net.sock').unlink(missing_ok=True)
                argv=([args.rust,str(work/'net.sock'),'127.0.0.1','11.0.0.2'] if candidate=='rust' else
                      [args.go,'--net-uds',str(work/'net.sock'),'--gw-ip','100.64.0.1','--prefix','24','--mac','02:00:00:00:00:01'])
                p=spawn(argv,f'netd-{len(children)}.log')
                deadline=time.monotonic()+5
                while not (work/'net.sock').exists():
                    if p.poll() is not None or time.monotonic()>deadline: raise RuntimeError('netd readiness')
                    time.sleep(.01)
                return p
            def boot(spec):
                for name in ['c.sock','f.sock','k.sock']: (work/name).unlink(missing_ok=True)
                (work/'spec.json').write_text(json.dumps(spec))
                vm=spawn([args.vmm,str(work/'spec.json')],f'vmm-{len(children)}.log',dict(
                    PATH='/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin', HOME='/root',
                    LANG='C.UTF-8',LD_LIBRARY_PATH='/usr/local/lib64'))
                deadline=time.monotonic()+30
                while True:
                    if vm.poll() is not None: raise RuntimeError('VMM exited')
                    try:
                        if execute(work/'c.sock','true',timeout=1)['exit_code']==0: return vm
                    except (OSError,RuntimeError): pass
                    if time.monotonic()>deadline: raise RuntimeError('guest readiness')
                    time.sleep(.05)
            def check(label,command):
                try:
                    r=execute(work/'c.sock',command,timeout=120)
                    out=base64.b64decode(r['stdout_b64']).decode(errors='replace')
                    err=base64.b64decode(r['stderr_b64']).decode(errors='replace')
                    entry[label]=dict(exit_code=r['exit_code'],stdout=out,stderr=err)
                    return r['exit_code']==0
                except Exception as e:
                    entry[label]={'error':repr(e)}; return False
            probe='curl -4 -fsS --connect-timeout 2 --max-time 3 http://11.0.0.1:18080/ | grep -qx probe-ok'
            # pipefail ensures a failed curl cannot be hidden by the assertion.
            probe='set -o pipefail; '+probe + ' && curl -4 -fsS --connect-timeout 2 --max-time 3 http://recovery.test:18080/ | grep -qx probe-ok'
            configure='ip link set eth0 up; ip addr add 100.64.0.2/24 dev eth0; ip route add default via 100.64.0.1; printf "nameserver 100.64.0.1\\n" > /etc/resolv.conf; '
            try:
                shutil.copyfile(args.image,work/'guest.ext4')
                spec=dict(vcpus=1,mem_mib=512,log_level=3,root_disk=str(work/'guest.ext4'),root_disk_format='raw',
                          pid1=True,exec_path='/init.krun',net_uds=str(work/'net.sock'),net_mac='02:00:00:00:00:02',
                          vsock_control_uds=str(work/'c.sock'),vsock_forward_uds=str(work/'f.sock'),
                          control_socket_uds=str(work/'k.sock'),env=[])
                network=netd(); vm=boot(spec)
                if not check('setup','mount -t proc proc /proc; mount -t sysfs sysfs /sys; mount -t devtmpfs devtmpfs /dev; ip link set lo up; '+configure+probe):
                    raise RuntimeError('setup failed')
                before=stats(network.pid); time.sleep(2); after=stats(network.pid)
                entry['idle']=dict(cpu_seconds=after['cpu_seconds']-before['cpu_seconds'],window_seconds=2,**{k:v for k,v in after.items() if k!='cpu_seconds'})
                encoded=base64.b64encode(GUEST_BENCH.encode()).decode()
                command=f"python3 -c \"import base64; exec(base64.b64decode('{encoded}'))\""
                before=stats(network.pid)
                check('benchmark',command)
                after=stats(network.pid)
                entry['loaded']=dict(cpu_seconds=after['cpu_seconds']-before['cpu_seconds'],**{k:v for k,v in after.items() if k!='cpu_seconds'})
                check('post-load-health',probe)
                entry['pause']=ctl(work/'k.sock','PAUSE')
                entry['resume']=ctl(work/'k.sock','RESUME')
                check('after-pause',probe)
                check('sync','sync')
                bundle=work/'snapshot'
                entry['snapshot']=ctl(work/'k.sock',f'SNAPSHOT {bundle}')
                if entry['snapshot'].startswith('OK'):
                    shutil.copyfile(work/'guest.ext4',work/'frozen.ext4')
                    stop(vm); stop(network)
                    shutil.copyfile(work/'frozen.ext4',work/'guest.ext4')
                    network=netd()
                    spec['snapshot_dir']=str(bundle)
                    try:
                        vm=boot(spec)
                        check('after-cold-restore',probe)
                    except Exception as e:
                        entry['after-cold-restore']={'error':repr(e)}
                        for p in children:
                            if p!=network and p.poll() is None: stop(p)
                        spec.pop('snapshot_dir')
                        stop(network); network=netd(); vm=boot(spec)
                        check('reboot-after-restore-failure',configure+probe)
                else:
                    entry['resume-after-snapshot-error']=ctl(work/'k.sock','RESUME')
                # Isolate netd restart from any earlier cold-restore failure.
                stop(vm); stop(network)
                spec.pop('snapshot_dir',None)
                network=netd(); vm=boot(spec)
                if not check('before-netd-restart',configure+probe):
                    raise RuntimeError('fresh boot before netd restart failed')
                # SIGKILL exactly our child. Fresh netd, same running VM.
                if network.poll() is None: network.kill(); network.wait()
                network=netd()
                check('after-netd-restart',probe)
                entry['recovery-cycles']=[]
                for cycle in range(3):
                    # Snapshot the current generation, then restore in a fresh process.
                    if not check(f'sync-{cycle}', 'sync'): raise RuntimeError('sync failed')
                    bundle=work/f'cycle-{cycle}'
                    reply=ctl(work/'k.sock',f'SNAPSHOT {bundle}')
                    if not reply.startswith('OK'): raise RuntimeError(reply)
                    stop(vm); stop(network)
                    network=netd()
                    spec['snapshot_dir']=str(bundle)
                    vm=boot(spec)
                    label=f'cold-restore-{cycle}'
                    check(label,probe); entry['recovery-cycles'].append(entry[label])
                    # Leave the backend absent long enough to exercise failed reconnects.
                    network.kill(); network.wait(); time.sleep(.3)
                    network=netd()
                    label=f'netd-restart-{cycle}'
                    check(label,probe); entry['recovery-cycles'].append(entry[label])
                for kind, fragment in [('header', b'\x00\x00'), ('body', struct.pack('!I', 60) + b'ab')]:
                    network.kill(); network.wait()
                    (work/'net.sock').unlink(missing_ok=True)
                    with socket.socket(socket.AF_UNIX) as listener:
                        listener.settimeout(3)
                        listener.bind(str(work/'net.sock')); listener.listen(1)
                        peer, _ = listener.accept()
                        with peer:
                            peer.sendall(fragment)
                            # Give the receiver an opportunity to enter the partial-frame path.
                            time.sleep(.1)
                            entry[f'partial-{kind}-snapshot']=ctl(work/'k.sock',f'SNAPSHOT {work / ("partial-" + kind)}')
                            entry[f'partial-{kind}-resume']=ctl(work/'k.sock','RESUME')
                    network=netd()
                    check(f'after-partial-{kind}',probe)
            except Exception as e:
                entry['harness_error']=repr(e)
            finally:
                for p in reversed(children): stop(p)
                for log in logs: log.close()
                (args.root/'qualification.json').write_text(json.dumps(summary,indent=2))
                print(json.dumps({candidate:entry}),flush=True)
    finally:
        server.shutdown(); server.server_close()
        dns.shutdown(); dns.server_close()
    if qualification_failed(summary):
        raise SystemExit(1)


if __name__=='__main__': main()
