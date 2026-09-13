#!/usr/bin/env python3
"""Independent-process ownership qualification, one private 1-MiB R2 volume.
No VM or installed services. Removes local identities/journals; leaves the exact
printed R2 prefix for explicit cleanup. Uses an existing scoped private config.
"""
import argparse
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import time

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--config',required=True)
p.add_argument('--probe',required=True)
a=p.parse_args()
work=Path(tempfile.mkdtemp(prefix='ahvm-ownership-'))
volume='owner-'+str(time.time_ns())
print('Fixture volume:',volume,flush=True)
base=[a.probe,a.config,volume]
owner=None
try:
    subprocess.run(base+[str(work/'a'),'init'],check=True,timeout=30)
    owner=subprocess.Popen(base+[str(work/'a'),'hold'],stdout=subprocess.PIPE,text=True)
    with selectors.DefaultSelector() as ready:
        ready.register(owner.stdout,selectors.EVENT_READ)
        assert ready.select(30), 'owner readiness timeout'
        assert owner.stdout.readline().strip()=='LOCAL-DURABLE-READY'
    subprocess.run(base+[str(work/'b'),'blocked'],check=True,timeout=30)
    owner.kill();owner.wait(timeout=10)
    # A dead owner is NOT permission to take over with another identity.
    subprocess.run(base+[str(work/'b'),'blocked'],check=True,timeout=30)
    subprocess.run(base+[str(work/'a'),'resume-release'],check=True,timeout=300)
    subprocess.run(base+[str(work/'b'),'verify-release'],check=True,timeout=300)
    subprocess.run(base+[str(work/'a'),'blocked'],check=True,timeout=30)
    print('PASS exclusive ownership, SIGKILL journal recovery, drain, handoff and stale-owner rejection',flush=True)
finally:
    if owner is not None and owner.poll() is None:
        owner.kill();owner.wait(timeout=10)
    shutil.rmtree(work)
    config=json.loads(Path(a.config).read_text())
    print('R2 fixture cleanup prefix:',config['prefix']+'/'+volume+'/',flush=True)
