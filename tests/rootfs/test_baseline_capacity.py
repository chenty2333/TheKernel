from pathlib import Path
import re
import unittest

from tools.product_state import rootfs_image_bytes

ROOT = Path(__file__).resolve().parents[2]


class BaselineCapacityTests(unittest.TestCase):
    def test_cli_and_standalone_builders_have_the_same_regression_headroom(self):
        # The static test corpus needs writable headroom for non-root Linux
        # oracle cases and the sparse/direct-I/O filesystem fixtures.
        expected = 160
        self.assertEqual(rootfs_image_bytes('none'), expected * 1024 * 1024)
        for script, expression in [
            ('build-rootfs.sh', r'SIZE_MB=\$\{THEKERNEL_ROOTFS_SIZE_MB:-(\d+)\}'),
            ('create-rootfs-image.sh', r'^SIZE_MB=(\d+)$'),
        ]:
            source = (ROOT/'scripts'/script).read_text()
            match = re.search(expression, source, re.MULTILINE)
            self.assertIsNotNone(match, script)
            self.assertEqual(int(match.group(1)), expected, script)
        self.assertEqual(rootfs_image_bytes('inspect'), 160 * 1024 * 1024)
