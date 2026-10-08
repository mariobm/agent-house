#!/usr/bin/env bash
# Fresh installation only. Does not replace the Go installation or existing data.
# Usage: install-rust.sh BUNDLE [--prefix PATH] [--config-dir PATH] [--data-dir PATH]
#        [--unit-name NAME] [--user USER] [--no-start]
#        [--broker-state-dir PATH] [--worker-id-base NUMBER] [--worker-id-count NUMBER]
set -euo pipefail
umask 077
BUNDLE=${1:?Usage: install-rust.sh BUNDLE [options]}; shift
PREFIX=/opt/ahvm-rust
CONFIG=/etc/ahvm-rust
DATA=/var/lib/ahvm-rust
UNIT=ahvm-rust
RUN_USER=ahvm-rust
BROKER_STATE=
WORKER_ID_BASE=1073741824
WORKER_ID_COUNT=1048576
START=1
while (($#)); do
    case "$1" in
        --prefix) PREFIX=${2:?}; shift 2 ;;
        --config-dir) CONFIG=${2:?}; shift 2 ;;
        --data-dir) DATA=${2:?}; shift 2 ;;
        --unit-name) UNIT=${2:?}; shift 2 ;;
        --user) RUN_USER=${2:?}; shift 2 ;;
        --broker-state-dir) BROKER_STATE=${2:?}; shift 2 ;;
        --worker-id-base) WORKER_ID_BASE=${2:?}; shift 2 ;;
        --worker-id-count) WORKER_ID_COUNT=${2:?}; shift 2 ;;
        --no-start) START=0; shift ;;
        *) echo "Unknown option: $1" >&2; exit 1 ;;
    esac
done
BROKER_STATE=${BROKER_STATE:-$DATA-worker-broker}
[[ $(uname -s) == Linux && $(uname -m) == x86_64 && $EUID == 0 ]] || { echo 'Requires root on Linux x86_64' >&2; exit 1; }
[[ $UNIT =~ ^[a-zA-Z0-9_-]+$ && $RUN_USER =~ ^[a-z_][a-z0-9_-]*$ ]] || { echo 'Invalid unit/user name' >&2; exit 1; }
for path in "$PREFIX" "$CONFIG" "$DATA" "$BROKER_STATE"; do
    [[ $path =~ ^/[a-zA-Z0-9_./-]+$ && $path != *'/../'* && $path != */.. && $path != / ]] || { echo 'Use absolute paths without spaces or traversal' >&2; exit 1; }
    [[ ! -e $path && ! -L $path ]] || { echo "Refusing existing installation/state: $path" >&2; exit 1; }
