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
        self.assertEqual(rootfs_image_bytes('inspect'), 192 * 1024 * 1024)
        for name in ['scripts/build-inspect-payload.sh', 'config/inspect-apks.lock',
                     'tests/guest/inspect-tools.sh', 'tests/guest/block-gpt-tools.sh']:
            self.assertIn(name, ROOTFS_INPUT_FILES)
        self.assertIn('tests/guest/block-partition-mkfs-smoke.sh', ROOTFS_INPUT_FILES)

    def test_e2fsprogs_and_partition_tools_are_pinned_and_staged(self):
        lines = [line.split('#', 1)[0].strip()
                 for line in (ROOT/'config/inspect-apks.lock').read_text().splitlines()]
        pins = dict(line.split('=', 1) for line in lines if line)
        self.assertEqual(pins['e2fsprogs'], '1.47.4-r0')
        self.assertEqual(pins['e2fsprogs-libs'], '1.47.4-r0')
        builder = (ROOT/'scripts/build-inspect-payload.sh').read_text()
        for tool in ['sfdisk', 'mke2fs', 'mkfs.ext4', 'e2fsck']:
            self.assertIn(f"'{tool}'", builder)
        self.assertIn('partition-mkfs-smoke.sh', builder)

    def test_namespace_acceptance_is_staged_as_an_optional_guest_script(self):
        self.assertIn('tests/guest/container-namespace.sh', ROOTFS_INPUT_FILES)
        builder = (ROOT/'scripts/build-inspect-payload.sh').read_text()
        self.assertIn("out/'opt/thekernel-tools/container-namespace.sh'", builder)
        script = ROOT/'tests/guest/container-namespace.sh'
        subprocess.run(['sh', '-n', str(script)], check=True)
        text = script.read_text()
        self.assertIn('unshare" -mpfUr --mount-proc', text)
        self.assertIn('--pid="/proc/$launcher/ns/pid_for_children"', text)
        self.assertIn('THEKERNEL_CONTAINER_NAMESPACE_OK', text)

    def test_exact_package_closure_has_required_real_tools(self):
        lines = [line.split('#', 1)[0].strip()
                 for line in (ROOT/'config/inspect-apks.lock').read_text().splitlines()]
        pins = [line.split('=', 1) for line in lines if line]
        self.assertEqual(len(pins), len(dict(pins)))
        for name in ['busybox', 'musl', 'procps-ng', 'sysstat', 'util-linux', 'htop',
                     'pciutils', 'usbutils', 'iproute2-ss', 'net-tools', 'eudev', 'eudev-hwids']:
            self.assertIn(name, dict(pins))
        self.assertNotIn('podman', dict(pins))
        self.assertTrue(all(version for _, version in pins))

    def test_terminfo_follows_the_alpine_runtime_path(self):
        source = (ROOT/'scripts/build-inspect-payload.sh').read_text()
        self.assertIn("'etc/terminfo'", source)
        self.assertNotIn("for path in ['etc']", source)

    def test_hardware_names_compile_in_staging_without_host_udev_actions(self):
        source = (ROOT/'scripts/build-inspect-payload.sh').read_text()
        self.assertIn("'hwdb', '--update', '--root', str(root)", source)
        self.assertIn("shutil.copy2(hwdb, out/'etc/udev/hwdb.bin')", source)
        self.assertIn('check=True, env=env', source)
        self.assertIn('probe usb-hwdb', (ROOT/'tests/guest/inspect-tools.sh').read_text())
        self.assertNotIn("'trigger'", source)
        self.assertNotIn("'control'", source)
        self.assertNotIn("str(root/'sbin/udevd')", source)

    def test_busybox_keeps_multicall_dispatch_basename(self):
        source = (ROOT/'scripts/build-inspect-payload.sh').read_text()
        self.assertIn("bin_dir/'busybox'", source)
        self.assertIn('/opt/thekernel-tools/bin/busybox {program}', source)
        self.assertNotIn('alpine-busybox', source)

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
