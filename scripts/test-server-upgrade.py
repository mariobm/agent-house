#!/usr/bin/env python3
"""Exercise rollback after a candidate daemon fails its health check."""
import importlib.util
from pathlib import Path
import tempfile
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('bootstrap', Path(__file__).with_name('server-bootstrap.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    module.PREFIX = root / 'runtime'
    module.DATA = root / 'data'
    module.PREFIX.mkdir(); module.DATA.mkdir()
    (module.PREFIX / 'share').mkdir()
    image = root / 'image.ext4'; image.write_bytes(b'image')
    (module.PREFIX / 'share/base.ext4').symlink_to(image)
    (module.PREFIX / 'version').write_text('old')
    (module.DATA / 'daemon.db').write_bytes(b'original database')
    stage = root / 'candidate'; stage.mkdir(); (stage / 'share').mkdir()
    (stage / 'share/base.ext4').symlink_to(image)
    (stage / 'version').write_text('new')
    temp = root / 'work'; temp.mkdir()
    def fail_health():
        (module.DATA / 'daemon.db').write_bytes(b'candidate modified database')
        raise RuntimeError('candidate failed')
    with patch.object(module, 'run'), patch.object(module, 'health', side_effect=fail_health):
        try:
            module.upgrade_runtime(stage, temp)
            raise AssertionError('expected failure')
        except RuntimeError:
            pass
    assert (module.PREFIX / 'version').read_text() == 'old'
    assert (module.DATA / 'daemon.db').read_bytes() == b'original database'
    assert (module.PREFIX / 'share/base.ext4').read_bytes() == b'image'
    assert not module.PREFIX.with_name('runtime.previous').exists()
print('Failed server candidate restores original runtime and database')

with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    module.PREFIX, module.DATA, module.CONFIG = (root / name for name in ('runtime', 'data', 'config'))
    for path in (module.PREFIX, module.DATA, module.CONFIG): path.mkdir()
    (module.PREFIX / 'share').mkdir()
    image = root / 'image.ext4'; image.write_bytes(b'image')
    (module.PREFIX / 'share/base.ext4').symlink_to(image)
    (module.PREFIX / 'version').write_text('old')
    (module.DATA / 'daemon.db').write_bytes(b'original database')
    (module.CONFIG / 'daemon.env').write_text('AHVM_LISTEN=127.0.0.1:8080\n')
    stage = root / 'candidate'; stage.mkdir()
    (stage / 'bin').mkdir(); (stage / 'bin/ahvm-worker-broker').write_text('candidate broker')
    (stage / 'share').mkdir(); (stage / 'share/base.ext4').symlink_to(image)
    (stage / 'version').write_text('new')
    temp = root / 'work'; temp.mkdir()
    with patch.object(module, 'run') as run:
        try:
            module.upgrade_runtime(stage, temp)
            raise AssertionError('unmigrated installation accepted')
        except ValueError as error:
            assert 'migration' in str(error)
        run.assert_not_called()
    assert (module.PREFIX / 'version').read_text() == 'old'
    assert (module.DATA / 'daemon.db').read_bytes() == b'original database'
    (module.CONFIG / 'daemon.env').write_text('AHVM_WORKER_BROKER_SOCKET=/run/test/worker.sock\n')
    (module.CONFIG / 'worker-broker.json').write_text('{}')
    with patch.object(module, 'run') as run, patch.object(module, 'health', side_effect=fail_health):
        try:
            module.upgrade_runtime(stage, temp)
            raise AssertionError('expected failed candidate')
        except RuntimeError:
            pass
        calls = [call.args for call in run.call_args_list]
        assert calls == [
            ('systemctl', 'is-active', '--quiet', 'ahvm-rust-worker-broker'),
            ('systemctl', 'stop', 'ahvm-rust'),
            ('systemctl', 'stop', 'ahvm-rust-worker-broker'),
            ('systemctl', 'start', 'ahvm-rust-worker-broker'),
            ('systemctl', 'start', 'ahvm-rust'),
            ('systemctl', 'stop', 'ahvm-rust'),
            ('systemctl', 'stop', 'ahvm-rust-worker-broker'),
            ('systemctl', 'start', 'ahvm-rust-worker-broker'),
            ('systemctl', 'start', 'ahvm-rust'),
        ], calls
    assert (module.PREFIX / 'version').read_text() == 'old'
    assert (module.DATA / 'daemon.db').read_bytes() == b'original database'
print('Unmigrated upgrade refuses before mutation; isolated candidate rollback restores broker and daemon')
