#!/usr/bin/env python3
"""Merge release metadata into a verified catalog, ready for offline signing.
Usage: assemble-catalog.py SIGNED_BASE OUTPUT_PAYLOAD PLATFORM.json ...
"""
import base64
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

base, output = map(Path, sys.argv[1:3])
envelope = json.loads(base.read_bytes())
with tempfile.TemporaryDirectory() as temp:
    p = Path(temp)
    payload = base64.b64decode(envelope['payload'], validate=True)
    (p/'payload').write_bytes(payload)
    (p/'signature').write_bytes(base64.b64decode(envelope['signature'], validate=True))
    subprocess.run(['openssl','pkeyutl','-verify','-pubin','-inkey',str(Path(__file__).resolve().parent.parent/'packaging/keys/releases.pem'),'-rawin','-in',str(p/'payload'),'-sigfile',str(p/'signature')],check=True,stdout=subprocess.DEVNULL)
    catalog = json.loads(payload)
for path in sys.argv[3:]:
    metadata = json.loads(Path(path).read_bytes())
    for kind in ['cli', 'client', 'server']:
        catalog.setdefault(kind, {}).update(metadata.get(kind, {}))
catalog['expires'] = int(time.time()) + 90*86400
output.write_text(json.dumps(catalog,indent=2)+'\n')
