#!/usr/bin/env python3
"""Run under `unshare --net`: no host networking or internet routes are changed."""
import argparse
import json
import random
import socket
import struct
import subprocess
import threading
import time
from pathlib import Path


def checksum(data):
    if len(data) % 2:
        data += b'\0'
    total = sum(struct.unpack('!%dH' % (len(data)//2), data))
    while total >> 16:
        total = (total & 65535) + (total >> 16)
    return (~total) & 65535


def syn(dst='11.0.0.1', source='100.64.0.2', mac=b'\x02\0\0\0\0\x02',
        port=18080, sport=42000, fragment=0, flags=2):
    src, dest = socket.inet_aton(source), socket.inet_aton(dst)
    tcp = struct.pack('!HHIIBBHHH', sport, port, 100, 0, 5 << 4, flags, 65535, 0, 0)
    pseudo = src + dest + struct.pack('!BBH', 0, 6, len(tcp))
    tcp = tcp[:16] + struct.pack('!H', checksum(pseudo+tcp)) + tcp[18:]
    ip = struct.pack('!BBHHHBBH4s4s', 0x45, 0, 40, 0, fragment, 64, 6, 0, src, dest)
    ip = ip[:10] + struct.pack('!H', checksum(ip)) + ip[12:]
    return b'\x02\0\0\0\0\x01' + mac + b'\x08\x00' + ip + tcp


def setup_namespace():
    # Require caller-created private net namespace before touching its interfaces.
    if Path('/proc/self/ns/net').readlink() == Path('/proc/1/ns/net').readlink():
        raise RuntimeError('must run under unshare --net')
    subprocess.run(['ip','link','set','lo','up'], check=True)
    for ip in ['11.0.0.1','11.0.0.2','10.0.0.1','169.254.169.254','100.64.0.3']:
        subprocess.run(['ip','addr','add',ip+'/32','dev','lo'], check=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('binary')
    ap.add_argument('work',type=Path)
    args = ap.parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    setup_namespace()
    upstream = socket.socket()
    upstream.bind(('0.0.0.0',18080))
    upstream.listen(128)
    upstream.settimeout(.1)
    connections = []
    stop = threading.Event()
    def accept():
        while not stop.is_set():
            try:
                client, _ = upstream.accept()
                connections.append(client)
            except socket.timeout:
                continue
    thread = threading.Thread(target=accept)
    thread.start()
    report = []
    cases = [
        ('allowed-control', syn(), True),
        ('host', syn(dst='11.0.0.2'), False),
        ('loopback', syn(dst='127.0.0.1'), False),
        ('private', syn(dst='10.0.0.1'), False),
        ('metadata', syn(dst='169.254.169.254'), False),
        ('other-sandbox', syn(dst='100.64.0.3'), False),
        ('spoofed-ip', syn(source='100.64.0.3'), False),
        ('spoofed-mac', syn(mac=b'\x02\0\0\0\0\x03'), False),
        ('fragmented', syn(fragment=0x2000), False),
        ('zero-destination-port', syn(port=0), False),
        ('zero-source-port', syn(sport=0), False),
        ('syn-fin', syn(flags=3), False),
    ]
    try:
        # All denied destinations really have a reachable service in this namespace.
        for addr in ['11.0.0.1','11.0.0.2','127.0.0.1','10.0.0.1','169.254.169.254','100.64.0.3']:
            with socket.create_connection((addr,18080),timeout=1):
                pass
        time.sleep(.1)
        for i, (name, frame, allowed) in enumerate(cases):
            uds = args.work/f'{i}.sock'
            uds.unlink(missing_ok=True)
            with (args.work/f'{name}.log').open('w') as log:
                child = subprocess.Popen([args.binary,str(uds),'127.0.0.1','11.0.0.2'],stdout=log,stderr=log)
                try:
                    deadline = time.monotonic()+5
                    while not uds.exists():
                        if child.poll() is not None or time.monotonic()>deadline:
                            raise RuntimeError('netd startup failed')
                        time.sleep(.01)
                    count = len(connections)
                    with socket.socket(socket.AF_UNIX) as client:
                        client.connect(str(uds))
                        client.sendall(struct.pack('!I',len(frame))+frame)
                        time.sleep(.15)
                        reached = len(connections)>count
                        alive = child.poll() is None
                    result = dict(case=name, upstream_reached=reached, process_alive=alive,
                                  passed=(reached == allowed and alive))
                    report.append(result)
                    print(json.dumps(result),flush=True)
                finally:
                    if child.poll() is None: child.terminate()
                    child.wait(timeout=5)
        # Deterministic malformed-frame corpus, not a substitute for fuzzing.
        uds=args.work/'malformed.sock'
        uds.unlink(missing_ok=True)
        with (args.work/'malformed.log').open('w') as log:
            child=subprocess.Popen([args.binary,str(uds),'127.0.0.1','11.0.0.2'],stdout=log,stderr=log)
            try:
                deadline=time.monotonic()+5
                while not uds.exists():
                    if child.poll() is not None or time.monotonic()>deadline: raise RuntimeError('startup')
                    time.sleep(.01)
                count=len(connections)
                rng=random.Random(42)
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(5)
                    client.connect(str(uds))
                    for i in range(2000):
                        frame=bytearray(rng.randbytes(rng.randrange(14,512)))
                        # Exercise actual protocol parsers, not only the MAC prefilter.
                        frame[6:12]=b'\x02\0\0\0\0\x02'
                        frame[12:14]=[b'\x08\x00',b'\x08\x06',b'\x86\xdd'][i%3]
                        if i%3==0 and len(frame)>=34:
                            frame[14]=0x45
                            frame[16:18]=struct.pack('!H',len(frame)-14)
                            frame[26:30]=socket.inet_aton('100.64.0.2')
                            frame[30:34]=socket.inet_aton('11.0.0.1')
                            frame[24:26]=b'\0\0'
                            frame[24:26]=struct.pack('!H',checksum(frame[14:34]))
                        client.sendall(struct.pack('!I',len(frame))+frame)
                    time.sleep(.2)
                    report.append(dict(case='malformed-corpus',process_alive=child.poll() is None,
                                       passed=child.poll() is None and len(connections)==count))
            finally:
                if child.poll() is None: child.terminate()
                child.wait(timeout=5)
        uds=args.work/'capacity.sock'
        uds.unlink(missing_ok=True)
        with (args.work/'capacity.log').open('w') as log:
            child=subprocess.Popen([args.binary,str(uds),'127.0.0.1','11.0.0.2'],stdout=log,stderr=log)
            drain_stop=threading.Event()
            drain_thread=None
            try:
                deadline=time.monotonic()+5
                while not uds.exists():
                    if child.poll() is not None or time.monotonic()>deadline: raise RuntimeError('startup')
                    time.sleep(.01)
                count=len(connections)
                with socket.socket(socket.AF_UNIX) as client:
                    client.settimeout(2)
                    client.connect(str(uds))
                    def drain():
                        while not drain_stop.is_set():
                            try:
                                if not client.recv(65536): break
                            except socket.timeout: continue
                            except OSError: break
                    drain_thread=threading.Thread(target=drain)
                    drain_thread.start()
                    for port in range(43000,44000):
                        frame=syn(sport=port)
                        client.sendall(struct.pack('!I',len(frame))+frame)
                    time.sleep(.5)
                    reached=len(connections)-count
                    process=Path('/proc')/str(child.pid)
                    fds=len(list((process/'fd').iterdir()))
                    memory={line.split(':')[0]:line.split(':')[1].strip() for line in (process/'status').read_text().splitlines() if ':' in line}
                    rss=int(memory['VmRSS'].split()[0])
                    report.append(dict(case='1000-half-open-flows',upstream_connections=reached,fds=fds,rss_kib=rss,
                                       passed=child.poll() is None and reached==64 and fds<=80 and rss<32768))
                    drain_stop.set()
                    client.shutdown(socket.SHUT_RDWR)
                    drain_thread.join(timeout=3)
            finally:
                drain_stop.set()
                if child.poll() is None: child.terminate()
                child.wait(timeout=5)
                if drain_thread: drain_thread.join(timeout=3)
    finally:
        stop.set()
        thread.join()
        upstream.close()
        for client in connections: client.close()
        (args.work/'isolation.json').write_text(json.dumps(report,indent=2))
    if not all(r['passed'] for r in report): raise SystemExit(1)


if __name__=='__main__': main()
