# Bounded protocol fuzzing

Install `cargo-fuzz` and a nightly Rust toolchain. From `rust/ahvm-netd`:

```sh
CARGO_BUILD_JOBS=2 cargo +nightly fuzz run dns fuzz/seeds/dns -- \
  -max_total_time=60 -rss_limit_mb=512 -max_len=4096
CARGO_BUILD_JOBS=2 cargo +nightly fuzz run ingress fuzz/seeds/ingress -- \
  -max_total_time=60 -rss_limit_mb=512 -max_len=2048
```

Run sequentially. These processes create no sandboxes and make no network calls.
The `#[path]` imports compile the production parsers, not copies. DNS assertions
cover QR/ID matching and the generated truncated reply. Ingress assertions cover
accepted source identity, fragmentation and size limits. Smoltcp disables its
checksum checks under `cfg(fuzzing)`; `wire::tests` and the real gateway protocol
gate cover checksum corruption in normal builds. This harness does not fuzz the
entire smoltcp TCP state machine or VMM device code.

The two binary ingress seeds have valid IPv4 and TCP/UDP checksums. Minimized
failures should become deterministic production-module regression tests. Keep
large generated corpora and crash artifacts outside commits.
