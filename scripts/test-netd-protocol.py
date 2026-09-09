#!/usr/bin/env python3
"""Linux gateway protocol regression gate, no VMs and no external network.

Usage: test-netd-protocol.py /absolute/path/to/ahvm-netd
Creates one gateway and a positive TCP listener in a temporary directory.
"""
import json
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import time


def checksum(data):
    if len(data) % 2: data += b'\0'
    n = sum(struct.unpack('!%dH' % (len(data)//2), data))
    while n >> 16: n = (n & 65535) + (n >> 16)
    return (~n) & 65535


def syn(port):
    src, dst = bytes([100,64,0,2]), bytes([127,0,0,1])
    tcp = bytearray(struct.pack('!HHIIBBHHH',40000,port,0,0,0x50,2,4096,0,0))
    struct.pack_into('!H',tcp,16,checksum(src+dst+struct.pack('!BBH',0,6,len(tcp))+tcp))
    ip = bytearray(struct.pack('!BBHHHBBH',0x45,0,40,0,0,64,6,0)+src+dst)
    struct.pack_into('!H',ip,10,checksum(ip))
    return bytearray(bytes([2,0,0,0,0,1,2,0,0,0,0,2,8,0])+ip+tcp)


def run(binary):
    with tempfile.TemporaryDirectory(prefix='ahvm-protocol-') as directory, socket.socket() as listener:
        root = Path(directory)
        listener.bind(('127.0.0.1',0)); listener.listen(); listener.settimeout(0.3)
        port = listener.getsockname()[1]
        path = root/'net.sock'
        (root/'net.json').write_text(json.dumps({'ethernet_contract':1,'socket':str(path),
            'resolver':'127.0.0.1','private_access':[f'127.0.0.1:{port}']}))
        with (root/'netd.log').open('wb') as log:
            proc = subprocess.Popen([binary,str(root/'net.json')],stdout=log,stderr=log)
            try:
                deadline=time.monotonic()+5
                while not path.exists():
                    assert proc.poll() is None
                    assert time.monotonic()<deadline
                    time.sleep(0.01)
                def link():
                    assert proc.poll() is None, (root/'netd.log').read_text()
                    s=socket.socket(socket.AF_UNIX);s.settimeout(7);s.connect(str(path));return s
                # Invalid and fragmented framing closes only this link; the same
                # gateway accepts a fresh connection after every failure.
                for length in [0,1,13,131073,0xffffffff]:
                    with link() as s:
                        header=struct.pack('!I',length)
                        for byte in header:s.sendall(bytes([byte]))
                        assert s.recv(1)==b''
                with link() as s:
                    s.sendall(struct.pack('!I',100)+b'partial')
                    start=time.monotonic();assert s.recv(1)==b''
                    assert 4.5 <= time.monotonic()-start <= 7
                packet=syn(port)
                invalid=[]
                corrupt=packet.copy();corrupt[-1]^=1;invalid.append(corrupt)
                spoof=packet.copy();spoof[6]^=1;invalid.append(spoof)
                fragment=packet.copy();fragment[20]=0x20;fragment[24:26]=b'\0\0'
                struct.pack_into('!H',fragment,24,checksum(fragment[14:34]));invalid.append(fragment)
                with link() as s:
                    for malformed in invalid:
                        s.sendall(struct.pack('!I',len(malformed))+malformed)
                    try:
                        unexpected,_=listener.accept();unexpected.close()
                    except TimeoutError:pass
                    else:raise AssertionError('invalid packet opened host connection')
                    # Positive control on the same link and destination.
                    data=struct.pack('!I',len(packet))+packet
                    for byte in data:s.sendall(bytes([byte]))
                    listener.settimeout(3)
                    c,_=listener.accept();c.close()
                assert proc.poll() is None
                print(json.dumps({'result':'pass','checks':['malformed frame lengths','partial-frame timeout',
                    'bad TCP checksum denied before dial','spoofed MAC denied','fragments denied',
                    'valid SYN positive control','gateway survives link churn'],'vm_count':0}))
            finally:
                proc.terminate();proc.wait(timeout=5)


if __name__=='__main__': run(sys.argv[1])
