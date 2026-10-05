import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from tools.product_state import ROOTFS_INPUT_FILES, rootfs_image_bytes, selected_tool_payload

ROOT = Path(__file__).resolve().parents[2]


class ContainersPayloadTests(unittest.TestCase):
    def test_separate_payload_and_declared_inputs(self):
        self.assertEqual(selected_tool_payload('containers'), 'containers')
        self.assertEqual(rootfs_image_bytes('containers'), 384 * 1024 * 1024)
        self.assertEqual(rootfs_image_bytes('none'), 128 * 1024 * 1024)
        for name in ['scripts/build-containers-payload.sh', 'config/containers-apks.lock', 'tests/guest/container-bwrap.sh']:
            self.assertIn(name, ROOTFS_INPUT_FILES)

    def test_signed_closure_has_runtime_and_rootless_helpers(self):
        rows = [line.split('#', 1)[0].strip() for line in
                (ROOT/'config/containers-apks.lock').read_text().splitlines()]
        pins = [row.split('=', 1) for row in rows if row]
        self.assertEqual(len(pins), len(dict(pins)))
        for name in ['bubblewrap', 'crun', 'podman', 'conmon', 'fuse-overlayfs',
                     'shadow-subids', 'util-linux', 'musl']:
            self.assertIn(name, dict(pins))

    def test_builder_does_not_run_host_container_or_account_scriptlets(self):
        text = (ROOT/'scripts/build-containers-payload.sh').read_text()
        subprocess.run(['bash', '-n', str(ROOT/'scripts/build-containers-payload.sh')], check=True)
        self.assertIn('--no-scripts --no-chown add', text)
        self.assertIn("images/'alpine-3.24.1.tar'", text)
        self.assertIn("'RepoTags': ['alpine:3.24.1', 'alpine:latest']", text)
        self.assertNotIn('podman pull', text)
        self.assertNotIn("out/'etc/passwd'", text)
        self.assertNotIn('sudo', text)

    def test_bwrap_acceptance_requests_real_isolation_and_readonly_checks(self):
        path = ROOT/'tests/guest/container-bwrap.sh'
        subprocess.run(['sh', '-n', str(path)], check=True)
        text = path.read_text()
        self.assertIn('--ro-bind "$root" /', text)
        self.assertIn('--tmpfs /tmp', text)
        self.assertIn('user mnt pid net uts ipc', text)
        self.assertIn('THEKERNEL_CONTAINER_BWRAP_OK', text)

    def test_builder_refuses_unrelated_output_before_package_install(self):
        with tempfile.TemporaryDirectory(dir=os.environ.get('TMPDIR')) as directory:
            out = Path(directory)/'unrelated'
            out.mkdir()
            keep = out/'keep'
            keep.write_text('keep')
            run = subprocess.run(['bash', str(ROOT/'scripts/build-containers-payload.sh'),
                                  '--output', str(out)], capture_output=True, text=True)
            self.assertNotEqual(run.returncode, 0)
            self.assertIn('unrelated output tree', run.stderr)
            self.assertEqual(keep.read_text(), 'keep')
