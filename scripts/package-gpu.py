#!/usr/bin/env python3
"""Bundle GPU worker dependencies, including dynamically loaded Mesa drivers.
Run inside the same glibc 2.35 builder as package-rust.sh.
"""
from pathlib import Path
import json, os, shutil, subprocess, sys
root = Path(sys.argv[1]); dest = root / 'gpu/lib'; dest.mkdir(parents=True)
system = Path('/usr/lib/x86_64-linux-gnu')
# EGL's vendor loader and Mesa load these at runtime; ldd alone misses them.
seeds = [system / name for name in ['libEGL.so.1', 'libEGL_mesa.so.0', 'libGL.so.1', 'libgbm.so.1']]
dri = root / 'gpu/dri'; dri.mkdir()
for name in ['iris_dri.so', 'radeonsi_dri.so', 'swrast_dri.so']:
    source = system / 'dri' / name
    shutil.copy2(source, dri / name)
    seeds.append(dri / name)
(root / 'gpu/egl.json').write_text(json.dumps({'file_format_version':'1.0.0','ICD':{'library_path':'libEGL_mesa.so.0'}}))
pending = [root / 'bin/ahvm-vmm-gpu', *seeds]; seen = set()
excluded = {'libc.so.6','libm.so.6','libpthread.so.0','libdl.so.2','librt.so.1','ld-linux-x86-64.so.2'}
while pending:
    binary = pending.pop()
    if binary.name in seen: continue
    seen.add(binary.name)
    if binary.parent == system:
        shutil.copy2(binary, dest / binary.name)
        binary = dest / binary.name
    output = subprocess.check_output(['ldd', str(binary)], text=True)
    if 'not found' in output: raise SystemExit(output)
    for line in output.splitlines():
        parts = line.split()
        if len(parts) < 3 or parts[1] != '=>' or not parts[2].startswith('/'): continue
        name, source = parts[0], Path(parts[2])
        if name in excluded or (dest / name).exists(): continue
        shutil.copy2(source, dest / name)
        pending.append(dest / name)
    if binary.parent == dest:
        subprocess.run(['patchelf', '--set-rpath', '$ORIGIN', str(binary)], check=True)
# Preserve package copyright notices for every bundled system graphics library.
notices = root / 'licenses/gpu'; notices.mkdir(parents=True, exist_ok=True)
shutil.copy2(Path(os.environ['AHVM_VIRGL_PREFIX']) / 'COPYING', notices / 'virglrenderer.COPYING')
for name in seen:
    paths = list(system.glob(name)) + list((system / 'dri').glob(name))
    for path in paths:
        result = subprocess.run(['dpkg-query','-S',str(path)],capture_output=True,text=True)
        for line in result.stdout.splitlines():
            package = line.split(': ',1)[0].split(':',1)[0]
            copyright = Path('/usr/share/doc') / package / 'copyright'
            if copyright.is_file(): shutil.copy2(copyright, notices / (package + '.copyright'))
