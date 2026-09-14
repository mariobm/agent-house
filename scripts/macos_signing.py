#!/usr/bin/env python3
"""Developer ID signing and notarization for the standalone macOS executables.

The private key stays in the signing Mac's Keychain. See docs/MACOS-RELEASE.md.
"""
import argparse
import gzip
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import zipfile

TEAM = 'RJS3R23FND'
IDENTITY = f'Developer ID Application: MARIO BALUKCIC ({TEAM})'
TARGETS = {'bin/ahvm': 'app.ahvm.cli', 'desktop/ahvm-desktop': 'app.ahvm.desktop'}


def run(*args, **kwargs):
    return subprocess.run(list(map(str, args)), check=True, **kwargs)


def require_mac():
    if sys.platform != 'darwin':
        raise SystemExit('macOS release verification requires a Mac with Apple codesign')


def verify(binary, identifier, *, notarized=True, ticket_deadline=0):
    require_mac()
    # Check the actual certificate chain, not just the printable Authority name.
    requirement = (f'anchor apple generic and identifier "{identifier}" '
                   f'and certificate leaf[subject.OU] = "{TEAM}" '
                   'and certificate 1[field.1.2.840.113635.100.6.2.6] exists '
                   'and certificate leaf[field.1.2.840.113635.100.6.1.13] exists')
    run('codesign', '--verify', '--strict', '--all-architectures', '-R=' + requirement, binary)
    details = subprocess.check_output(['codesign', '--display', '--verbose=4', str(binary)],
                                      stderr=subprocess.STDOUT, text=True)
    flags = re.search(r'flags=0x([0-9a-fA-F]+)', details)
    if not flags or not int(flags[1], 16) & 0x10000 or '\nTimestamp=' not in details:
        raise SystemExit(f'{binary}: hardened runtime and secure timestamp required')
    if notarized:
        # Raw executables cannot carry stapled tickets. Check Apple's online ticket.
        while True:
            try:
                run('codesign', '--verify', '--strict', '--all-architectures',
                    '-R=notarized', '--check-notarization', binary)
                break
            except subprocess.CalledProcessError:
                if time.monotonic() >= ticket_deadline:
                    raise
                print('Waiting for Apple ticket availability; verification still required.', flush=True)
                time.sleep(min(15, max(0, ticket_deadline - time.monotonic())))


def verify_bundle(bundle, *, notarized=True, ticket_deadline=0):
    for name, identifier in TARGETS.items():
        verify(bundle / name, identifier, notarized=notarized, ticket_deadline=ticket_deadline)


def verify_artifact(archive, kind):
    """Verify final compressed bytes before publication, regardless of metadata."""
    require_mac()
    with tempfile.TemporaryDirectory(prefix='ahvm-verify-') as directory:
        temp = Path(directory)
        if kind == 'cli':
            with gzip.open(archive, 'rb') as source, (temp / 'ahvm').open('wb') as dest:
                shutil.copyfileobj(source, dest)
        elif kind == 'client':
            with tarfile.open(archive, 'r:gz') as tar:
                for name in ('ahvm', 'ahvm-desktop'):
                    members = [m for m in tar.getmembers() if m.name == name]
                    if len(members) != 1 or not members[0].isfile():
                        raise SystemExit(f'{archive}: missing, duplicate or nonregular {name}')
                    with tar.extractfile(members[0]) as source, (temp / name).open('wb') as dest:
                        shutil.copyfileobj(source, dest)
        else:
            raise SystemExit(f'Unexpected macOS artifact kind: {kind}')
        for binary in temp.iterdir():
            binary.chmod(0o755)
            verify(binary, 'app.ahvm.cli' if binary.name == 'ahvm' else 'app.ahvm.desktop')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['sign', 'notarize', 'verify'])
    parser.add_argument('bundle', type=Path)
    parser.add_argument('--identity', default=IDENTITY)
    parser.add_argument('--profile', default='ahvm-notary', help='notarytool Keychain profile')
    parser.add_argument('--submission-id', help='resume waiting for an existing submission')
    args = parser.parse_args()
    require_mac()
    if args.action == 'sign':
        for name, identifier in TARGETS.items():
            run('codesign', '--force', '--sign', args.identity, '--identifier', identifier,
                '--options', 'runtime', '--timestamp', args.bundle / name)
        verify_bundle(args.bundle, notarized=False)
        print('Signed both executables. Notarize before packaging; do not modify them afterward.')
    elif args.action == 'notarize':
        verify_bundle(args.bundle, notarized=False)
        developer = Path(subprocess.check_output(['xcode-select', '-p'], text=True).strip())
        notary = developer / 'usr/bin/notarytool'
        auth = ['--keychain-profile', args.profile]
        # Print the submission ID immediately. If waiting times out, resume that ID
        # instead of signing again or creating duplicate submissions.
        if args.submission_id:
            run(notary, 'wait', args.submission_id, *auth, '--timeout', '20m')
        else:
            with tempfile.TemporaryDirectory(prefix='ahvm-notarize-') as directory:
                archive = Path(directory) / 'ahvm.zip'
                with zipfile.ZipFile(archive, 'w', zipfile.ZIP_DEFLATED) as zip_file:
                    for name in TARGETS:
                        zip_file.write(args.bundle / name, Path(name).name)
                run(notary, 'submit', archive, *auth, '--wait', '--timeout', '20m')
        # A successful process exit alone is insufficient: validate each ticket.
        verify_bundle(args.bundle, ticket_deadline=time.monotonic() + 300)
        print('Both executables are signed and notarized. Ready to package.')
    else:
        verify_bundle(args.bundle)


if __name__ == '__main__':
    main()
