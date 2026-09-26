#!/usr/bin/env python3
"""One disposable replicated VM: detached job, daemon adoption, idle stop, disk.

Use an isolated daemon and volume service. AHVM_TEST_API and
AHVM_TEST_TOKEN_FILE are required. --restart-command must restart ONLY that
isolated daemon while leaving its workers alive (e.g. systemd KillMode=process).
No models, provider credentials or production data are used.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import time
import urllib.error
import urllib.request
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--restart-command', required=True)
    args = parser.parse_args()
    api = os.environ['AHVM_TEST_API'].rstrip('/')
    token = Path(os.environ['AHVM_TEST_TOKEN_FILE']).read_text().strip()
    name = 'agent-gate-' + uuid.uuid4().hex[:12]

    def call(method, path, body=None, expected=200):
        request = urllib.request.Request(api + path, method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
        try:
            response = urllib.request.urlopen(request, timeout=360)
        except urllib.error.HTTPError as error:
            response = error
        data = response.read()
        if response.status != expected:
            raise AssertionError((method, path, response.status, data[:1000]))
        return json.loads(data) if data else None

    def wait_run(run, limit=180):
        until = time.monotonic() + limit
        while time.monotonic() < until:
            result = call('GET', '/v1/admin/runs/' + run)
            if result['finished_at'] is not None:
                return result
            time.sleep(1)
        raise AssertionError('run did not finish')

    created = False
    run = name + '-job'
    try:
        result = call('POST', '/v1/sandboxes', {
            'name': name, 'cpus': 1, 'memory_mb': 2048,
            'storage_mode': 'replicated'}, expected=201)
        created = True
        assert result['storage']['mode'] == 'replicated'
        prefix = '/v1/sandboxes/' + name
        request = {'sandbox_id': name, 'argv': ['/bin/sh', '-c',
            'printf once >> /workspace/managed-count; sleep 85; '
            'head -c 524288 /dev/zero; printf PERSISTED > /workspace/managed-result; sync'],
            'max_runtime_secs': 240}
        first = call('POST', '/v1/admin/runs/' + run, request)
        retry = call('POST', '/v1/admin/runs/' + run, request)
        assert retry['id'] == first['id']
        changed = dict(request, argv=['/bin/false'])
        call('POST', '/v1/admin/runs/' + run, changed, expected=409)
        call('POST', prefix + '/stop', {}, expected=409)
        call('DELETE', prefix, expected=409)
        print('PASS admission, exact retry, lifecycle conflicts; HTTP caller detached', flush=True)
        time.sleep(36)
        assert call('GET', prefix)['state'] == 'running'
        before = call('GET', '/v1/admin/runs/' + run)
        assert before['phase'] == 'running' and before['session_id']
        subprocess.run(shlex.split(args.restart_command), check=True)
        until = time.monotonic() + 30
        while True:
            try:
                after = call('GET', '/v1/admin/runs/' + run)
                break
            except (urllib.error.URLError, ConnectionError):
                if time.monotonic() > until:
                    raise
                time.sleep(.5)
        assert after['epoch'] > before['epoch']
        assert after['session_id'] == before['session_id']
        print('PASS quiet job beyond 30s and daemon restart adoption, same session', flush=True)
        completed = wait_run(run)
        assert completed['phase'] == 'succeeded', completed
        assert completed['exit_code'] == 0
        finish = time.monotonic()
        print('PASS completed with >256-KiB output; waiting actual five-minute idle stop', flush=True)
        # Status is observation only; no guest calls while measuring idle.
        while time.monotonic() - finish < 325:
            info = call('GET', prefix)
            elapsed = time.monotonic() - finish
            if info['state'] == 'stopped':
                assert elapsed >= 295, elapsed
                print('PASS idle stop after %.1fs, VM record retained' % elapsed, flush=True)
                break
            time.sleep(2)
        else:
            raise AssertionError('idle stop did not happen')
        call('POST', prefix + '/start', {})
        output = call('POST', prefix + '/exec', {'argv': ['/bin/sh', '-c',
            'cat /workspace/managed-count; printf "\\n"; cat /workspace/managed-result']})
        assert output['stdout'] == 'once\nPERSISTED', output
        print('PASS cold wake preserves disk and job ran exactly once', flush=True)
        cancel_run = name + '-cancel'
        call('POST', '/v1/admin/runs/' + cancel_run, {
            'sandbox_id': name, 'argv': ['/bin/sleep', '120'], 'max_runtime_secs': 60})
        time.sleep(3)
        call('POST', '/v1/admin/runs/' + cancel_run + '/cancel', {})
        assert wait_run(cancel_run)['phase'] == 'interrupted'
        print('PASS cancellation confirms exit', flush=True)
    finally:
        if created:
            # Leave records intact on an unresolved job. Never force-delete work
            # or mask the gate failure with a broad host cleanup.
            call('DELETE', '/v1/sandboxes/' + name, expected=204)
            print('Deleted disposable VM', flush=True)


if __name__ == '__main__':
    main()
