# macOS signing and notarization

Public macOS releases must be Developer ID signed and notarized. The signed
AHVM update catalog authenticates downloads but does not replace Apple signing.

## Identity and credentials

The release identity is `Developer ID Application: MARIO BALUKCIC (RJS3R23FND)`.
The private key stays in the release Mac's Keychain. Do not export it to GitHub
Actions. Xcode Settings → Apple Accounts → Manage Certificates creates or shows
this identity. An Apple Development or Apple Distribution certificate is not a
substitute for Developer ID Application.

Configure notarization once on that Mac. At <https://account.apple.com>, generate
an app-specific password under Sign-In and Security → App-Specific Passwords.
Run this command and enter the password at its hidden prompt (never in a script,
command-line argument, issue or chat):

```bash
xcrun notarytool store-credentials ahvm-notary \
  --apple-id YOUR_APPLE_ACCOUNT_EMAIL \
  --team-id RJS3R23FND
```

The command validates the credentials with Apple before saving them in Keychain.
An existing App Store Connect API key can also be stored with `notarytool
store-credentials`; neither credential is the code-signing private key. Changing
the Apple Account password revokes its app-specific passwords.

## Build, sign, notarize, package

GitHub's **Build release assets** workflow produces
`unsigned-client-darwin-aarch64` and `unsigned-client-darwin-x86_64` artifacts.
Each contains a tar archive preserving executable permissions. These are build
inputs, not public release assets. The Linux job still produces release packages.
Download artifacts only from the qualified commit's successful workflow run.

On the release Mac, extract each unsigned archive into its own bundle directory.
The layout is `bin/ahvm`, `desktop/ahvm-desktop`, and the desktop notices. For each
architecture, run (replace paths, version and platform with the qualified values):

```bash
python3 scripts/macos_signing.py sign /path/to/bundle
python3 scripts/macos_signing.py notarize /path/to/bundle --profile ahvm-notary
python3 scripts/package-distribution.py /path/to/bundle /path/to/dist VERSION darwin-aarch64
```

Signing uses stable identifiers `app.ahvm.cli` and `app.ahvm.desktop`, hardened
runtime, and Apple's secure timestamp. No relaxed runtime entitlements are added.
Do not strip, rebuild or re-sign the executables after notarization. The generated
`.app` directory from `desktop-viewer/build.sh` is not a release artifact: the CLI
ships and launches the standalone viewer binary. Do not distribute that unsigned
app bundle separately.

The notarization step submits a temporary ZIP containing precisely the two signed
executables. It prints the submission ID before waiting, and waits up to 20 minutes.
If Apple takes longer, retain the signed bundle and resume the same submission:

```bash
python3 scripts/macos_signing.py notarize /path/to/bundle \
  --profile ahvm-notary --submission-id SUBMISSION_UUID
```

After acceptance, the script allows another five minutes for Apple's tickets to
become available to local verification. If that still fails, it refuses packaging;
retain the submission ID and retry verification later. Do not run notarization
checks on newly signed binaries before submitting them: a negative lookup can
remain cached even after Apple accepts the submission.

For a rejected submission, inspect `xcrun notarytool log SUBMISSION_UUID
--keychain-profile ahvm-notary`. Fix the reported issue before signing/submitting
again. Raw executables cannot have stapled tickets; macOS retrieves their tickets
from Apple online. The ZIP is a submission container, not an extra public asset.
An offline-first installer would need a separate stapled PKG or DMG.

Packaging verifies the Apple certificate chain, our Team ID, both stable
identifiers, hardened runtime, secure timestamp and notarization tickets.
`publish-release.py` repeats verification against executables extracted from the
final `.gz` and `.tar.gz` assets before any publication or catalog update. Run the
publisher on the release Mac, with network access to Apple. Do not weaken these
checks to work around an Apple service outage.

`package-distribution.py --allow-unsigned` (after its four positional arguments)
is only for development fixtures. The publisher has no corresponding bypass.
Published assets are immutable; signing must ship in a new release, not replace
an existing version's files.

## Qualification

- Run `python3 scripts/test-client-bundle.py` for unsigned-package rejection and
  installer archive/checksum contracts (also included in `make test`).
- On the signing Mac run `python3 scripts/macos_signing.py verify /path/to/bundle`.
- Run CLI contract tests against the signed CLI, and smoke-test the signed viewer.
- Test the downloaded release on a Mac with normal Gatekeeper settings.
- Compare `codesign -d -r- PATH` for successive signed CLI versions: the designated
  requirement should retain the same identifier and team across updates.

The transition from an ad-hoc release may prompt once for Keychain access. Stable
signing enables future versions to meet the same identity requirement; it does
not bypass the user's Keychain decisions or eliminate prompts for locked keys.

References: [Apple notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow),
[Apple app-specific passwords](https://support.apple.com/102654).
