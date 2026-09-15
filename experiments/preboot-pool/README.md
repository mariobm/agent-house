# Prebooted Ubuntu pool experiment

This is a qualification tool, **not a production pool or tenant authorization
service**. It measures the benefit of preparing one pristine Ubuntu VM before a
create request. The existing daemon and CLI provide real guest execution; a
local SQLite ledger models an atomic placement claim. Production Cloud would
need that claim integrated into its existing D1 ownership, entitlement and
operation transaction.

The script keeps at most **one 1-vCPU / 2-GiB VM in total**. Preparation reserves
capacity before calling the daemon. Failed/uncertain preparation stays charged
until deletion and storage reclamation are confirmed. A claimed VM never returns
to the pool. Replenishment is sequential and stops on an error.

It compares three ordinary creates, three running pool hits, and three paused
pool hits. A hit measures durable local claim through a real CLI shell executing
a marker. Preparation is reported separately, not hidden in the total. The
SQLite claim does not model Cloud network/D1 latency or grant tenant access.

Checks include competing claims, retry after lost claim response, restart,
profile mismatch, retained resource accounting, unchanged worker/boot identity on
claim, separate volume IDs, guest entropy, image identity, stale user files and
inherited machine/SSH identity. No credentials, shell contents or raw benchmark
results belong in this directory.

## Run

Use a dedicated isolated daemon and volume supervisor, a disposable data
root, a spare NBD device, and an already-imported Ubuntu image. Never point the
tool at production. It temporarily changes the isolated daemon's idle timeout
and restores it on exit. It refuses the ordinary port 8080 and a nonempty daemon.

```sh
python3 -m unittest discover -s experiments/preboot-pool -v

python3 experiments/preboot-pool/measure.py \
  --endpoint http://127.0.0.1:28183 \
  --token-file /path/to/isolated/token \
  --cli /path/to/ahvm \
  --engine-root /path/to/isolated/data/sandboxes \
  --volume-root /path/to/isolated/volumes \
  --image /path/to/default.ext4 \
  --ledger /path/to/isolated/pool.db
```

Keep the ledger after an interrupted run. Inspect any retained allocation rather
than deleting the ledger to bypass its accounting. Stop the isolated services
only after VM deletion and volume reclamation are confirmed. Test results and
integration decisions are recorded in `docs/PREBOOT-POOL.md`.
