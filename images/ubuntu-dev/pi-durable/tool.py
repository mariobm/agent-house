#!/usr/bin/python3 -I
"""Root-owned Pi tool scopes on guest cgroup v2; no systemd is required.

run RUN32 TOOL64 --cwd PATH -- COMMAND [ARG...]
abort RUN32 / finish RUN32: seal admission before checking/joining descendants.
result RUN32 TOOL64: root-owned exit receipt, available only after an empty join.
Only the child enters its tool cgroup; this supervisor stays outside it.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import pwd
import re
import select
import signal
import stat
import sys
import time

STATE = Path('/run/ahvm-pi-tool')
CGROUP = Path('/sys/fs/cgroup/ahvm-pi-tool')
JOIN_SECONDS = 5


def protected_directory(path):
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
        raise RuntimeError(f'Unprotected scope directory: {path}')


def available():
    if os.geteuid() != 0:
        raise RuntimeError('Run this helper through sudo as guest root')
    protected_directory(STATE)
    protected_directory(CGROUP)
    mounts = [line.split(' - ', 1) for line in Path('/proc/self/mountinfo').read_text().splitlines()]
    if not any(before.split()[4] == str(STATE) and after.split()[0] == 'tmpfs' for before, after in mounts):
        raise RuntimeError('Tool receipts must use the per-boot private tmpfs')
    if not any(before.split()[4] == '/sys/fs/cgroup' and after.split()[0] == 'cgroup2' for before, after in mounts):
        raise RuntimeError('Guest cgroup v2 must be mounted')
    if not (CGROUP / 'cgroup.kill').is_file():
        raise RuntimeError('Guest cgroup v2 with cgroup.kill is required')


def lock(run):
    fd = os.open(STATE / (run + '.lock'), os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o077:
        os.close(fd)
        raise RuntimeError('Unprotected scope lock')
    fcntl.flock(fd, fcntl.LOCK_EX)
    return fd


def seal(run):
    try:
        fd = os.open(STATE / (run + '.stopped'), os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    except FileExistsError:
        return
    os.close(fd)


def populated(path):
    try:
        fields = dict(line.split() for line in (path / 'cgroup.events').read_text().splitlines())
    except FileNotFoundError:
        return False
    if fields.get('populated') not in ('0', '1'):
        raise RuntimeError('Missing cgroup populated state')
    return fields['populated'] == '1'


def kill_and_join(path):
    try:
        (path / 'cgroup.kill').write_text('1')
    except FileNotFoundError:
        if path.exists():
            raise RuntimeError('Scope has no cgroup.kill')
        return
    deadline = time.monotonic() + JOIN_SECONDS
    while populated(path):
        if time.monotonic() >= deadline:
            raise RuntimeError('Tool descendants did not terminate before the join deadline')
        time.sleep(0.01)


def remove_empty(path):
    # Only the two fixed levels created by this helper may be removed.
    try:
        for child in path.iterdir():
            if child.is_dir():
                child.rmdir()
        path.rmdir()
    except FileNotFoundError:
        pass


def close_run(run, abort):
    fd = lock(run)
    try:
        seal(run)  # Late launches cannot race past successful termination.
        scope = CGROUP / run
        if abort:
            kill_and_join(scope)
        elif populated(scope):
            raise RuntimeError('Run still contains live tools; use abort to terminate them')
        remove_empty(scope)
        return {'run': run, 'sealed': True, 'empty': True, 'operation': 'abort' if abort else 'finish'}
    finally:
        os.close(fd)


def result_path(run, tool):
    return STATE / (run + '.' + tool + '.result.json')


def record_result(run, tool, code):
    fd = lock(run)
    try:
        if populated(CGROUP / run / tool):
            raise RuntimeError('Cannot record a result while tool descendants are live')
        path = result_path(run, tool)
        receipt = {'schema': 1, 'run_id': run, 'tool_id': tool, 'exit_code': code, 'empty': True}
        # The root-only parent and exclusive identity prevent command output,
        # symlinks, or a later launch from forging/replacing this receipt.
        receipt_fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        with os.fdopen(receipt_fd, 'w') as file:
            json.dump(receipt, file)
    finally:
        os.close(fd)


def query_result(run, tool):
    fd = lock(run)
    try:
        receipt_fd = os.open(result_path(run, tool), os.O_RDONLY | os.O_NOFOLLOW)
        info = os.fstat(receipt_fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o077:
            os.close(receipt_fd)
            raise RuntimeError('Unprotected tool receipt')
        with os.fdopen(receipt_fd) as file:
            receipt = json.load(file)
        if (receipt.get('schema') != 1 or receipt.get('run_id') != run or receipt.get('tool_id') != tool
                or receipt.get('empty') is not True or type(receipt.get('exit_code')) is not int
                or not 0 <= receipt['exit_code'] <= 255 or populated(CGROUP / run / tool)):
            raise RuntimeError('Tool receipt does not prove an empty scope')
        return receipt
    finally:
        os.close(fd)


def run_tool(run, tool, cwd, command):
    if not Path(cwd).is_absolute() or not command:
        raise ValueError('An absolute --cwd and a command are required')
    user = pwd.getpwnam('ahvm')
    interrupted = []
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, lambda number, _frame: interrupted.append(number))
    scope = CGROUP / run / tool
    fd = lock(run)
    pid = None
    try:
        if (STATE / (run + '.stopped')).exists():
            raise RuntimeError('Run admission is sealed')
        started = STATE / (run + '.' + tool + '.started')
        started_fd = os.open(started, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        os.close(started_fd)  # A tool identity can never acquire a second process tree.
        (CGROUP / run).mkdir(mode=0o755, exist_ok=True)
        protected_directory(CGROUP / run)
        scope.mkdir(mode=0o755)  # Refuse tool identity reuse.
        ready_read, ready_write = os.pipe2(os.O_CLOEXEC)
        pid = os.fork()
        if pid == 0:
            try:
                os.close(ready_read)
                os.close(fd)
                (scope / 'cgroup.procs').write_text(str(os.getpid()))
                os.setsid()
                os.initgroups(user.pw_name, user.pw_gid)
                os.setgid(user.pw_gid)
                os.setuid(user.pw_uid)
                os.chdir(cwd)  # Resolve guest-controlled paths only after dropping privileges.
                env = {key: value for key, value in os.environ.items() if not key.startswith('SUDO_')}
                env.update(HOME=user.pw_dir, USER=user.pw_name, LOGNAME=user.pw_name,
                           PATH='/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin')
                os.write(ready_write, b'1')
                os.close(ready_write)
                for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
                    signal.signal(sig, signal.SIG_DFL)
                os.execvpe(command[0], command, env)
            except BaseException as error:
                print(f'Pi tool child failed: {error}', file=sys.stderr, flush=True)
                os._exit(127)
        os.close(ready_write)
        try:
            # Keep admission locked until the child belongs to the cgroup. An
            # abort must never observe empty, return, then let that child exec.
            if not select.select([ready_read], [], [], 5)[0] or os.read(ready_read, 1) != b'1':
                raise RuntimeError('Tool launch did not acknowledge cgroup membership')
        finally:
            os.close(ready_read)
    except BaseException:
        if pid is not None:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            kill_and_join(scope)
            os.waitpid(pid, 0)
        raise
    finally:
        os.close(fd)
    try:
        while True:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                break
            if interrupted:
                close_run(run, abort=True)
            time.sleep(0.01)
        # Detached/background descendants cannot outlive a completed bash tool.
        kill_and_join(scope)
        code = os.waitstatus_to_exitcode(status) if os.WIFEXITED(status) else 128 + os.WTERMSIG(status)
        record_result(run, tool, code)
        return code
    finally:
        # If a supervisor is forcibly killed, the controller must still issue
        # abort separately. cgroup membership survives its supervisor's death.
        kill_and_join(scope)
        try:
            scope.rmdir()
        except FileNotFoundError:
            pass


def main():
    argv = sys.argv[1:]
    command = []
    if '--' in argv:
        boundary = argv.index('--')
        command, argv = argv[boundary + 1:], argv[:boundary]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=['probe', 'run', 'abort', 'finish', 'check', 'result'])
    parser.add_argument('run', nargs='?')
    parser.add_argument('tool', nargs='?')
    parser.add_argument('--cwd')
    args = parser.parse_args(argv)
    available()
    if args.operation == 'probe':
        if args.run or args.tool or command or args.cwd:
            parser.error('probe accepts no arguments')
        print(json.dumps({'protocol': 1, 'cgroup_v2': True, 'cgroup_kill': True, 'guest_user': 'ahvm'}))
        return 0
    if not args.run or not re.fullmatch('[0-9a-f]{32}', args.run):
        parser.error('run ID must be 32 lowercase hex characters')
    if args.operation == 'result':
        if not args.tool or not re.fullmatch('[0-9a-f]{64}', args.tool) or command or args.cwd:
            parser.error('result requires only a run ID and 64 lowercase hex tool ID')
        print(json.dumps(query_result(args.run, args.tool)))
        return 0
    if args.operation == 'run':
        if not args.tool or not re.fullmatch('[0-9a-f]{64}', args.tool):
            parser.error('tool ID must be 64 lowercase hex characters')
        return run_tool(args.run, args.tool, args.cwd or '', command)
    if args.tool or command or args.cwd:
        parser.error('abort/finish/check accept only a run ID')
    print(json.dumps(close_run(args.run, abort=args.operation == 'abort')))
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, RuntimeError, ValueError) as error:
        print(f'Pi tool scope failed: {error}', file=sys.stderr)
        sys.exit(75)
