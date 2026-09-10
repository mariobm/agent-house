# Streaming file uploads

`ahvm files put SANDBOX LOCAL_PATH GUEST_PATH` streams a file; use `-` for
stdin. There is no fixed total upload size cap. Available guest disk space and
the CLI's configurable `--timeout` still apply.

The CLI uses `PUT /v1/sandboxes/{id}/files/upload?path=...` with a raw binary
body and the normal bearer authentication. Content-Length and chunked HTTP
requests are supported. Success returns `{"bytes":N}`. The legacy JSON/base64
file-write endpoint remains available for small writes.

The daemon forwards at most 64 KiB per chunk through a bounded channel to one
guest connection. Uploads consume an operation permit and hold activity until
the backend finishes, preventing automatic idle-stop during a transfer. Busy
admission returns 409; uploads are not automatically retried.

The guest writes a sibling temporary file, syncs it, and atomically renames it
only after an explicit commit with the complete byte count. Disconnects before
commit, invalid frames and write errors remove the temporary and preserve the
previous destination. Upload I/O has a 30-second idle/stall timeout. A VM crash
can leave a temporary file; this is not resumable upload storage. Losing the
HTTP response after commit can leave the client uncertain whether it succeeded.

Deploy matching CLI, daemon and guest forge artifacts. Existing guest images
need the new forge protocol; an old guest rejects upload before replacement.

Coverage includes multi-megabyte file/stdin CLI requests, ownership checks,
HTTP body errors, a 10 MiB guest transaction, empty replacement, disconnect,
invalid commit count and oversized chunks. The packaging acceptance gate adds
real KVM and installed-CLI coverage.