done
UNIT_FILE=/etc/systemd/system/$UNIT.service
WORKERS_FILE=/etc/systemd/system/$UNIT-workers.service
BROKER_FILE=/etc/systemd/system/$UNIT-worker-broker.service
SLICE=ahvm_${UNIT//-/_}.slice
SLICE_FILE=/etc/systemd/system/$SLICE
[[ ! -e $UNIT_FILE && ! -e $WORKERS_FILE && ! -e $BROKER_FILE && ! -e $SLICE_FILE ]] || { echo 'Daemon/worker resource units already exist' >&2; exit 1; }
python3 - "$PREFIX" "$CONFIG" "$DATA" "$BROKER_STATE" <<'PY'
from pathlib import Path
import os,sys
paths=[Path(p) for p in sys.argv[1:]]
for i,path in enumerate(paths):
    for other in paths[i+1:]:
        if path==other or path in other.parents or other in path.parents:
            raise SystemExit('Installation, configuration, daemon data and broker state must be separate directories')
    for parent in path.parents:
        if not parent.exists(): continue
        s=parent.lstat()
        if parent.is_symlink() or s.st_uid != 0 or (s.st_mode & 0o022 and not s.st_mode & 0o1000):
            raise SystemExit('Installation ancestors must be root-owned and not writable by other users: '+str(parent))
PY
[[ -c /dev/kvm ]] || { echo '/dev/kvm is required' >&2; exit 1; }
getent group kvm >/dev/null || { echo 'kvm group is required' >&2; exit 1; }
command -v python3 >/dev/null
command -v systemctl >/dev/null
python3 - <<'PY'
import ctypes, os, subprocess
from pathlib import Path
libc=ctypes.CDLL(None, use_errno=True)
abi=libc.syscall(444, None, 0, 1)
if abi < 6:
    raise SystemExit('Requires enabled Landlock ABI 6 (Linux 6.12+); worker isolation fails closed')
version=int(subprocess.check_output(['systemctl','--version'],text=True).split()[1])
if version < 254:
    raise SystemExit('Requires systemd 254+ for delegated VM cgroups')
controllers=(Path('/sys/fs/cgroup')/'cgroup.controllers').read_text().split()
if not {'cpu','memory','pids'}.issubset(controllers):
    raise SystemExit('Requires unified cgroup v2 with cpu, memory and pids controllers')
PY
BUNDLE=$(realpath "$BUNDLE")
(cd "$BUNDLE" && sha256sum --quiet --strict -c SHA256SUMS)
[[ -x $BUNDLE/bin/ahvm && -x $BUNDLE/bin/ahvm-worker-broker && -s $BUNDLE/share/base.ext4 ]] || { echo 'Incomplete bundle (isolated worker broker and guest image are required)' >&2; exit 1; }
# Resolve DNS from the host; a local stub is reachable by the host gateway.
RESOLVER=${AHVM_DNS_RESOLVER:-$(awk '$1=="nameserver" && $2 ~ /^[0-9.]+$/ {print $2; exit}' /etc/resolv.conf)}
python3 - "$RESOLVER" <<'PY'
import ipaddress,sys
ipaddress.IPv4Address(sys.argv[1])
PY
if ! id "$RUN_USER" >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --shell /usr/sbin/nologin "$RUN_USER"
fi
getent group "$RUN_USER" >/dev/null || { echo 'Service user requires a matching primary group' >&2; exit 1; }
[[ $(id -u "$RUN_USER") != 0 && $(id -g "$RUN_USER") != 0 ]] || { echo 'Daemon account must have a nonroot UID and primary GID' >&2; exit 1; }
# Reserve a lifetime range independently of daemon-writable data. Never remove
# this record automatically: an old installation may still own worker IDs.
python3 - "$UNIT" "$BROKER_STATE" "$WORKER_ID_BASE" "$WORKER_ID_COUNT" <<'PY'
from pathlib import Path
import fcntl,json,os,stat,subprocess,sys
unit,state,raw_base,raw_count=sys.argv[1:]
try: base,count=int(raw_base),int(raw_count)
except ValueError: raise SystemExit('Worker identity base/count must be integers')
if base<65536 or not 1<=count<=16777216 or base+count>=2147483647:
    raise SystemExit('Invalid worker identity range')
def overlaps(start,length): return start<base+count and start+length>base
for database in ('passwd','group'):
    records=subprocess.check_output(['getent',database],text=True)
    for line in records.splitlines():
        fields=line.split(':')
        if len(fields)<3 or not fields[2].isdigit(): raise SystemExit('Invalid '+database+' entry')
        if overlaps(int(fields[2]),1): raise SystemExit('Worker range overlaps '+database+'; choose --worker-id-base')
for path in (Path('/etc/subuid'),Path('/etc/subgid')):
    if not path.exists(): continue
    for line in path.read_text().splitlines():
        if not line.strip() or line.startswith('#'): continue
        fields=line.split(':')
        if len(fields)!=3 or not all(v.isdigit() for v in fields[1:]): raise SystemExit('Invalid subordinate identity entry')
        if overlaps(int(fields[1]),int(fields[2])): raise SystemExit('Worker range overlaps subordinate IDs; choose --worker-id-base')
registry=Path('/etc/ahvm-worker-ranges')
registry.mkdir(mode=0o700,exist_ok=True)
s=registry.lstat()
if not stat.S_ISDIR(s.st_mode) or s.st_uid!=0 or s.st_mode & 0o077:
    raise SystemExit('Worker identity registry requires a root-owned private directory')
fd=os.open(registry/'.lock',os.O_RDWR|os.O_CREAT|os.O_NOFOLLOW|os.O_CLOEXEC,0o600)
with os.fdopen(fd,'r+') as lock:
    s=os.fstat(lock.fileno())
    if not stat.S_ISREG(s.st_mode) or s.st_uid!=0 or s.st_nlink!=1 or s.st_mode & 0o077: raise SystemExit('Unsafe identity registry lock')
    fcntl.flock(lock,fcntl.LOCK_EX)
    for file in registry.glob('*.json'):
        s=file.lstat()
        if not stat.S_ISREG(s.st_mode) or s.st_uid!=0 or s.st_nlink!=1 or s.st_mode & 0o077 or s.st_size>16384: raise SystemExit('Unsafe identity reservation')
        v=json.loads(file.read_text())
        if overlaps(v['uid_base'],v['identity_count']) or overlaps(v['gid_base'],v['identity_count']):
            raise SystemExit('Worker range is already reserved by '+v['unit']+'; choose a different --worker-id-base')
    record={'unit':unit,'broker_state':state,'uid_base':base,'gid_base':base,'identity_count':count}
    with (registry/(unit+'.json')).open('x') as out:
        os.fchmod(out.fileno(),0o600);json.dump(record,out);out.write('\n');out.flush();os.fsync(out.fileno())
    directory=os.open(registry,os.O_RDONLY|os.O_DIRECTORY)
    try: os.fsync(directory)
    finally: os.close(directory)
PY
# Grant only the render-node group, never the display/card device group.
if [[ -x $BUNDLE/bin/ahvm-vmm-gpu ]]; then
    for node in /dev/dri/renderD*; do
        [[ -c $node ]] || continue
        group=$(stat -c %G "$node")
        [[ $group != root && $group != UNKNOWN ]] || continue
        usermod -a -G "$group" "$RUN_USER"
    done
fi
install -d -m755 "$PREFIX"
cp -a "$BUNDLE/." "$PREFIX/"
chown -R root:root "$PREFIX"
chmod -R go-w "$PREFIX"
chmod 755 "$PREFIX"
# Catch inaccessible ancestors/images before registering a broken service.
for file in bin/ahvm-daemon bin/ahvm-vmm bin/ahvm-netd bin/ahvm-worker-broker lib/libkrunfw.so.5 share/base.ext4; do
    runuser -u "$RUN_USER" -- test -r "$PREFIX/$file" || { echo "Service cannot read $PREFIX/$file" >&2; exit 1; }
done
install -d -m700 "$CONFIG"
install -d -m700 -o "$RUN_USER" -g "$RUN_USER" "$DATA"
install -d -m700 -o "$RUN_USER" -g "$RUN_USER" "$DATA/sandboxes"
install -d -m700 -o root -g root "$BROKER_STATE" "$BROKER_STATE/state" "$BROKER_STATE/jails"
python3 - "$CONFIG" "$PREFIX" "$DATA" "$RESOLVER" "$UNIT" "$SLICE" "$RUN_USER" "$BROKER_STATE" "$WORKER_ID_BASE" "$WORKER_ID_COUNT" <<'PY'
from pathlib import Path
import json,pwd,secrets,sys
config,prefix,data,dns,unit,slice_name,user,broker_state,base,count=sys.argv[1:]
p=Path(config)
account=pwd.getpwnam(user)
broker={'socket':f'/run/{unit}-worker-broker/worker.sock',
        'state_dir':f'{broker_state}/state','jail_dir':f'{broker_state}/jails',
        'data_dir':f'{data}/sandboxes','cgroup_root':f'/sys/fs/cgroup/{slice_name}/{unit}-workers.service',
        'daemon_uid':account.pw_uid,'daemon_gid':account.pw_gid,
        'uid_base':int(base),'gid_base':int(base),'identity_count':int(count),
        'vmm_bin':f'{prefix}/bin/ahvm-vmm','netd_bin':f'{prefix}/bin/ahvm-netd',
        'gpu_bin':f'{prefix}/bin/ahvm-vmm-gpu' if Path(prefix+'/bin/ahvm-vmm-gpu').is_file() else None,
        'lib_path':f'{prefix}/lib','image_roots':[f'{prefix}/share',str(Path(prefix+'/share/base.ext4').resolve())],
        'devices':[str(node) for node in sorted(Path('/dev/dri').glob('renderD*')) if node.is_char_device()]}
if Path('/var/lib/ahvm-images').is_dir(): broker['image_roots'].append(str(Path('/var/lib/ahvm-images').resolve()))
(p/'worker-broker.json').write_text(json.dumps(broker,indent=2)+'\n')
(p/'worker-broker.json').chmod(0o600)
token=secrets.token_hex(32)
(p/'admin.token').write_text(token+'\n'); (p/'admin.token').chmod(0o600)
(p/'private-access.json').write_text('{}\n')
(p/'daemon.env').write_text(f'''AHVM_LISTEN=127.0.0.1:8080
AHVM_DATA_DIR={data}
AHVM_VMM_BIN={prefix}/bin/ahvm-vmm
AHVM_NETD_BIN={prefix}/bin/ahvm-netd
AHVM_CGROUP_ROOT=/sys/fs/cgroup/{slice_name}/{unit}-workers.service
AHVM_WORKER_BROKER_SOCKET={broker['socket']}
AHVM_BASE_IMAGE={prefix}/share/base.ext4
AHVM_LIB={prefix}/lib
AHVM_DNS_RESOLVER={dns}
AHVM_ADMIN_TOKEN={token}
AHVM_PRIVATE_ACCESS_FILE={config}/private-access.json
AHVM_PREVIEW_LISTEN=127.0.0.1:8081
AHVM_PREVIEW_DOMAIN=preview.localhost
AHVM_MAX_CONCURRENT_OPS=4
AHVM_IDLE_SECS=3600
''')
(p/'daemon.env').chmod(0o600)
PY
# Policy must be readable by the service, but writable only by the administrator.
chown root:"$RUN_USER" "$CONFIG" "$CONFIG/private-access.json"
chmod 750 "$CONFIG"
chmod 640 "$CONFIG/private-access.json"
python3 - "$PREFIX/packaging" "$UNIT_FILE" "$WORKERS_FILE" "$BROKER_FILE" "$SLICE_FILE" "$PREFIX" "$CONFIG" "$DATA" "$RUN_USER" "$UNIT" "$SLICE" "$BROKER_STATE" <<'PY'
from pathlib import Path
import sys
source,daemon,workers,broker,slice_file,prefix,config,data,user,unit,slice_name,broker_state=sys.argv[1:]
for name,dest in [('ahvm-rust.service.in',daemon),('ahvm-rust-workers.service.in',workers),('ahvm-worker-broker.service.in',broker),('ahvm-rust.slice.in',slice_file)]:
    s=(Path(source)/name).read_text()
    for key,val in [('PREFIX',prefix),('CONFIG',config),('DATA',data),('USER',user),('UNIT',unit),('SLICE',slice_name),('BROKER_STATE',broker_state)]:
        s=s.replace('@'+key+'@',val)
    Path(dest).write_text(s)
PY
systemd-analyze verify "$UNIT_FILE" "$WORKERS_FILE" "$BROKER_FILE" "$SLICE_FILE"
systemctl daemon-reload
if ((START)); then systemctl enable --now "$UNIT.service"; fi
printf 'Installed. CLI: %s/bin/ahvm\nToken file: %s/admin.token (root-only)\nConfig: %s/daemon.env\nService: %s.service\n' "$PREFIX" "$CONFIG" "$CONFIG" "$UNIT"
