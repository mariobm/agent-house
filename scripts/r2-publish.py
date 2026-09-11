#!/usr/bin/env python3
"""Upload with a bucket-scoped token, never the broad Cloudflare admin token.
Usage: r2-publish.py FILE KEY [--immutable]
AHVM_R2_TOKEN_FILE defaults to ~/.config/ahvm-release/r2-publisher.json.
Requires AWS CLI; credentials stay in the subprocess environment.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

file = Path(sys.argv[1])
key = sys.argv[2]
if key.startswith('/') or '..' in key.split('/') or not key:
    raise SystemExit('Invalid object key')
private = Path(os.environ.get('AHVM_R2_TOKEN_FILE', str(Path.home() / '.config/ahvm-release/r2-publisher.json')))
token = json.loads(private.read_text())
account = '7bab54566d0dba9bb4492b716aeeed2b'
env = {**os.environ, 'AWS_ACCESS_KEY_ID': token['id'], 'AWS_SECRET_ACCESS_KEY': hashlib.sha256(token['value'].encode()).hexdigest(), 'AWS_DEFAULT_REGION': 'auto', 'AWS_EC2_METADATA_DISABLED': 'true'}
for name in ['AWS_PROFILE', 'AWS_DEFAULT_PROFILE', 'AWS_SESSION_TOKEN']:
    env.pop(name, None)
base = ['aws', '--endpoint-url', f'https://{account}.r2.cloudflarestorage.com']
with file.open('rb') as source:
    digest = hashlib.sha256()
    for chunk in iter(lambda: source.read(131072), b''):
        digest.update(chunk)
sha = digest.hexdigest()
with tempfile.TemporaryDirectory() as temp:
    config = Path(temp) / 'config'
    config.write_text('[default]\nregion=auto\nretry_mode=adaptive\nmax_attempts=10\ns3=\n    max_concurrent_requests=2\n    multipart_chunksize=32MB\n')
    env['AWS_CONFIG_FILE'] = str(config)
    if '--immutable' in sys.argv:
        head = subprocess.run(base + ['s3api', 'head-object', '--bucket', 'ahvm-images', '--key', key], env=env, capture_output=True, text=True)
        if head.returncode == 0:
            if json.loads(head.stdout).get('Metadata', {}).get('sha256') == sha:
                print('Verified immutable object already exists')
                sys.exit(0)
            raise SystemExit('Refusing to replace an existing immutable object')
        if '404' not in head.stderr and 'Not Found' not in head.stderr:
            raise SystemExit('Unable to check existing object; refusing upload')
    subprocess.run(base + ['s3', 'cp', str(file), 's3://ahvm-images/' + key, '--no-progress', '--metadata', 'sha256=' + sha, '--content-type', 'application/json' if key.endswith('.json') else 'application/gzip', '--cache-control', 'public, max-age=31536000, immutable' if '--immutable' in sys.argv else 'public, max-age=60'], env=env, check=True)
