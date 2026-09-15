import tempfile
import threading
import subprocess
import sys
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from pool import Pool

PROFILE = dict(cpus=1, memory_mib=2048, storage='replicated', image_sha256='a'*64, network_bytes_per_sec=0)


class PoolTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name)/'pool.db'
        self.pool = Pool(self.path)

    def tearDown(self):
        self.tmp.cleanup()

    def test_one_spare_and_capacity_charged_until_reclamation(self):
        self.assertTrue(self.pool.reserve('one', PROFILE))
        self.assertFalse(self.pool.reserve('two', PROFILE))
        self.pool.ready('one', PROFILE)
        self.assertEqual(self.pool.claim('alice', 'request', PROFILE), 'one')
        self.assertFalse(self.pool.reserve('two', PROFILE))
        self.pool.deleting('one')
        self.assertFalse(self.pool.reserve('two', PROFILE))
        self.pool.reclaimed('one')
        self.assertTrue(self.pool.reserve('two', PROFILE))

    def test_competing_claims_have_exactly_one_owner(self):
        self.pool.reserve('one', PROFILE)
        self.pool.ready('one', PROFILE)
        barrier = threading.Barrier(8)
        def claim(i):
            independent = Pool(self.path)
            barrier.wait(timeout=5)
            return independent.claim(str(i), 'request', PROFILE)
        with ThreadPoolExecutor(max_workers=8) as executor:
            results = list(executor.map(claim, range(8)))
        self.assertEqual(results.count('one'), 1)
        self.assertEqual(results.count(None), 7)

    def test_crash_before_claim_commit_rolls_back(self):
        self.pool.reserve('one', PROFILE)
        self.pool.ready('one', PROFILE)
        subprocess.run([sys.executable, '-c',
            "import sqlite3,os,sys; db=sqlite3.connect(sys.argv[1]); db.execute('BEGIN IMMEDIATE'); "
            "db.execute(\"UPDATE machines SET state='claimed',owner='alice',request_key='lost' WHERE id='one'\"); os._exit(0)",
            str(self.path)], check=True)
        self.assertEqual(Pool(self.path).claim('bob', 'request', PROFILE), 'one')

    def test_competing_refills_cannot_exceed_one_spare(self):
        barrier = threading.Barrier(4)
        def reserve(i):
            independent = Pool(self.path, cpu_budget=4, memory_budget_mib=8192)
            barrier.wait(timeout=5)
            return independent.reserve(str(i), PROFILE)
        with ThreadPoolExecutor(max_workers=4) as executor:
            results = list(executor.map(reserve, range(4)))
        self.assertEqual(results.count(True), 1)

    def test_restart_and_lost_reply_never_reassign(self):
        self.pool.reserve('one', PROFILE)
        # A create interrupted before readiness is not claimable.
        self.assertIsNone(Pool(self.path).claim('alice', 'request', PROFILE))
        self.pool.ready('one', PROFILE)
        self.assertEqual(self.pool.claim('alice', 'request', PROFILE), 'one')
        reopened = Pool(self.path)
        self.assertEqual(reopened.claim('alice', 'request', PROFILE), 'one')
        self.assertIsNone(reopened.claim('bob', 'request', PROFILE))
        self.assertFalse(reopened.reserve('two', PROFILE))

    def test_image_sizing_network_or_storage_mismatch_is_a_miss(self):
        self.pool.reserve('one', PROFILE)
        self.pool.ready('one', PROFILE)
        for key, value in [('cpus',2),('memory_mib',4096),('storage','local'),('image_sha256','b'*64),('network_bytes_per_sec',1048576)]:
            self.assertIsNone(self.pool.claim('alice', key, {**PROFILE,key:value}))
        self.assertEqual(self.pool.claim('alice', 'matching', PROFILE), 'one')
        with self.assertRaises(ValueError):
            self.pool.claim('alice', 'matching', {**PROFILE,'cpus':2})

    def test_claimed_vm_cannot_return_to_pool(self):
        self.pool.reserve('one', PROFILE)
        self.pool.ready('one', PROFILE)
        self.pool.claim('alice', 'request', PROFILE)
        with self.assertRaises(ValueError):
            self.pool.ready('one', PROFILE)
        self.pool.deleting('one')
        self.pool.reclaimed('one')
        with self.assertRaises(ValueError):
            self.pool.claim('alice', 'request', PROFILE)
        self.assertIsNone(self.pool.claim('bob', 'another', PROFILE))

    def test_reservation_respects_cpu_and_ram_independently(self):
        self.assertFalse(self.pool.reserve('ram', {**PROFILE,'memory_mib':4096}))
        self.assertFalse(self.pool.reserve('cpu', {**PROFILE,'cpus':2}))
        self.assertTrue(self.pool.reserve('fits', PROFILE))
        with self.assertRaises(ValueError):
            self.pool.reclaimed('fits')


if __name__ == '__main__':
    unittest.main()
