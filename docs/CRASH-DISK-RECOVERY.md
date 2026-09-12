# Cold disk recovery after worker SIGKILL

The runtime pins imago 0.2.4 plus the metadata-ordering fix at
`a3e05bc06bd3364ca97ffa772fb1f3c52db062f8` in the workspace manifest and lockfile.
The maintenance mirror retains upstream history and licensing:
https://github.com/mariobm/imago/pull/1.

## Reproduction and cause

On the Ubuntu dev image, create a VM, write a file and run guest `sync`, then
SIGKILL its worker before any checkpoint exists. The old worker failed its next
cold boot with an ext4 directory-checksum error. `qemu-img check` found an L2
mapping table referenced by L1 whose allocation refcount was zero. On reopening,
that table could be allocated again and overwritten. Upgrading from imago 0.2.3
to unpatched 0.2.4 alone reproduced the same failure.

Metadata allocation updates a cached refcount, but the new L2 table and its L1
reference are written directly. The usual data-mapping cache dependency does not
protect that publication. The fix flushes refblock dependencies before returning
a metadata allocation to the caller, before it can publish that reference.
Ordinary writes to existing data clusters do not gain an additional flush.

## Validation

The dependency's deterministic test reads a published L2 reference and its refcount
through a separate file handle, before any explicit image flush. Removing the fix
fails with refcount 0; the fixed version returns 1. Its unit tests, doctests, clippy
and synchronous-feature build passed on macOS.

The Linux KVM regression `kvm_crash_disk` creates one 1-vCPU/1-GiB VM, writes and
syncs a new marker, hard-kills its worker, waits for observed death and cold-starts
it, five times. Every marker from every prior iteration must survive, and the test
asserts there was no checkpoint. The gate passed on agent_house with Ubuntu dev.
Snapshot/restore also passed three cycles, preserving session RAM and rolling disk
state back as intended. Engine tests/clippy passed on macOS; Linux VMM build and
engine/VMM clippy passed with the exact dependency pin.

Run the cold-crash gate on Linux with `AHVM_KVM_TEST=1`, `AHVM_VMM_BIN`,
`AHVM_GUEST_IMAGE` and `LD_LIBRARY_PATH` configured, using:

```bash
cargo test --manifest-path rust/Cargo.toml --locked -p ahvm-engine --test kvm_crash_disk -- --nocapture
```

This proves worker-process crash recovery for the tested path, not host power-loss
protection. Unsynced writes can still be lost. Existing checkpoint rollback policy
is unchanged. The fix prevents new allocation corruption; it does not repair an
already-corrupted overlay. Preserve such disks for recovery rather than silently
resetting them. Host-loss backup qualification remains separate.
