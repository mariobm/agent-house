"""Deterministic fail-closed paths without root, NBD or R2."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('service', Path(__file__).with_name('engine-service.py'))
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

class Recovery(unittest.TestCase):
    def service(self):
        service = m.Service.__new__(m.Service)
        service.a = Mock(device='/dev/nbd0')
        service.record = dict(worker={'pid':123}, nbd_pid=456, starting=False,
                              volume_id='a'*64, deleted=False)
        service.nbd_pid = Mock(return_value=456)
        service.persist = Mock()
        return service

    def test_live_consumer_blocks_detach_before_any_signal(self):
        service = self.service()
        with patch.object(m, 'consumers', return_value={456, 789}), patch.object(m, 'terminate') as kill:
            with self.assertRaises(AssertionError): service.detach()
            kill.assert_not_called()

    def test_dead_worker_does_not_allow_reconnect_under_vm(self):
        service = self.service()
        with patch.object(m, 'alive', return_value=False), patch.object(m, 'consumers', return_value={789}):
            with self.assertRaises(AssertionError): service.attach()
            service.persist.assert_not_called()

    def test_adoption_inspects_without_detach(self):
        service = self.service()
        service.control = Mock(return_value={'pending_bytes':42})
        service.detach = Mock(side_effect=AssertionError('must not detach'))
        with patch.object(m, 'alive', return_value=True):
            self.assertEqual(service.attach(), {'pending_bytes':42})
        service.detach.assert_not_called()

    def test_unrecorded_device_is_never_disconnected(self):
        service = self.service()
        service.nbd_pid.return_value = 999
        with patch.object(m, 'consumers', return_value=set()), patch.object(m.subprocess, 'run') as run:
            with self.assertRaises(AssertionError): service.detach()
            run.assert_not_called()

    def test_failed_delete_retains_intent(self):
        service = self.service()
        service.detach = Mock(side_effect=RuntimeError('busy'))
        with self.assertRaises(RuntimeError):
            service.request(dict(version=1, volume_id='a'*64, operation='delete'))
        self.assertTrue(service.record['deleted'])
        service.persist.assert_called_once()
        with self.assertRaises(AssertionError):
            service.request(dict(version=1, volume_id='a'*64, operation='attach'))

    def test_unrecorded_spawn_cannot_be_forgotten_by_delete(self):
        service = self.service()
        service.record.update(starting=True, worker=None)
        with self.assertRaises(AssertionError):
            service.request(dict(version=1, volume_id='a'*64, operation='delete'))
        self.assertTrue(service.record['starting'])
        self.assertTrue(service.record['deleted'])

    def test_detached_status_is_unavailable_not_cached_healthy(self):
        service = self.service()
        service.record.update(worker=None, status=None)
        with self.assertRaises(AssertionError):
            service.request(dict(version=1, volume_id='a'*64, operation='status'))

    def test_short_lived_helper_is_waited_for_without_excluding_it(self):
        service = self.service()
        with patch.object(m, 'consumers', side_effect=[{456, 789}, set()]), patch.object(m.time, 'sleep') as wait:
            service.unused()
            wait.assert_called_once()

    def test_reused_pid_is_not_signalled(self):
        with patch.object(m.os, 'pidfd_open', return_value=99, create=True), patch.object(m.os, 'close'), patch.object(m, 'alive', return_value=False), patch.object(m.signal, 'pidfd_send_signal', create=True) as kill:
            m.terminate({'pid':123})
            kill.assert_not_called()

if __name__ == '__main__': unittest.main()
