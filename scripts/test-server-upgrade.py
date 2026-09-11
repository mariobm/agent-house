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
