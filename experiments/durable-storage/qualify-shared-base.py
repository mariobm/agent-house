#!/usr/bin/env python3
"""Serial, disposable shared-base guest gate on an empty, drained node.
Run after operator prewarming. Failures retain their VM for diagnosis.
"""
import argparse
import json
from pathlib import Path
import time
import urllib.error
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--token-file', type=Path, required=True)
    parser.add_argument('--url', default='http://127.0.0.1:8082')
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--cycles', type=int, choices=(1, 2), default=2)
    args = parser.parse_args()
    token = args.token_file.read_text().strip()

    def call(method, path, body=None):
        request = urllib.request.Request(args.url + path, method=method,
            headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'},
            data=None if body is None else json.dumps(body).encode())
        try:
            response = urllib.request.urlopen(request, timeout=35)
        except urllib.error.HTTPError as error:
            response = error
        raw = response.read()
        return response.status, json.loads(raw) if raw else None

    def inventory():
        status, result = call('GET', '/v1/sandboxes')
        assert status == 200, status
        return result['sandboxes']

    assert inventory() == [], 'requires an empty, drained test node'
    assert 'lifecycle-storage-v1' in call('GET', '/v1/healthz')[1]['features']
    for cycle in range(args.cycles):
        vm = 'base-gate-' + uuid.uuid4().hex[:8]
        prefix = '/v1/sandboxes/' + vm
        timings = {}
        print('VM:', vm, flush=True)

        def operation(action):
            started = time.monotonic()
            key = 'base-gate-' + uuid.uuid4().hex
            body = dict(action=action, sandbox_id=vm)
            if action == 'create':
                body.update(cpus=1, memory_mb=1024, storage_mode='replicated')
            status, result = call('POST', '/v1/operations/' + key, body)
            deadline = started + 3700
            while status == 202 and time.monotonic() < deadline:
                time.sleep(0.25)
                status, result = call('GET', '/v1/operations/' + key)
            assert status == 200 and result['state'] == 'done', (status, result)
            assert 200 <= result['status'] < 300, result
            assert result['sandbox_state'] == dict(create='running', start='running', stop='stopped', delete='absent')[action], result
            timings[action] = round(time.monotonic() - started, 2)
            print(action, timings[action], 'seconds', flush=True)

        def execute(command):
            status, result = call('POST', prefix + '/exec', {'argv': ['bash', '-lc', command]})
            assert status == 200 and result['exit_code'] == 0, (status, result)
            return result['stdout']

        def wait_record(field, expected=True):
            started = time.monotonic()
            while time.monotonic() - started < 180:
                for path in (args.state_dir / 'volumes').glob('*/record.json'):
                    record = json.loads(path.read_text())
                    if Path(record['sandbox']).name == vm and record[field] == expected:
                        timings[field] = round(time.monotonic() - started, 2)
                        print(field, timings[field], 'seconds', flush=True)
                        return
                time.sleep(0.25)
            raise RuntimeError(f'{vm}: {field} not observed within 180 seconds')

        operation('create')
        assert execute('test ! -e /tmp/ahvm-base-proof && printf isolated-base-proof > /tmp/ahvm-base-proof && sync && cat /tmp/ahvm-base-proof') == 'isolated-base-proof'
        operation('stop')
        assert call('POST', prefix + '/storage/sync')[0] == 200
        wait_record('evicted')
        operation('start')
        assert execute('cat /tmp/ahvm-base-proof') == 'isolated-base-proof'
        operation('delete')
        wait_record('reclaimed')
        assert inventory() == []
        print('PASS cycle', cycle + 1, timings, flush=True)


if __name__ == '__main__':
    main()
