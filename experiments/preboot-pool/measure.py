#!/usr/bin/env python3
"""Qualification only. Requires a disposable, isolated daemon with no other VMs.

Uses the real daemon and CLI, but models Cloud ownership in a local SQLite
ledger. Timings are NOT deployed Cloud create timings. Never use a production
endpoint: this script changes its idle policy for the duration of the test.
"""
import argparse
import hashlib
import json
import os
import pty
import select
import subprocess
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from pool import Pool


def shell(cli, endpoint, token, name):
    master, slave = pty.openpty()
    env = {k:v for k,v in os.environ.items() if not k.startswith('AHVM_')}
    env.update(AHVM_TOKEN=token, TERM='xterm-256color')
    start = time.monotonic()
    proc = subprocess.Popen([cli,'--endpoint',endpoint,'shell',name],stdin=slave,stdout=slave,stderr=slave,env=env)
    os.close(slave)
    try:
        output = b''
        sent = False
        deadline = start + 30
        while time.monotonic() < deadline:
            if select.select([master],[],[],.1)[0]:
                try:
                    data = os.read(master,65536)
                except OSError:
                    break
                output = (output+data)[-131072:]
                if not sent and (b'# ' in output or b'$ ' in output):
                    os.write(master,b"printf '%s%s\\n' '__POOL_' 'READY__'\n")
                    sent = True
                if sent and b'__POOL_READY__' in output:
                    elapsed = time.monotonic()-start
                    os.write(master,b'exit\n')
                    proc.wait(timeout=10)
                    if proc.returncode != 0:
                        raise RuntimeError('shell exited unsuccessfully')
                    return elapsed
            if proc.poll() is not None:
                break
        raise RuntimeError('no usable shell prompt within deadline')
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:proc.wait(timeout=5)
            except subprocess.TimeoutExpired:proc.kill();proc.wait()
        os.close(master)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--endpoint',required=True)
    parser.add_argument('--token-file',type=Path,required=True)
    parser.add_argument('--cli',required=True)
    parser.add_argument('--engine-root',type=Path,required=True)
    parser.add_argument('--volume-root',type=Path,required=True)
    parser.add_argument('--image',type=Path,required=True)
    parser.add_argument('--ledger',type=Path,required=True)
    parser.add_argument('--samples',type=int,default=3)
    parser.add_argument('--kinds',nargs='+',choices=['ordinary','prepared-running','prepared-paused'],default=['ordinary','prepared-running','prepared-paused'])
    args=parser.parse_args()
    if not 1<=args.samples<=10:parser.error('samples must be 1..10')
    endpoint=args.endpoint.rstrip('/')
    # An explicit isolated loopback endpoint is required for this operator tool.
    u=urllib.parse.urlparse(endpoint)
    if u.scheme!='http' or u.hostname not in ('127.0.0.1','localhost') or u.port in (None,8080):
        parser.error('use an isolated loopback daemon, not port 8080')
    if args.ledger.exists():parser.error('use a fresh ledger; retained ledgers require inspection, not automatic reuse')
    token=args.token_file.read_text().strip()
    def api(path,method='GET',body=None):
        req=urllib.request.Request(endpoint+'/v1'+path,method=method,headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'},data=None if body is None else json.dumps(body).encode())
        with urllib.request.urlopen(req,timeout=180) as r:
            raw=r.read();return json.loads(raw) if raw else None
    assert api('/sandboxes')['sandboxes']==[], 'isolated daemon must be empty'
    original_policy=api('/admin/idle-policy')
    image=args.image.resolve()
    def fingerprint():
        st=image.stat()
        return st.st_dev,st.st_ino,st.st_size,st.st_mtime_ns,st.st_ctime_ns
    image_stat=fingerprint()
    digest=hashlib.sha256()
    with image.open('rb') as f:
        for block in iter(lambda:f.read(4*1024*1024),b''):digest.update(block)
    image_digest=digest.hexdigest()
    assert fingerprint()==image_stat, 'image changed while hashing'
    profile=dict(cpus=1,memory_mib=2048,storage='replicated',image_sha256=image_digest,network_bytes_per_sec=0)
    pool=Pool(args.ledger)
    observed={key:set() for key in ['boot_id','random','volume']}
    machine_ids=set()
    ssh_keys=set()
    results=[]
    def exec_vm(name,cmd):
        r=api('/sandboxes/'+name+'/exec','POST',{'argv':['bash','-lc',cmd]})
        assert r['exit_code']==0,'guest qualification command failed'
        return r['stdout']
    identity_script="""import json,os,glob,hashlib,pathlib
p=pathlib.Path('/etc/machine-id')
print(json.dumps(dict(boot_id=pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip(),machine_id=p.read_text().strip() if p.exists() else '',random=os.getrandom(32).hex(),keys=[hashlib.sha256(pathlib.Path(k).read_bytes()).hexdigest() for k in glob.glob('/etc/ssh/ssh_host_*_key.pub')],marker=pathlib.Path('/workspace/.pool-user-marker').exists())))"""
    import shlex
    try:
        for kind in args.kinds:
            api('/admin/idle-policy','PUT',{'pause_after_secs':5 if kind=='prepared-paused' else 0})
            for sample in range(args.samples):
                name='pool-'+uuid.uuid4().hex[:12]
                assert pool.reserve(name,profile),'one-VM budget was exceeded'
                try:
                    began=time.monotonic()
                    api('/sandboxes','POST',{'name':name,'cpus':1,'memory_mb':2048,'storage_mode':'replicated'})
                    create_s=time.monotonic()-began
                    record=args.engine_root/name
                    spec=json.loads((record/'sandbox.json').read_text())
                    assert Path(spec['backing']).resolve()==image,'daemon used a different base image'
                    before=json.loads((record/'state.json').read_text())
                    if kind=='ordinary':
                        shell_s=shell(args.cli,endpoint,token,name)
                        ready_s=create_s+shell_s
                    identity=json.loads(exec_vm(name,'python3 -c '+shlex.quote(identity_script)))
                    assert not identity['marker'],'user file leaked from a previously used VM'
                    volume=spec['info']['storage']['volume_id']
                    for key,value in [('volume',volume),('boot_id',identity['boot_id']),('random',identity['random'])]:
                        assert value and value not in observed[key], 'duplicate '+key
                        observed[key].add(value)
                    if identity['machine_id']:
                        assert identity['machine_id'] not in machine_ids,'duplicate machine-id from base image'
                        machine_ids.add(identity['machine_id'])
                    for key in identity['keys']:
                        assert key not in ssh_keys,'duplicated SSH host key from base image'
                        ssh_keys.add(key)
                    assert fingerprint()==image_stat, 'image generation changed during preparation'
                    pool.ready(name,profile)
                    if kind=='prepared-paused':
                        deadline=time.monotonic()+20
                        while api('/sandboxes/'+name)['state']!='paused':
                            if time.monotonic()>deadline:raise RuntimeError('did not idle-pause')
                            time.sleep(.2)
                    claim_started=time.monotonic()
                    assert pool.claim('tenant-'+name,'create-'+name,profile)==name
                    claim_s=time.monotonic()-claim_started
                    if kind!='ordinary':
                        shell_s=shell(args.cli,endpoint,token,name)
                        ready_s=time.monotonic()-claim_started
                    # Reopening after an ambiguous claim reply resolves to the same VM.
                    assert Pool(args.ledger).claim('tenant-'+name,'create-'+name,profile)==name
                    assert pool.claim('foreign-tenant','competing-request',profile) is None
                    after=json.loads((record/'state.json').read_text())
                    assert before['pid']==after['pid'],'claim rebooted the worker'
                    assert exec_vm(name,'cat /proc/sys/kernel/random/boot_id').strip()==identity['boot_id']
                    exec_vm(name,"printf 'private-user-data' > /workspace/.pool-user-marker")
                    result=dict(kind=kind,sample=sample+1,prepare_s=round(create_s,3),claim_ms=round(claim_s*1000,3),usable_shell_s=round(ready_s,3))
                    results.append(result)
                    print(json.dumps(result),flush=True)
                finally:
                    pool.deleting(name)
                    try:api('/sandboxes/'+name,'DELETE')
                    except urllib.error.HTTPError as e:
                        if e.code!=404:raise
                    # A missing row alone is not proof of released disk allocation.
                    deadline=time.monotonic()+120
                    while True:
                        retained=[json.loads(p.read_text()) for p in (args.volume_root/'volumes').glob('*/record.json')]
                        if all(v['reclaimed'] for v in retained):break
                        if time.monotonic()>deadline:raise RuntimeError('reclamation not confirmed; reservation retained')
                        time.sleep(.2)
                    assert api('/sandboxes')['sandboxes']==[]
                    pool.reclaimed(name)
        print(json.dumps(dict(identity_checks='passed',unique_boots=len(observed['boot_id']),unique_volumes=len(observed['volume']),machine_ids_present=len(machine_ids),ssh_host_keys_present=len(ssh_keys))),flush=True)
    finally:
        api('/admin/idle-policy','PUT',original_policy)


if __name__=='__main__':main()
