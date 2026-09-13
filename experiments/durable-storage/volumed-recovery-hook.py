#!/usr/bin/env python3
"""Root-only isolated gate hook for the Rust service; never installed."""
import json, os, signal, socket, struct, subprocess, sys, time
from pathlib import Path
root=Path(os.environ['AHVM_SERVICE_ROOT'])
meta=json.loads((root/'gate.json').read_text())
records=[json.loads(p.read_text()) for p in (root/'volumes').glob('*/record.json')]
record=next(r for r in records if Path(r['sandbox']).name=='replica')

def identity(pid):
    try:
        f=Path(f'/proc/{pid}/stat').read_text().rsplit(')',1)[1].split()
        if f[0]=='Z': return None
        return dict(pid=pid,start=int(f[19]),boot=Path('/proc/sys/kernel/random/boot_id').read_text().strip())
    except FileNotFoundError:return None

def kill(p):
    fd=os.pidfd_open(p['pid'])
    try:
        assert identity(p['pid'])==p
        signal.pidfd_send_signal(fd,signal.SIGKILL)
    finally:os.close(fd)
    end=time.monotonic()+10
    while identity(p['pid'])==p:
        assert time.monotonic()<end
        time.sleep(.05)

def request(op, r=None, image=None):
    r = r or record
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(600 if op=="prepare" else 40);s.connect(str(root/'service.sock'))
        s.sendall(json.dumps(dict(version=1,volume_id=r['id'],sandbox_dir=r['sandbox'],operation=op,image=image)).encode()+b'\n')
        line=s.makefile("rb").readline(4097)
        assert len(line)<=4096 and line.endswith(b"\n")
        return json.loads(line)

op=sys.argv[1]
if op=='restart':
    # A second tiny raw volume exercises independent service slots, no extra VM.
    import secrets
    peer_id=secrets.token_hex(32)
    peer_dir=Path(record['sandbox']).parent/'peer'
    peer_dir.mkdir()
    (peer_dir/'sandbox.json').write_text(json.dumps({'info':{'storage':{'volume_id':peer_id,'mode':'replicated'}}}))
    image=root/'peer.raw'
    with image.open('wb') as f:f.truncate(1024*1024)
    peer={'id':peer_id,'sandbox':str(peer_dir)}
    assert request('prepare',peer,str(image))['ok']
    response=request('attach',peer);assert response['ok']
    with open(response['device'],'r+b',buffering=0) as f:
        f.seek(65536);f.write(b'peer survives');os.fsync(f.fileno())
    meta['peer']=peer
    meta['peer_worker']=json.loads((root/'volumes'/peer_id/'record.json').read_text())['worker']
    kill(meta['supervisor'])
    with (root/'gate.log').open('ab') as log:
        p=subprocess.Popen(meta['command'],stdout=log,stderr=log,start_new_session=True)
    meta['supervisor']=identity(p.pid);(root/'gate.json').write_text(json.dumps(meta))
    end=time.monotonic()+30
    while True:
        try:
            if request('inspect')['ok']:break
        except (ConnectionRefusedError,FileNotFoundError,ConnectionResetError):pass
        assert p.poll() is None and time.monotonic()<end
        time.sleep(.1)
elif op=='kill-storage':
    meta['killed_worker']=record['worker'];(root/'gate.json').write_text(json.dumps(meta))
    kill(record['worker'])
elif op=='recovered':
    end=time.monotonic()+120
    while True:
        latest=json.loads((root/'volumes'/record['id']/'record.json').read_text())
        if latest['worker'] is not None and latest['worker'] != meta['killed_worker'] and request('inspect')['ok']:break
        assert time.monotonic()<end
        time.sleep(.2)
elif op=='cleanup-peer':
    import shutil
    peer=meta['peer'];latest=json.loads((root/'volumes'/peer['id']/'record.json').read_text())
    assert latest['worker']==meta['peer_worker']
    response=request('inspect',peer);assert response['ok']
    with open(response['device'],'rb',buffering=0) as f:
        f.seek(65536);assert f.read(13)==b'peer survives'
    assert request('sync',peer)['ok']
    assert request('delete',peer)['ok']
    shutil.rmtree(peer['sandbox'])
    print('PASS independent peer volume retained worker and data')
elif op=='busy-detach':assert not request('detach')['ok']
else:raise ValueError(op)
print('PASS',op,flush=True)
