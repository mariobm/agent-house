"""One-slot placement experiment, not a production authorization boundary.

VM IDs and disks never change on claim. A claimed VM is never put back. SQLite
models the transaction we must later integrate with Cloud's D1 ownership,
entitlement and operation rows; this ledger does not grant daemon API access.
"""
import json
import sqlite3
from contextlib import closing, contextmanager


class Pool:
    def __init__(self, path, *, cpu_budget=1, memory_budget_mib=2048):
        self.path = str(path)
        self.cpu_budget = cpu_budget
        self.memory_budget = memory_budget_mib
        with closing(self.connect()) as db:
            db.executescript("""
                CREATE TABLE IF NOT EXISTS machines (
                    id TEXT PRIMARY KEY, profile TEXT NOT NULL,
                    cpus INTEGER NOT NULL CHECK(cpus > 0),
                    memory_mib INTEGER NOT NULL CHECK(memory_mib > 0),
                    state TEXT NOT NULL CHECK(state IN ('preparing','ready','claimed','deleting','deleted')),
                    owner TEXT, request_key TEXT,
                    CHECK((owner IS NULL) = (request_key IS NULL))
                );
                CREATE UNIQUE INDEX IF NOT EXISTS one_spare ON machines((1))
                    WHERE state IN ('preparing','ready');
                CREATE UNIQUE INDEX IF NOT EXISTS claim_request ON machines(owner,request_key)
                    WHERE owner IS NOT NULL;
            """)

    def connect(self):
        db = sqlite3.connect(self.path, timeout=5)
        db.execute('PRAGMA journal_mode=WAL')
        db.execute('PRAGMA synchronous=FULL')
        db.row_factory = sqlite3.Row
        return db

    @contextmanager
    def transaction(self):
        db = self.connect()
        try:
            db.execute('BEGIN IMMEDIATE')
            yield db
            db.commit()
        except BaseException:
            db.rollback()
            raise
        finally:
            db.close()

    @staticmethod
    def profile(value):
        return json.dumps(value, sort_keys=True, separators=(',', ':'))

    def reserve(self, vm_id, profile):
        """Charge before dispatching create; uncertain creates stay charged."""
        cpus, memory = profile['cpus'], profile['memory_mib']
        if not isinstance(cpus, int) or not isinstance(memory, int) or cpus <= 0 or memory <= 0:
            raise ValueError('invalid sizing')
        with self.transaction() as db:
            if db.execute("SELECT 1 FROM machines WHERE state IN ('preparing','ready')").fetchone():
                return False
            used = db.execute("SELECT coalesce(sum(cpus),0),coalesce(sum(memory_mib),0) FROM machines WHERE state!='deleted'").fetchone()
            if used[0] + cpus > self.cpu_budget or used[1] + memory > self.memory_budget:
                return False
            db.execute("INSERT INTO machines(id,profile,cpus,memory_mib,state) VALUES(?,?,?,?,'preparing')",
                       (vm_id, self.profile(profile), cpus, memory))
            return True

    def ready(self, vm_id, profile):
        """Caller has verified guest readiness and the exact image/profile."""
        with self.transaction() as db:
            result = db.execute("UPDATE machines SET state='ready' WHERE id=? AND state='preparing' AND profile=?",
                                (vm_id, self.profile(profile)))
            if result.rowcount != 1:
                raise ValueError('not a matching preparing VM')

    def claim(self, owner, request_key, profile):
        if not owner or not request_key:
            raise ValueError('owner and request identity required')
        encoded = self.profile(profile)
        with self.transaction() as db:
            prior = db.execute('SELECT * FROM machines WHERE owner=? AND request_key=?', (owner, request_key)).fetchone()
            if prior:
                if prior['profile'] != encoded:
                    raise ValueError('request key reused with a different profile')
                if prior['state'] != 'claimed':
                    raise ValueError('claim was retired; create a new request')
                return prior['id']
            row = db.execute("SELECT id FROM machines WHERE state='ready' AND profile=?", (encoded,)).fetchone()
            if row is None:
                return None
            db.execute("UPDATE machines SET state='claimed',owner=?,request_key=? WHERE id=? AND state='ready'",
                       (owner, request_key, row['id']))
            return row['id']

    def deleting(self, vm_id):
        with self.transaction() as db:
            result = db.execute("UPDATE machines SET state='deleting' WHERE id=? AND state!='deleted'", (vm_id,))
            if result.rowcount != 1:
                raise ValueError('unknown or already deleted VM')

    def reclaimed(self, vm_id):
        """Only after VM absence AND replicated storage reclamation are observed."""
        with self.transaction() as db:
            result = db.execute("UPDATE machines SET state='deleted' WHERE id=? AND state='deleting'", (vm_id,))
            if result.rowcount != 1:
                raise ValueError('deletion was not requested')
