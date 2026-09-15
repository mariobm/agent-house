#!/usr/bin/env python3
"""Measure one disposable VM at a time; token and terminal contents are never printed.
Uses an existing daemon and CLI. Run against an isolated qualification deployment.
Emits small JSON records to stdout; retain aggregate results in Markdown, not raw logs.
"""
import argparse
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--endpoint', required=True)
    parser.add_argument('--token-file', type=Path, required=True)
    parser.add_argument('--cli', required=True)
    parser.add_argument('--mode', choices=['local', 'replicated'], required=True)
    parser.add_argument('--samples', type=int, default=5)
    parser.add_argument('--volume-root', type=Path, help='isolated supervisor root; wait for reclamation between samples')
    args = parser.parse_args()
    if not 1 <= args.samples <= 20:
        parser.error('samples must be between 1 and 20')
    token = args.token_file.read_text().strip()

    def api(path, method='GET', body=None):
        req = urllib.request.Request(args.endpoint.rstrip('/') + '/v1' + path,
            method=method, headers={'Authorization': 'Bearer ' + token,
                                   'Content-Type': 'application/json'},
            data=None if body is None else json.dumps(body).encode())
        with urllib.request.urlopen(req, timeout=600) as response:
            raw = response.read()
            return json.loads(raw) if raw else None

    def shell(name):
        master, slave = pty.openpty()
        env = {k: v for k, v in os.environ.items() if not k.startswith('AHVM_')}
        env.update(AHVM_TOKEN=token, TERM='xterm-256color')
        started = time.monotonic()
        proc = subprocess.Popen([args.cli, '--endpoint', args.endpoint, 'shell', name],
            stdin=slave, stdout=slave, stderr=slave, env=env, start_new_session=True)
        os.close(slave)
        output = b''
        try:
            deadline = started + 60
            sent = False
            while time.monotonic() < deadline:
                if select.select([master], [], [], .1)[0]:
                    output = (output + os.read(master, 65536))[-131072:]
                    # Wait for a real shell prompt, not just a session receipt.
                    if not sent and (b'# ' in output or b'$ ' in output):
                        os.write(master, b"printf '%s%s\\n' '__AHVM_' 'READY__'\n")
                        sent = True
                    if sent and b'__AHVM_READY__' in output:
                        elapsed = time.monotonic() - started
                        os.write(master, b'exit\n')
                        proc.wait(timeout=10)
                        if proc.returncode != 0:
                            raise RuntimeError('shell failed after marker')
                        return elapsed
                if proc.poll() is not None:
                    raise RuntimeError('shell exited before marker')
            raise TimeoutError('shell marker deadline')
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
            os.close(master)

    for sample in range(args.samples):
        name = 'timing-' + uuid.uuid4().hex[:12]
        path = '/sandboxes/' + name
        print(json.dumps({'sample': sample, 'id': name, 'event': 'begin'}), flush=True)
        vm = None
        try:
            started = time.monotonic()
            vm = api('/sandboxes', 'POST', dict(name=name, cpus=1, memory_mb=2048,
                                                storage_mode=args.mode))
            created = time.monotonic() - started
            attached = shell(name)
            print(json.dumps(dict(sample=sample, id=name, mode=args.mode,
                volume_id=vm.get('storage', {}).get('volume_id'),
                create_s=created, shell_s=attached, create_to_shell_s=created+attached)), flush=True)
            started = time.monotonic()
            api(path + '/stop', 'POST', {})
            stopped = time.monotonic() - started
            started = time.monotonic()
            api(path + '/start', 'POST', {})
            resumed = time.monotonic() - started
            attached = shell(name)
            print(json.dumps(dict(sample=sample, id=name, mode=args.mode,
                stop_s=stopped, start_s=resumed, shell_s=attached,
                start_to_shell_s=resumed+attached)), flush=True)
        finally:
            # Delete only our random ID, including partially completed creates.
            try:
                api(path, 'DELETE')
            except urllib.error.HTTPError as error:
                if error.code != 404:
                    raise
            if args.volume_root and vm and vm.get('storage', {}).get('volume_id'):
                volume_id = vm['storage']['volume_id']
                if len(volume_id) != 64 or any(c not in '0123456789abcdef' for c in volume_id):
                    raise RuntimeError('invalid volume ID')
                record = args.volume_root / 'volumes' / volume_id / 'record.json'
                deadline = time.monotonic() + 180
                while not json.loads(record.read_text()).get('reclaimed'):
                    if time.monotonic() >= deadline:
                        raise TimeoutError('reclamation deadline; stop before creating another VM')
                    time.sleep(.5)


if __name__ == '__main__':
    main()
