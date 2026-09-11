# Remote distribution qualification

Qualification on `agent_house`, using at most one 2-CPU / 4-GiB sandbox at a time:

- Saved SSH host from macOS, first-host default, named create, exec and token
  retrieval without a locally stored API token.
- Direct server download of the signed Ubuntu image from R2. Verified compressed
  digest and size, guest ABI and sparse extraction; approximately 2.2 GiB allocated
  for the 16 GiB filesystem. Repeated use takes the cached image.
- Real v0.1.0 to v0.2.0 server upgrade with the existing VM surviving. Its worker's
  executable remained in the previous runtime directory, and a guest marker file
  remained readable after adoption by the new daemon.
- Fresh `host add --install`, explicit `--image ubuntu-dev`, Node/Bun/Python exec,
  1.9 MB streamed file upload, interactive PTY shell and clean detach.
- Standalone updater downloaded the signed candidate, atomically replaced a
  lower-version test client, and retained the previous executable.
- Deterministic tests cover host selection/rejections, tampered signatures,
  wrong signing key, expired catalogs, decompression limits, and runtime/database
  rollback on failed candidate health.

The portable Rust suite and CLI contracts pass. KVM-specific tests that are
skipped outside Linux are not counted as live validation. Docker image import,
Docker-in-guest and authenticated AI-provider workloads are outside this change.
