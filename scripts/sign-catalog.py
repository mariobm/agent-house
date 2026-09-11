#!/usr/bin/env python3
"""Sign an image/release catalog with an offline Ed25519 PEM key.
Usage: sign-catalog.py PAYLOAD.json PRIVATE.pem CATALOG.json
Private keys must never be committed or uploaded to image storage.
"""
import base64
import json
from pathlib import Path
import subprocess
import sys
import tempfile

payload, key, output = map(Path, sys.argv[1:])
json.loads(payload.read_bytes())
with tempfile.TemporaryDirectory() as temp:
    signature = Path(temp) / 'signature'
    subprocess.run(['openssl', 'pkeyutl', '-sign', '-inkey', str(key), '-rawin', '-in', str(payload), '-out', str(signature)], check=True)
    output.write_text(json.dumps({'payload': base64.b64encode(payload.read_bytes()).decode(), 'signature': base64.b64encode(signature.read_bytes()).decode()}, separators=(',', ':')) + '\n')
