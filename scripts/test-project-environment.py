#!/usr/bin/env python3
"""Guest-only gate: run as ahvm in an empty disposable Ubuntu VM.

Stage the root-owned helper first. Run `init HELPER` then sync/stop, discard only
this gate VM's RAM bundle, cold-start, and run `reopen HELPER`. No provider/model
requests are made. All files and process scopes are under this gate's checkout.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid

ROOT = Path('/workspace/project-environment-gate')
HELPER = sys.argv[2]
PHASE = sys.argv[1]
RUNS = []


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else 'none'


def identity(path, revision=None):
    return [uuid.uuid4().hex, hashlib.sha256(str(path).encode()).hexdigest(),
            revision or hashlib.sha256((str(path) + ':revision1').encode()).hexdigest()]


def argv(operation, path, ids, *extra):
    return ['sudo', '-n', '/usr/bin/python3', '-I', HELPER, operation, *ids,
            '--cwd', str(path), *extra]


def invoke(operation, path, ids, *extra, check=True, timeout=20):
    process = subprocess.run(argv(operation, path, ids, *extra), capture_output=True, timeout=timeout)
    if check:
        assert process.returncode == 0, (operation, process.returncode, process.stdout[-2048:], process.stderr)
    return process


def result(path, ids, hook):
    return json.loads(invoke('result', path, ids, '--hook', hook).stdout)


def prepare(path, ids, hook, check=True, timeout=20, hook_timeout=15):
    return invoke(hook, path, ids, '--hook-sha256', digest(path / '.ahvm' / (hook + '.sh')),
                  '--timeout-secs', str(hook_timeout), check=check, timeout=timeout)


def alive(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[0] != 'Z'
    except FileNotFoundError:
        return False


def wait(condition, seconds=5):
    deadline = time.monotonic() + seconds
    while not condition():
        assert time.monotonic() < deadline, 'guest gate deadline'
        time.sleep(.02)


def checkout(name, install=None, start=None):
    path = ROOT / name
    path.mkdir()
    (path / '.ahvm').mkdir()
    for hook, script in [('install', install), ('start', start)]:
        if script is not None:
            (path / '.ahvm' / (hook + '.sh')).write_text(script)
    return path


SERVICE = '''
set -eu
printf start >> start-count
rm -f service.pid
python3 -u -c 'import os,time; os.setsid(); from pathlib import Path; Path("service.pid").write_text(str(os.getpid()));\nwhile True: print("SERVICE OUTPUT",flush=True); time.sleep(.01)' &
while [ ! -f service.pid ]; do sleep .01; done
printf 'Services ready\n'
'''
INSTALL = '''
set -eu
[ "$(id -un)" = ahvm ]
[ "$PWD" = "$AHVM_WORKSPACE_PATH" ]
[ "${AHVM_FAKE_PROVIDER_TOKEN:-}" = "" ]
sudo -n true
printf install >> install-count
printf 'Installed arbitrary project dependencies\n'
'''


def start_async(path, ids, hook='install'):
    RUNS.append((path, ids))
    return subprocess.Popen(argv(hook, path, ids, '--hook-sha256', digest(path / '.ahvm' / (hook + '.sh')),
                                 '--timeout-secs', '30'), stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def gate():
    assert PHASE in ('init', 'reopen') and os.getuid() != 0
    probe = subprocess.run(['sudo', '-n', '/usr/bin/python3', '-I', HELPER, 'probe'],
                           capture_output=True, check=True, timeout=10)
    assert json.loads(probe.stdout)['protocol'] == 'project-environment-v1'
    boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
    if PHASE == 'reopen':
        old = json.loads((ROOT / 'cold.json').read_text())
        assert old['boot'] != boot, 'gate requires a real cold boot'
        path = ROOT / 'cold'
        ids = identity(path, old['revision'])
        prepare(path, ids, 'install')
        assert result(path, ids, 'install')['reused'] is True
        assert (path / 'install-count').read_text() == 'install'
        prepare(path, ids, 'start')
        assert result(path, ids, 'start')['reused'] is False
        assert (path / 'start-count').read_text() == 'startstart'
        invoke('abort', path, ids)
        print(json.dumps({'phase': PHASE, 'install_preserved': True, 'start_replayed': True,
                          'new_boot': boot}))
        return
    ROOT.mkdir()
    empty = checkout('empty')
    ids = identity(empty)
    for hook in ('install', 'start'):
        prepare(empty, ids, hook)
        assert result(empty, ids, hook)['hook_present'] is False
    primary = checkout('primary', INSTALL, SERVICE)
    ids = identity(primary)
    RUNS.append((primary, ids))
    prepare(primary, ids, 'install')
    started = time.monotonic()
    prepare(primary, ids, 'start', timeout=10)
    assert time.monotonic() - started < 5, 'background stdout kept the helper alive'
    service_pid = int((primary / 'service.pid').read_text())
    assert alive(service_pid)
    another = identity(primary, ids[2])
    for hook in ('install', 'start'):
        prepare(primary, another, hook)
        assert result(primary, another, hook)['reused'] is True
    assert (primary / 'install-count').read_text() == 'install'
    assert (primary / 'start-count').read_text() == 'start'
    invoke('abort', primary, another)
    assert alive(service_pid), 'reused service owned by a previous run was killed'
    sibling = checkout('sibling', None, SERVICE)
    sibling_ids = identity(sibling)
    RUNS.append((sibling, sibling_ids))
    prepare(sibling, sibling_ids, 'start')
    sibling_pid = int((sibling / 'service.pid').read_text())
    invoke('abort', primary, ids)
    wait(lambda: not alive(service_pid))
    assert alive(sibling_pid), 'unrelated checkout service did not survive'

    replacement = checkout('replacement', None, SERVICE)
    original_ids = identity(replacement)
    RUNS.append((replacement, original_ids))
    prepare(replacement, original_ids, 'start')
    old_pid = int((replacement / 'service.pid').read_text())
    (replacement / '.ahvm/start.sh').write_text(SERVICE + '\nprintf replaced > replaced\n')
    replacement_ids = identity(replacement, hashlib.sha256(b'replaced-hook').hexdigest())
    RUNS.append((replacement, replacement_ids))
    replace_args = ('--previous-revision', original_ids[2])
    assert json.loads(invoke('replace', replacement, replacement_ids, *replace_args).stdout)['replaced'] is True
    assert not alive(old_pid) and alive(sibling_pid)
    assert json.loads(invoke('replace', replacement, replacement_ids, *replace_args).stdout)['replaced'] is False
    prepare(replacement, replacement_ids, 'start')
    assert (replacement / 'replaced').read_text() == 'replaced'
    assert (replacement / 'start-count').read_text() == 'startstart'
    failed_revision = identity(replacement, hashlib.sha256(b'failed-replacement').hexdigest())
    invoke('replace', replacement, failed_revision, '--previous-revision', replacement_ids[2])
    (replacement / '.ahvm/start.sh').write_text(SERVICE + '\nexit 23\n')
    assert prepare(replacement, failed_revision, 'start', check=False).returncode == 23
    assert result(replacement, failed_revision, 'start')['empty'] is True
    retry = identity(replacement, failed_revision[2])
    RUNS.append((replacement, retry))
    (replacement / '.ahvm/start.sh').write_text(SERVICE)
    prepare(replacement, retry, 'start')
    assert result(replacement, retry, 'start')['services_running'] is True

    # Same path, changed source/hook/base revision invalidates installation.
    changed = identity(primary, hashlib.sha256(b'revision2').hexdigest())
    prepare(primary, changed, 'install')
    assert result(primary, changed, 'install')['reused'] is False
    assert (primary / 'install-count').read_text() == 'installinstall'
    assert invoke('install', primary, identity(primary), '--hook-sha256', 'f' * 64,
                  check=False).returncode != 0
    symlink = ROOT / 'symlink'
    symlink.symlink_to(primary, target_is_directory=True)
    assert prepare(symlink, identity(symlink), 'install', check=False).returncode != 0
    bad = checkout('symlink-hook')
    (bad / '.ahvm' / 'install.sh').symlink_to(primary / '.ahvm' / 'install.sh')
    assert prepare(bad, identity(bad), 'install', check=False).returncode != 0
    bad_directory = ROOT / 'symlink-directory'
    bad_directory.mkdir()
    (bad_directory / '.ahvm').symlink_to(primary / '.ahvm', target_is_directory=True)
    assert prepare(bad_directory, identity(bad_directory), 'install', check=False).returncode != 0

    failure = checkout('failed-start', None, SERVICE + '\nexit 23\n')
    failure_ids = identity(failure)
    failed = prepare(failure, failure_ids, 'start', check=False)
    assert failed.returncode == 23
    proof = result(failure, failure_ids, 'start')
    assert proof['exit_code'] == 23 and proof['empty'] is True
    wait(lambda: not alive(int((failure / 'service.pid').read_text())))
    timeout = checkout('timeout-start', None, SERVICE + '\nsleep 30\n')
    timeout_ids = identity(timeout)
    assert prepare(timeout, timeout_ids, 'start', check=False, hook_timeout=1).returncode == 124
    assert result(timeout, timeout_ids, 'start')['empty'] is True
    assert not alive(int((timeout / 'service.pid').read_text()))

    # A host marker aborts work even when the managed controller is absent.
    cancelled = checkout('host-cancel', SERVICE + '\nsleep 30\n')
    cancel_ids = identity(cancelled)
    process = start_async(cancelled, cancel_ids)
    wait(lambda: (cancelled / 'service.pid').exists())
    detached_pid = int((cancelled / 'service.pid').read_text())
    host_id = str(uuid.UUID(hex=cancel_ids[0]))
    subprocess.run(['sudo', '-n', '/usr/bin/python3', '-c',
                    'from pathlib import Path; p=Path("/run/ahvm-managed-cancel")/"' + host_id + '"; p.write_text("cancel\\n");p.chmod(0o644)'],
                   check=True, timeout=5)
    process.communicate(timeout=10)
    assert process.returncode == 130
    proof = result(cancelled, cancel_ids, 'install')
    assert proof['cancelled'] is True and proof['empty'] is True
    assert not alive(detached_pid)
    assert alive(sibling_pid)

    locked = checkout('install-owner', 'printf ready > leader-ready\nsleep 30\n')
    locked_ids = identity(locked)
    owner = start_async(locked, locked_ids)
    wait(lambda: (locked / 'leader-ready').exists())
    waiting = checkout('install-waiter', INSTALL)
    waiting_ids = identity(waiting)
    waiter = start_async(waiting, waiting_ids)
    def waiting_lock():
        out = invoke('status', waiting, waiting_ids, '--hook', 'install', check=False)
        return out.returncode == 0 and json.loads(out.stdout).get('state') == 'waiting_install_lock'
    wait(waiting_lock)
    invoke('abort', waiting, waiting_ids)
    waiter.communicate(timeout=10)
    assert waiter.returncode == 130 and not (waiting / 'install-count').exists()
    assert owner.poll() is None, 'cancelling a lock waiter stopped the lock owner'
    invoke('abort', locked, locked_ids)
    owner.communicate(timeout=10)
    assert owner.returncode == 130

    # Sudo remains usable for real package installation, without host secrets.
    package = checkout('package', 'set -eu\nsudo -n sed -i s,http://,https://,g /etc/apt/sources.list.d/ubuntu.sources\nsudo -n apt-get update -qq\nsudo -n apt-get install -y -qq --no-install-recommends tree\ntree --version\n')
    package_ids = identity(package)
    prepare(package, package_ids, 'install', timeout=180, hook_timeout=120)
    assert result(package, package_ids, 'install')['exit_code'] == 0

    large = checkout('large-output', 'python3 -c "print(\'🦀\'*400000)"\n')
    large_ids = identity(large)
    out = prepare(large, large_ids, 'install')
    assert len(out.stdout) <= 1024 * 1024 + 16384 + 100
    assert result(large, large_ids, 'install')['output_truncated'] is True
    cold = checkout('cold', INSTALL, SERVICE)
    cold_ids = identity(cold)
    RUNS.append((cold, cold_ids))
    prepare(cold, cold_ids, 'install')
    prepare(cold, cold_ids, 'start')
    (ROOT / 'cold.json').write_text(json.dumps({'boot': boot, 'revision': cold_ids[2]}))
    print(json.dumps({'phase': PHASE, 'repeat_install_and_start': True, 'new_revision_installs': True,
        'no_op': True, 'background_stdout_finite': True, 'sibling_isolation': True,
        'hook_sha256_and_path_safety': True, 'failed_start_joined': True, 'timeout_joined': True,
        'host_marker_cancel_joined': True, 'install_lock_waiter_cancelled': True,
        'sudo_package_installation': True, 'output_bounded': True, 'changed_hook_replacement_and_retry': True, 'boot': boot}))


try:
    gate()
finally:
    for path, ids in RUNS:
        if path.name != 'cold':
            invoke('abort', path, ids, check=False)
