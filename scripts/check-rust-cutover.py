#!/usr/bin/env python3
"""Prevent reintroducing the retired runtime/build paths (submodule excluded)."""
from pathlib import Path
import subprocess
import re

root = Path(__file__).resolve().parents[1]
files = subprocess.check_output(['git', 'ls-files'], cwd=root, text=True).splitlines()
errors = []
for name in files:
    path = root / name
    if not path.is_file():
        continue
    if name.endswith('.go') or name in ('go.mod', 'go.sum'):
        errors.append(name)
    if name.startswith('.github/workflows/') or name == 'Makefile':
        if re.search(r'setup-go|\bgo (?:build|test|vet|run|install|mod)\b', path.read_text()):
            errors.append(name)
assert not errors, f'Retired Go runtime/build references: {errors}'
print('Rust-only runtime and CI check passed')
