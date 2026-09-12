# Durable storage: R2 qualification

Phase 2 adds a private S3 transport to the experimental `ahvm-volume` crate.
It does not change the CLI, daemon, VMM, live disks or release artifacts. Local
storage remains the default. See [the full plan](DURABLE-STORAGE-PLAN.md).

## What is implemented

- AWS SigV4 signing using the AWS signing library, with an explicit HTTPS S3
  endpoint, region, bucket and prefix. R2 uses region `auto` and the appropriate
  jurisdiction endpoint. No ambient AWS credential discovery or metadata lookup.
- Immutable 64-KiB chunks use `If-None-Match: *`. A pre-existing chunk must be
  downloaded and verified before an upload retry can succeed.
- Head creation uses `If-None-Match: *`; replacement uses the exact opaque ETag
  in `If-Match`. A 412 response means conflict. Publication errors or a missing
  success ETag force reconciliation by reopening; no blind publication retry.
- Bounded GET bodies, including responses without Content-Length. Reads check
  SHA-256 and exact chunk size. Missing data never becomes zero-filled data.
- Five-second connect and twenty-second total request deadlines. Redirects,
  automatic retries and inherited HTTP proxies are disabled. Errors and Debug
  output do not include credentials or server response bodies.
- Explicit file credentials, with a 16-KiB input bound and private permissions on
  Unix. Optional S3 session tokens are supported. Normal configuration rejects
  plaintext HTTP; only unit tests can use a loopback HTTP fixture.

This first adapter accepts path-style S3 endpoints, simple lowercase/hyphen bucket
names and ASCII identifier prefix segments. Other S3 providers are unqualified.
There is no background upload, local disk cache, write log, GC, encryption-key
lifecycle or guest fsync support yet. The existing 64-MiB protocol-model limit
still applies. These synchronous calls must not run on an async executor thread
when the host service is integrated later.

## Run the opt-in gate

Use a **dedicated private qualification bucket**, not the images bucket or the
manual-backup repository. Give the test credential Object Read & Write access
only to that bucket. No public hostname is necessary. Store this configuration
outside the repository, with a private parent directory and file mode `600`:

```json
{
  "endpoint": "https://ACCOUNT_ID.eu.r2.cloudflarestorage.com",
  "region": "auto",
  "bucket": "ahvm-volume-qualification",
  "prefix": "phase2",
  "access_key_id": "ACCESS_KEY_ID",
  "secret_access_key": "SECRET_ACCESS_KEY"
}
```

```bash
cargo build --manifest-path rust/Cargo.toml --locked \
  -p ahvm-volume --example r2_qualify
rust/target/debug/examples/r2_qualify \
  "$HOME/.config/ahvm-volume/r2.json" "qual-$(date +%s)" qualify
```

The volume ID must be fresh: the gate refuses to overwrite an existing head.
The gate launches independent child processes to seed, recover and compete for
publication. It injects a failure after the first real chunk upload and drops a
successful real publication response at the ObjectStore boundary. Separately,
HTTP unit tests close the connection after receiving the request, test oversized
responses, redirects, missing ETags and timeouts. These are distinct tests, not a
claim that we forcibly dropped packets inside Cloudflare.

To recover the final marker on a different machine, copy only the private
configuration, compile the same example and run:

```bash
rust/target/debug/examples/r2_qualify /private/path/r2.json VOLUME_ID verify-lost
```

No volume data or local cache is copied. `seed` and `verify` are also available
for the initial cross-chunk marker. The tool deliberately has no broad bucket
cleanup command. After the gate, an operator can list and delete **only the exact
fixture prefix** `<prefix>/<volume-id>/`. Never run recursive cleanup against a
bucket with live volumes. Concurrent cleanup/GC is not supported.

## Qualification evidence, 2026-09-12

- 19 protocol/HTTP unit tests pass on macOS and Linux (`agent_house`). No VMs
  created. Local HTTP fixtures require no credentials or cloud access in CI.
- The portable Rust workspace suite passed (180 tests, VMM excluded); workspace
  Clippy and formatting passed. Hypervisor integration gates were not run.
- Real R2 gates passed on macOS and `agent_house`: fresh-process recovery,
  create-if-absent refusal, interrupted upload preserving the old head, immutable
  retry verification, two independent processes competing with one winner, and
  fresh-process reconciliation after an injected lost publication reply.
- `agent_house` read the final marker committed from the Mac with only the
  credential/configuration file, no local volume state. This demonstrates
  cross-machine protocol recovery, not recovery of a guest disk or RAM.
- The Linux gate completed in 5.78 seconds. This small debug-build correctness
  gate is **not** a throughput or guest-fsync benchmark.
- Real R2 corruption and deletion of a referenced fixture chunk both caused
  recovery to fail closed. The bucket-scoped credential received 403 when trying
  to access the separate backup bucket.
- Both fixtures used 14 objects totaling 787,168 bytes before the corruption test.
  Their exact prefixes were deleted after qualification; the temporary credential
  copy on the server was removed. The private bucket and local scoped credential
  remain for subsequent qualification. No recurring upload or backup was added.
- Qualification bucket: EU jurisdiction, managed public access disabled, no
  custom domains. Its account-owned credential is restricted to this bucket.
  Production services, image storage and manual backups were not modified.

The next phase must qualify a real block-device path, bounded caching and guest
flush semantics before any host-loss durability promise or cloud rollout.

References: [R2 conditional S3 operations](https://developers.cloudflare.com/r2/api/s3/api/),
[R2 credentials and bucket scoping](https://developers.cloudflare.com/r2/api/tokens/),
[AWS SigV4 signing library](https://docs.rs/aws-sigv4/1.5.1/aws_sigv4/http_request/).
