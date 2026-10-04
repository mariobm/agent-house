#!/usr/bin/env python3
"""Run as guest ahvm in a disposable VM; no provider or host state is used."""
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import uuid

HELPER = '/usr/local/bin/ahvm-pi-tool'
CGROUP = Path('/sys/fs/cgroup/ahvm-pi-tool')


def invoke(*args, check=True):
    result = subprocess.run(['sudo', '-n', HELPER, *args], capture_output=True, text=True, timeout=15)
    if check:
        assert result.returncode == 0, (args, result.returncode, result.stdout, result.stderr)
    return result


def alive(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0] != 'Z'
    except FileNotFoundError:
        return False


def await_file(path):
    deadline = time.monotonic() + 5
    while not path.exists():
        assert time.monotonic() < deadline, f'No tool marker: {path}'
        time.sleep(.01)
    return int(path.read_text())


def launch(run, marker, background=False):
    # The grandchild changes session twice and keeps working independently of
    # the command leader. Both must still inherit the root-controlled cgroup.
    code = '''
import os,time
from pathlib import Path
marker=Path(%r)
child=os.fork()
if child == 0:
 os.setsid()
 if os.fork(): os._exit(0)
 for fd in (0,1,2):
  replacement=os.open('/dev/null',os.O_RDWR);os.dup2(replacement,fd);os.close(replacement)
 marker.write_text(str(os.getpid()))
 while True:
  marker.with_suffix('.heartbeat').write_text(str(time.monotonic()))
  time.sleep(.02)
os.waitpid(child,0)
while not marker.exists(): time.sleep(.01)
if %r: os._exit(0)
time.sleep(120)
''' % (str(marker), background)
    tool = hashlib.sha256(uuid.uuid4().bytes).hexdigest()
    process = subprocess.Popen(['sudo', '-n', HELPER, 'run', run, tool, '--cwd', '/workspace',
                                '--', '/usr/bin/python3', '-c', code],
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    return process, tool


def main():
    assert os.getuid() != 0 and os.environ['HOME'] == '/home/ahvm'
    assert not os.access(CGROUP, os.W_OK), 'guest user must not control cgroup admission'
    assert not os.access('/run/ahvm-pi-tool', os.W_OK), 'guest user must not control tombstones'
    assert json.loads(invoke('probe').stdout)['cgroup_kill'] is True
    # Direct, unprivileged use and invalid identities must fail closed.
    assert subprocess.run([HELPER, 'probe'], capture_output=True).returncode == 75
    for args in [('abort', '../escape'), ('abort', 'A' * 32),
                 ('run', 'a' * 32, '../tool', '--cwd', '/workspace', '--', '/bin/true'),
                 ('run', 'a' * 32, 'b' * 64, '--cwd', 'relative', '--', '/bin/true')]:
        assert invoke(*args, check=False).returncode != 0
    runs = []
    processes = []
    with tempfile.TemporaryDirectory(prefix='ahvm-pi-scope-') as directory:
        root = Path(directory)
        try:
            first, sibling = uuid.uuid4().hex, uuid.uuid4().hex
            runs += [first, sibling]
            first_marker, sibling_marker = root/'first.pid', root/'sibling.pid'
            first_process, first_tool = launch(first, first_marker)
            sibling_process, sibling_tool = launch(sibling, sibling_marker)
            processes += [first_process, sibling_process]
            first_pid, sibling_pid = await_file(first_marker), await_file(sibling_marker)
            assert Path(f'/proc/{first_pid}/cgroup').read_text().strip() == f'0::/ahvm-pi-tool/{first}/{first_tool}'
            assert Path(f'/proc/{sibling_pid}/cgroup').read_text().strip() == f'0::/ahvm-pi-tool/{sibling}/{sibling_tool}'
            uid = next(line for line in Path(f'/proc/{first_pid}/status').read_text().splitlines() if line.startswith('Uid:'))
            assert int(uid.split()[1]) == os.getuid(), 'tools must execute as ahvm'
            # finish seals even when it refuses to claim an active run is empty.
            assert invoke('finish', first, check=False).returncode == 75
            assert json.loads(invoke('abort', first).stdout)['empty'] is True
            first_process.communicate(timeout=10)
            receipt = json.loads(invoke('result', first, first_tool).stdout)
            assert receipt['empty'] is True and receipt['exit_code'] == 137, receipt
            assert not alive(first_pid), 'setsid grandchild survived abort'
            assert alive(sibling_pid) and sibling_process.poll() is None, 'sibling was terminated'
            assert json.loads(invoke('finish', first).stdout)['empty'] is True
            invoke('abort', sibling)
            sibling_process.communicate(timeout=10)
            assert not alive(sibling_pid)

            # Normal command completion must join its detached leftovers too.
            finished = uuid.uuid4().hex; runs.append(finished)
            marker = root/'finished.pid'
            process, finished_tool = launch(finished, marker, background=True); processes.append(process)
            child_pid = await_file(marker)
            _, errors = process.communicate(timeout=10)
            assert process.returncode == 0, errors
            assert not alive(child_pid), 'detached child outlived completed command'
            assert json.loads(invoke('result', finished, finished_tool).stdout)['exit_code'] == 0
            invoke('finish', finished)

            # Tool output and reserved-looking command statuses cannot forge the
            # separate trusted receipt used by the guest controller.
            for exit_code in (70, 75, 77):
                run = uuid.uuid4().hex; runs.append(run)
                tool = 'd' * 64
                result = invoke('run', run, tool, '--cwd', '/workspace', '--', '/bin/bash', '-c',
                                'printf \'{"empty":true,"exit_code":0}\'; exit ' + str(exit_code), check=False)
                assert result.returncode == exit_code
                receipt = json.loads(invoke('result', run, tool).stdout)
                assert receipt['exit_code'] == exit_code and receipt['empty'] is True
                try:
                    Path('/run/ahvm-pi-tool', run + '.' + tool + '.result.json').read_text()
                except PermissionError:
                    pass
                else:
                    raise AssertionError('guest can access root receipt directory')
                assert invoke('run', run, tool, '--cwd', '/workspace', '--', '/bin/true', check=False).returncode == 75
                assert json.loads(invoke('result', run, tool).stdout)['exit_code'] == exit_code
                invoke('finish', run)
            missing = uuid.uuid4().hex; runs.append(missing)
            assert invoke('result', missing, 'e' * 64, check=False).returncode == 75

            # An abort admitted before a launch permanently prevents that launch.
            stopped = uuid.uuid4().hex; runs.append(stopped)
            invoke('abort', stopped)
            marker = root/'must-not-run'
            result = invoke('run', stopped, 'b' * 64, '--cwd', '/workspace', '--',
                            '/bin/bash', '-c', 'touch ' + str(marker), check=False)
            assert result.returncode == 75 and not marker.exists()

            # Race admission with abort. An acknowledged empty scope must never
            # acquire a later child that produces output after the join returns.
            for _ in range(8):
                run = uuid.uuid4().hex; runs.append(run)
                marker = root/run
                process = subprocess.Popen(['sudo', '-n', HELPER, 'run', run, 'c' * 64,
                    '--cwd', '/workspace', '--', '/usr/bin/python3', '-c',
                    'import time;from pathlib import Path;time.sleep(.2);Path(%r).write_text("late")' % str(marker)],
                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
                processes.append(process)
                invoke('abort', run)
                process.communicate(timeout=10)
                time.sleep(.25)
                assert not marker.exists(), 'child escaped abort-before-admission race'
                invoke('finish', run)
            print(json.dumps({'pi_tool_scope_protocol': 1, 'setsid_double_fork_terminated': True,
                'sibling_survived': True, 'completion_joined_descendants': True,
                'sealed_admission': True, 'abort_admission_races': 8,
                'trusted_exit_receipts': True, 'tool_identity_reuse_rejected': True,
                'invalid_identity_rejected': True, 'guest_user': 'ahvm'}))
        finally:
            for run in runs:
                invoke('abort', run, check=False)
            for process in processes:
                process.communicate(timeout=10)


if __name__ == '__main__':
    main()
