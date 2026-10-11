#!/usr/bin/python3 -I
"""Finite project hooks and per-checkout services on guest cgroup v2.

install/start RUN32 WORKSPACE64 REVISION64 --cwd PATH --hook-sha256 HASH|none
result/status RUN32 WORKSPACE64 REVISION64 --cwd PATH --hook install|start
abort RUN32 WORKSPACE64 REVISION64 --cwd PATH
replace RUN32 WORKSPACE64 NEWREV64 --cwd PATH --previous-revision OLDREV64

Install success persists across cold boots; service readiness is boot-local.
Root receipts and independent abort, rather than the supervisor's exit, prove
termination. The guest ahvm account retains its existing passwordless sudo.
"""
import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pwd
import re
import select
import signal
import stat
import subprocess
import sys
import time

STATE = Path('/run/ahvm-project-environment')
CACHE = Path('/var/lib/ahvm-project-environment')
CGROUP = Path('/sys/fs/cgroup/ahvm-project-environment')
JOIN_SECONDS = 5
OUTPUT_LIMIT = 1024 * 1024
TAIL_LIMIT = 16384


def protected(path, directory=False):
    info = path.lstat()
    kind = stat.S_ISDIR if directory else stat.S_ISREG
    if not kind(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
        raise RuntimeError('Unprotected environment helper path')


def private_directory(path):
    path.mkdir(mode=0o700, exist_ok=True)
    protected(path, directory=True)
    if path.stat().st_mode & 0o077:
        raise RuntimeError('Environment state must be private')


@contextmanager
def lock(path):
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o077:
            raise RuntimeError('Unprotected environment lock')
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield fd
    finally:
        os.close(fd)


def available():
    if os.geteuid() != 0:
        raise RuntimeError('Run the helper through sudo as guest root')
    script = Path(os.path.abspath(__file__))
    protected(script)
    for parent in script.parents:
        protected(parent, directory=True)
    protected(Path('/run'), directory=True)
    protected(Path('/var/lib'), directory=True)
    with lock(Path('/run/ahvm-project-environment-bootstrap.lock')):
        mounts = [line.split(' - ', 1) for line in Path('/proc/self/mountinfo').read_text().splitlines()]
        if not any(a.split()[4] == '/sys/fs/cgroup' and b.split()[0] == 'cgroup2' for a, b in mounts):
            raise RuntimeError('Guest cgroup v2 is required')
        CGROUP.mkdir(mode=0o755, exist_ok=True)
        protected(CGROUP, directory=True)
        if not (CGROUP / 'cgroup.kill').is_file():
            raise RuntimeError('Guest cgroup.kill is required')
        private_directory(STATE)
        mounted = [b.split()[0] for a, b in mounts if a.split()[4] == str(STATE)]
        if mounted and mounted != ['tmpfs']:
            raise RuntimeError('Environment receipts require private per-boot tmpfs')
        if not mounted:
            subprocess.run(['/bin/mount', '-t', 'tmpfs', '-o',
                            'mode=0700,nosuid,nodev,noexec,size=16m', 'tmpfs', str(STATE)],
                           check=True, env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin'})
        private_directory(STATE)
        private_directory(CACHE)


def read_json(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    except FileNotFoundError:
        return None
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o077 or info.st_size > 4096:
            raise RuntimeError('Unprotected environment receipt')
        with os.fdopen(fd) as stream:
            fd = None
            value = json.load(stream)
        if not isinstance(value, dict):
            raise RuntimeError('Invalid environment receipt')
        return value
    finally:
        if fd is not None:
            os.close(fd)


def publish(path, value):
    temporary = path.with_name(path.name + '.' + str(os.getpid()) + '.tmp')
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream, separators=(',', ':'))
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        temporary.unlink(missing_ok=True)


def populated(path):
    try:
        fields = dict(line.split() for line in (path / 'cgroup.events').read_text().splitlines())
    except FileNotFoundError:
        return False
    if fields.get('populated') not in ('0', '1'):
        raise RuntimeError('Missing cgroup population state')
    return fields['populated'] == '1'


def join_scope(path):
    if not path.exists():
        return
    try:
        (path / 'cgroup.kill').write_text('1')
    except FileNotFoundError:
        if path.exists():
            raise RuntimeError('Project scope lost cgroup.kill')
        return
    deadline = time.monotonic() + JOIN_SECONDS
    while populated(path):
        if time.monotonic() >= deadline:
            raise RuntimeError('Project descendants did not terminate')
        time.sleep(.01)
    try:
        path.rmdir()
    except FileNotFoundError:
        pass


def bind(args):
    for value, size in ((args.run, 32), (args.workspace, 64), (args.revision, 64)):
        if not isinstance(value, str) or not re.fullmatch('[0-9a-f]{%d}' % size, value):
            raise ValueError('Invalid project environment identity')
    cwd = args.cwd or ''
    path = Path(cwd)
    if (not path.is_absolute() or str(path) != cwd or '..' in path.parts
            or cwd != os.path.normpath(cwd) or not cwd.startswith('/workspace/')
            or path.resolve(strict=True) != path or not path.is_dir()
            or hashlib.sha256(cwd.encode()).hexdigest() != args.workspace):
        raise ValueError('Expected an exact canonical checkout under /workspace')
    # Refuse symlinked path components, including .ahvm and the script below.
    for parent in [path, *path.parents]:
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError('Checkout path components must be directories')
    boot = Path('/proc/sys/kernel/random/boot_id').read_text().strip()
    if not re.fullmatch('[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}', boot):
        raise RuntimeError('Invalid guest boot identity')
    return {'schema': 1, 'run_id': args.run, 'workspace_id': args.workspace,
            'revision_id': args.revision, 'workspace_path': cwd, 'boot_id': boot}


def matches(value, binding, current_boot=True, same_run=True):
    keys = ['schema', 'workspace_id', 'revision_id', 'workspace_path']
    if current_boot:
        keys.append('boot_id')
    if same_run:
        keys.append('run_id')
    return value is not None and all(value.get(key) == binding[key] for key in keys)


def paths(binding, hook):
    name = binding['run_id'] + '.' + hook
    workspace = binding['workspace_id']
    return (STATE / (name + '.result.json'), STATE / (name + '.started.json'),
            STATE / (name + '.status.json'), CGROUP / workspace / name)


def sealed(binding):
    value = read_json(STATE / (binding['run_id'] + '.cancelled.json'))
    if value is not None and not matches(value, binding):
        raise RuntimeError('Run identity already has another checkout')
    run = binding['run_id']
    host_id = '-'.join((run[:8], run[8:12], run[12:16], run[16:20], run[20:]))
    # The node publishes this root-owned marker independently of the managed
    # controller. Losing that controller must not strand an active hook tree.
    marker = Path('/run/ahvm-managed-cancel') / host_id
    try:
        info = marker.lstat()
    except FileNotFoundError:
        host_cancelled = False
    else:
        protected(marker)
        if info.st_size != 7 or marker.read_bytes() != b'cancel\n':
            raise RuntimeError('Invalid host cancellation marker')
        host_cancelled = True
    return value is not None or host_cancelled


def hook_digest(binding, hook, expected):
    directory = Path(binding['workspace_path']) / '.ahvm'
    try:
        info = directory.lstat()
    except FileNotFoundError:
        actual = None
    else:
        if not stat.S_ISDIR(info.st_mode):
            raise ValueError('.ahvm must be a directory, never a symlink')
        parent = os.open(directory, os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
        try:
            try:
                fd = os.open(hook + '.sh', os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=parent)
            except FileNotFoundError:
                actual = None
            else:
                with os.fdopen(fd, 'rb') as stream:
                    info = os.fstat(stream.fileno())
                    if not stat.S_ISREG(info.st_mode) or info.st_size > 1024 * 1024:
                        raise ValueError('Project hook must be a regular file of at most 1 MiB')
                    actual = hashlib.sha256(stream.read(1024 * 1024 + 1)).hexdigest()
        finally:
            os.close(parent)
    if expected != (actual or 'none'):
        raise ValueError('Project hook changed from its requested content digest')
    return actual


def receipt(binding, hook, digest, code=0, **extra):
    value = dict(binding, hook=hook, hook_present=digest is not None, hook_sha256=digest,
                 exit_code=code, empty=True, services_running=False,
                 owner_run_id=binding['run_id'], reused=False, cancelled=False,
                 timed_out=False, output_truncated=False)
    value.update(extra)
    return value


def query(binding, hook, status=False):
    result, _, progress, scope = paths(binding, hook)
    value = read_json(progress if status else result)
    if not matches(value, binding) or value.get('hook') != hook:
        raise RuntimeError('No matching project environment receipt')
    if not status and (type(value.get('exit_code')) is not int or not 0 <= value['exit_code'] <= 255):
        raise RuntimeError('Invalid project exit receipt')
    if not status and value.get('empty') is True and populated(scope):
        raise RuntimeError('Project receipt contradicts live descendants')
    return value


def abort(binding):
    with lock(STATE / (binding['workspace_id'] + '.lock')):
        publish(STATE / (binding['run_id'] + '.cancelled.json'), binding)
        for hook in ('install', 'start'):
            result, started, progress, scope = paths(binding, hook)
            claim = read_json(started)
            if claim is not None and not matches(claim, binding):
                raise RuntimeError('Run identity already has another checkout')
            join_scope(scope)
            # A cancellation racing a ready service must not leave a reused
            # success pointing at the now-empty scope on the next prepare.
            service = STATE / (binding['workspace_id'] + '.service.json')
            ready = read_json(service)
            if ready is not None and ready.get('owner_run_id') == binding['run_id']:
                service.unlink(missing_ok=True)
            if claim is not None:
                value = receipt(binding, hook, claim['hook_sha256'], 130, cancelled=True)
                publish(result, value)
                publish(progress, dict(value, state='cancelled'))
            active_path = STATE / (binding['workspace_id'] + '.' + hook + '-active.json')
            active = read_json(active_path)
            if active is not None and active.get('run_id') == binding['run_id']:
                active_path.unlink(missing_ok=True)
        return dict(binding, sealed=True, empty=True, operation='abort')


def replace(binding, previous):
    if not re.fullmatch('[0-9a-f]{64}', previous):
        raise ValueError('Expected a previous revision digest')
    with lock(STATE / (binding['workspace_id'] + '.lock')):
        if sealed(binding):
            raise RuntimeError('Environment admission is sealed')
        service = STATE / (binding['workspace_id'] + '.service.json')
        ready = read_json(service)
        if ready is None:
            return dict(binding, operation='replace', previous_revision_id=previous,
                        replaced=False, empty=True)
        for key in ('workspace_id', 'workspace_path', 'boot_id'):
            if ready.get(key) != binding[key]:
                raise RuntimeError('Service receipt does not match this checkout and boot')
        if ready.get('revision_id') == binding['revision_id']:
            return dict(binding, operation='replace', previous_revision_id=previous,
                        replaced=False, empty=True)
        if ready.get('revision_id') != previous:
            raise RuntimeError('Another revision already owns this checkout service')
        join_scope(paths(ready, 'start')[3])
        service.unlink(missing_ok=True)
        return dict(binding, operation='replace', previous_revision_id=previous,
                    replaced=True, replaced_run_id=ready['owner_run_id'], empty=True)


def global_install(binding, interrupted, status, deadline):
    fd = os.open(STATE / 'install.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    waited = False
    while True:
        if interrupted or sealed(binding) or time.monotonic() >= deadline:
            os.close(fd)
            return None
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            active = read_json(STATE / 'install-active.json')
            if active is None or not populated(paths(active, 'install')[3]):
                return fd
            # A lost supervisor releases flock, but its guest work may survive.
            # Retain the slot until independent abort has joined that scope.
            fcntl.flock(fd, fcntl.LOCK_UN)
        except BlockingIOError:
            pass
        if not waited:
            waited = True
            publish(status, dict(binding, hook='install', state='waiting_install_lock'))
            print('Waiting for another project installation to finish', flush=True)
        time.sleep(.05)


def child_environment(binding):
    user = pwd.getpwnam('ahvm')
    os.initgroups(user.pw_name, user.pw_gid)
    os.setgid(user.pw_gid)
    os.setuid(user.pw_uid)
    return {'HOME': user.pw_dir, 'USER': user.pw_name, 'LOGNAME': user.pw_name,
            'PATH': '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin',
            'LANG': 'C.UTF-8', 'AHVM_PROJECT_RUN_ID': binding['run_id'],
            'AHVM_PROJECT_WORKSPACE_ID': binding['workspace_id'],
            'AHVM_PROJECT_REVISION_ID': binding['revision_id'],
            'AHVM_WORKSPACE_PATH': binding['workspace_path']}


def fork_hook(binding, hook, scope, lock_fd):
    output_read, output_write = os.pipe2(os.O_CLOEXEC)
    ready_read, ready_write = os.pipe2(os.O_CLOEXEC)
    pid = os.fork()
    if pid == 0:
        try:
            os.close(lock_fd)
            os.close(output_read)
            os.close(ready_read)
            (scope / 'cgroup.procs').write_text(str(os.getpid()))
            os.setsid()
            env = child_environment(binding)
            os.chdir(binding['workspace_path'])
            null = os.open('/dev/null', os.O_RDONLY)
            os.dup2(null, 0)
            os.dup2(output_write, 1)
            os.dup2(output_write, 2)
            os.close(output_write)
            os.close(null)
            for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
                signal.signal(sig, signal.SIG_DFL)
            os.write(ready_write, b'1')
            os.close(ready_write)
            os.execve('/bin/bash', ['/bin/bash', '.ahvm/' + hook + '.sh'], env)
        except BaseException:
            os._exit(127)
    os.close(output_write)
    os.close(ready_write)
    try:
        if not select.select([ready_read], [], [], 5)[0] or os.read(ready_read, 1) != b'1':
            os.kill(pid, signal.SIGKILL)
            join_scope(scope)
            os.waitpid(pid, 0)
            os.close(output_read)
            raise RuntimeError('Hook did not acknowledge cgroup admission')
    finally:
        os.close(ready_read)
    return pid, output_read


def retain_output_drain(binding, scope, output, lock_fd):
    # Services inherit only the script-output pipe. A detached, unprivileged
    # reader drains future service output into /dev/null, so no inherited
    # controller pipe can delay a finite start receipt or fill a log file.
    ready_read, ready_write = os.pipe2(os.O_CLOEXEC)
    pid = os.fork()
    if pid == 0:
        try:
            os.close(lock_fd)
            os.close(ready_read)
            (scope / 'cgroup.procs').write_text(str(os.getpid()))
            os.setsid()
            child_environment(binding)
            null = os.open('/dev/null', os.O_RDWR)
            for fd in (0, 1, 2):
                os.dup2(null, fd)
            os.close(null)
            os.write(ready_write, b'1')
            os.close(ready_write)
            while os.read(output, 65536):
                pass
            os._exit(0)
        except BaseException:
            os._exit(127)
    os.close(ready_write)
    try:
        if not select.select([ready_read], [], [], 5)[0] or os.read(ready_read, 1) != b'1':
            raise RuntimeError('Service output drainer did not acknowledge admission')
    finally:
        os.close(ready_read)


def execute(binding, hook, digest, timeout, interrupted, progress, scope):
    global_fd = None
    pid = None
    output = None
    truncated = False
    deadline = time.monotonic() + timeout
    try:
        if hook == 'install':
            global_fd = global_install(binding, interrupted, progress, deadline)
            if global_fd is None:
                cancelled = bool(interrupted) or sealed(binding)
                return receipt(binding, hook, digest, 130 if cancelled else 124,
                               cancelled=cancelled, timed_out=not cancelled)
        with lock(STATE / (binding['workspace_id'] + '.lock')) as workspace_fd:
            if interrupted or sealed(binding):
                return receipt(binding, hook, digest, 130, cancelled=True)
            if hook == 'install':
                publish(STATE / 'install-active.json', binding)
            publish(progress, dict(binding, hook=hook, state='running'))
            if digest is None:
                return receipt(binding, hook, digest)
            (CGROUP / binding['workspace_id']).mkdir(mode=0o755, exist_ok=True)
            protected(CGROUP / binding['workspace_id'], directory=True)
            scope.mkdir(mode=0o755)
            pid, output = fork_hook(binding, hook, scope, workspace_fd)
        forwarded = 0
        tail = b''
        timed_out = False
        eof = False

        def forward(chunk):
            nonlocal tail, forwarded, truncated
            tail = (tail + chunk)[-TAIL_LIMIT:]
            shown = chunk[:max(0, OUTPUT_LIMIT - forwarded)]
            if shown:
                sys.stdout.buffer.write(shown)
                sys.stdout.buffer.flush()
                forwarded += len(shown)
            truncated = truncated or len(shown) < len(chunk)

        while True:
            if interrupted or sealed(binding) or time.monotonic() >= deadline:
                timed_out = not interrupted and not sealed(binding)
                join_scope(scope)
            if not eof and select.select([output], [], [], .02)[0]:
                chunk = os.read(output, 65536)
                if chunk:
                    forward(chunk)
                else:
                    eof = True
            if eof:
                time.sleep(.01)
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                pid = None
                code = os.waitstatus_to_exitcode(status) if os.WIFEXITED(status) else 128 + os.WTERMSIG(status)
                break
        # Drain the already-buffered tail after leader exit, with a time/byte
        # bound because successfully retained services may keep writing.
        tail_deadline = time.monotonic() + .1
        drained = 0
        while not eof and time.monotonic() < tail_deadline and drained < OUTPUT_LIMIT:
            if not select.select([output], [], [], 0)[0]:
                break
            chunk = os.read(output, 65536)
            if not chunk:
                break
            drained += len(chunk)
            forward(chunk)
        if truncated:
            sys.stdout.buffer.write(b'\n[Project hook output truncated; final tail]\n' + tail)
            sys.stdout.buffer.flush()
        with lock(STATE / (binding['workspace_id'] + '.lock')) as workspace_fd:
            cancelled = bool(interrupted) or sealed(binding)
            if hook == 'start' and code == 0 and not cancelled and not timed_out:
                retain_output_drain(binding, scope, output, workspace_fd)
                return receipt(binding, hook, digest, empty=not populated(scope),
                               services_running=populated(scope), output_truncated=truncated)
            join_scope(scope)
            return receipt(binding, hook, digest, 124 if timed_out else 130 if cancelled else code,
                           timed_out=timed_out, cancelled=cancelled, output_truncated=truncated)
    finally:
        if output is not None:
            os.close(output)
        if pid is not None:
            join_scope(scope)
            os.waitpid(pid, 0)
        if global_fd is not None:
            os.close(global_fd)


def run_hook(binding, hook, digest, timeout):
    result, started, progress, scope = paths(binding, hook)
    interrupted = []
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, lambda number, _frame: interrupted.append(number))
    with lock(STATE / (binding['workspace_id'] + '.lock')):
        if sealed(binding):
            raise RuntimeError('Environment admission is sealed')
        prior = read_json(result)
        if prior is not None:
            if not matches(prior, binding) or prior.get('hook_sha256') != digest:
                raise RuntimeError('Run identity has another project hook')
            return query(binding, hook)['exit_code']
        if started.exists():
            raise RuntimeError('Hook already started without a confirmed outcome; abort before retrying with a new run')
        publish(started, dict(binding, hook=hook, hook_sha256=digest))
        cached = read_json(CACHE / (binding['workspace_id'] + '.json') if hook == 'install'
                           else STATE / (binding['workspace_id'] + '.service.json'))
        if matches(cached, binding, current_boot=hook == 'start', same_run=False):
            if cached.get('hook_sha256') != digest:
                raise RuntimeError('Cached hook digest differs from this checkout')
            value = dict(cached, **binding, reused=True)
            if hook == 'start':
                live = populated(paths(cached, 'start')[3])
                value.update(empty=not live, services_running=live)
            publish(result, value)
            publish(progress, dict(value, state='succeeded'))
            return 0
        if hook == 'start' and cached is not None and populated(paths(cached, 'start')[3]):
            raise RuntimeError('This checkout already has services for another revision')
        active = read_json(STATE / (binding['workspace_id'] + '.' + hook + '-active.json'))
        if active is not None:
            raise RuntimeError('This checkout has an unconfirmed hook; abort its run first')
        publish(STATE / (binding['workspace_id'] + '.' + hook + '-active.json'), binding)
        publish(progress, dict(binding, hook=hook, state='queued'))
    value = execute(binding, hook, digest, timeout, interrupted, progress, scope)
    with lock(STATE / (binding['workspace_id'] + '.lock')):
        if sealed(binding) and not value['cancelled']:
            join_scope(scope)
            value = receipt(binding, hook, digest, 130, cancelled=True)
        if value['exit_code'] == 0:
            target = CACHE / (binding['workspace_id'] + '.json') if hook == 'install' else STATE / (binding['workspace_id'] + '.service.json')
            publish(target, value)
        publish(result, value)
        publish(progress, dict(value, state='cancelled' if value['cancelled'] else 'succeeded' if value['exit_code'] == 0 else 'failed'))
        (STATE / (binding['workspace_id'] + '.' + hook + '-active.json')).unlink(missing_ok=True)
    return value['exit_code']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['probe', 'install', 'start', 'result', 'status', 'abort', 'replace'])
    parser.add_argument('run', nargs='?')
    parser.add_argument('workspace', nargs='?')
    parser.add_argument('revision', nargs='?')
    parser.add_argument('--cwd')
    parser.add_argument('--hook', choices=['install', 'start'])
    parser.add_argument('--hook-sha256')
    parser.add_argument('--timeout-secs', type=int)
    parser.add_argument('--previous-revision')
    args = parser.parse_args()
    available()
    if args.operation == 'probe':
        if args.run or args.workspace or args.revision or args.cwd or args.hook or args.hook_sha256 or args.timeout_secs or args.previous_revision:
            parser.error('probe accepts no arguments')
        print(json.dumps({'schema': 1, 'protocol': 'project-environment-v1', 'cgroup_v2': True,
                          'cgroup_kill': True, 'guest_user': 'ahvm', 'helper_sha256':
                          hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}))
        return 0
    binding = bind(args)
    if args.operation == 'replace':
        if args.hook or args.hook_sha256 or args.timeout_secs or not args.previous_revision:
            parser.error('replace requires only --previous-revision OLDREV64')
        print(json.dumps(replace(binding, args.previous_revision)))
        return 0
    if args.previous_revision:
        parser.error('--previous-revision is only accepted by replace')
    if args.operation in ('install', 'start'):
        if args.hook or not args.hook_sha256 or not re.fullmatch('none|[0-9a-f]{64}', args.hook_sha256):
            parser.error('install/start require --hook-sha256 HASH64|none')
        timeout = args.timeout_secs if args.timeout_secs is not None else (900 if args.operation == 'install' else 120)
        if not 1 <= timeout <= 1800:
            parser.error('hook timeout must be 1..1800 seconds')
        digest = hook_digest(binding, args.operation, args.hook_sha256)
        return run_hook(binding, args.operation, digest, timeout)
    if args.hook_sha256 or args.timeout_secs:
        parser.error('result/status/abort do not accept hook input')
    if args.operation == 'abort':
        if args.hook:
            parser.error('abort accepts no hook selector')
        value = abort(binding)
    else:
        if not args.hook:
            parser.error('result/status require --hook install|start')
        value = query(binding, args.hook, status=args.operation == 'status')
    print(json.dumps(value))
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f'Project environment helper failed: {error}', file=sys.stderr)
        sys.exit(75)
