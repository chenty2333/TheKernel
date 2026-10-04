import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from tools.product_state import ROOTFS_INPUT_FILES, rootfs_image_bytes, selected_tool_payload

ROOT = Path(__file__).resolve().parents[2]


class InspectPayloadTests(unittest.TestCase):
    def test_separate_selection_and_all_build_inputs(self):
        self.assertEqual(selected_tool_payload('inspect'), 'inspect')
        self.assertEqual(rootfs_image_bytes('inspect'), 160 * 1024 * 1024)
        for name in ['scripts/build-inspect-payload.sh', 'config/inspect-apks.lock',
                     'tests/guest/inspect-tools.sh']:
            self.assertIn(name, ROOTFS_INPUT_FILES)

    def test_exact_package_closure_has_required_real_tools(self):
        lines = [line.split('#', 1)[0].strip()
                 for line in (ROOT/'config/inspect-apks.lock').read_text().splitlines()]
        pins = [line.split('=', 1) for line in lines if line]
        self.assertEqual(len(pins), len(dict(pins)))
        for name in ['busybox', 'musl', 'procps-ng', 'sysstat', 'util-linux', 'htop',
                     'pciutils', 'usbutils', 'iproute2-ss', 'net-tools']:
            self.assertIn(name, dict(pins))
        self.assertNotIn('podman', dict(pins))
        self.assertTrue(all(version for _, version in pins))

    def test_builder_refuses_to_remove_unrelated_files_before_network(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as directory:
            out = Path(directory)/'precious'
            out.mkdir()
            keep = out/'keep.txt'
            keep.write_text('untouched')
            result = subprocess.run(['bash', str(ROOT/'scripts/build-inspect-payload.sh'),
                                     '--output', str(out)], capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('unrelated output tree', result.stderr)
            self.assertEqual(keep.read_text(), 'untouched')
