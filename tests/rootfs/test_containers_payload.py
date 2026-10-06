import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import importlib.util
import struct

from tools.product_state import ROOTFS_INPUT_FILES, rootfs_image_bytes, selected_tool_payload

ROOT = Path(__file__).resolve().parents[2]


class ContainersPayloadTests(unittest.TestCase):
    def test_offline_filecaps_are_exact_signed_helper_rights(self):
        spec = importlib.util.spec_from_file_location('container_filecaps', ROOT/'scripts/install-container-filecaps.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        valid = {path: struct.pack('<IIIII', 0x02000001, 1 << bit, 0, 0, 0).hex()
                 for path, bit in module.TARGETS.items()}
        self.assertEqual(set(module.validate_filecaps(valid)), set(module.TARGETS))
        for invalid in [{}, {**valid, '/bin/sh': valid[next(iter(valid))]},
                        {**valid, '/opt/thekernel-tools/bin/newuidmap': '00'*20}]:
            with self.assertRaises(ValueError):
                module.validate_filecaps(invalid)
        builder = (ROOT/'scripts/build-containers-payload.sh').read_text()
        self.assertIn('SCHILY.xattr.security.capability', builder)
        self.assertIn('scripts/install-container-filecaps.py', ROOTFS_INPUT_FILES)
        self.assertIn('install-container-filecaps.py', (ROOT/'scripts/build-rootfs.sh').read_text())

    def test_rootless_guest_setup_keeps_real_identity_and_offline_overlay(self):
        script = (ROOT/'tests/guest/container-podman.sh').read_text()
        helper = (ROOT/'tests/guest/tools/container-rootless-run.c').read_text()
        self.assertIn('tests/guest/container-podman.sh', ROOTFS_INPUT_FILES)
        self.assertIn('--network=none --pull=never', script)
        self.assertIn('--storage-driver=overlay', script)
        self.assertIn('load --input /opt/thekernel-containers/images/', script)
        self.assertIn('>> /etc/passwd', script)
        self.assertIn('cgroup.subtree_control', script)
        self.assertIn('thekernel-containers-namespace.sh',
                      (ROOT/'scripts/build-containers-payload.sh').read_text())
        self.assertIn("'ps', 'top', 'mount'",
                      (ROOT/'scripts/build-containers-payload.sh').read_text())
        self.assertIn("'^hello$'", script)
        self.assertIn("ps --all --quiet", script)
        self.assertIn('[ ! -s "$base/containers-after-run" ]', script)
        self.assertIn("THEKERNEL_PODMAN_REMOVE_OK", script)
        self.assertIn('geteuid() != 1000', helper)
        self.assertLess(helper.index('write(fd, pid'), helper.index('setuid(1000)'))

    def test_crun_memory_checks_resident_effect_and_real_oom(self):
        script = (ROOT/'tests/guest/container-crun.sh').read_text()
        probe = (ROOT/'tests/guest/tools/container-limit-probe.c').read_text()
        self.assertIn('"memory":{"limit":33554432}', script)
        self.assertIn('[ "$result" = 137 ]', script)
        self.assertIn('for event in max oom oom_kill', script)
        self.assertIn('memory.current', script)
        self.assertIn('memory.peak', script)
        self.assertIn('resident - after', probe)
        self.assertIn('512UL * 1024 * 1024', probe)
        self.assertIn('MAP_PRIVATE | MAP_ANONYMOUS', probe)

    def test_crun_guest_uses_real_resources_and_kept_lifecycle(self):
        script = (ROOT/'tests/guest/container-crun.sh').read_text()
        self.assertIn('"pids":{"limit":8}', script)
        self.assertIn('run --keep', script)
        self.assertIn('pids.events', script)
        self.assertIn('pids.current', script)
        self.assertIn('delete "$pids"', script)
        self.assertIn('tests/guest/container-crun.sh', ROOTFS_INPUT_FILES)

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

    def test_cached_signed_versions_are_explicit_solver_candidates(self):
        text = (ROOT/'scripts/build-containers-payload.sh').read_text()
        self.assertIn('stem=${pin/=/-}', text)
        self.assertIn('ambiguous cached source for $pin', text)
        self.assertIn('add "${PINS[@]}" "${EXACT_APKS[@]}"', text)
        self.assertNotIn('--allow-untrusted', text)
        self.assertIn('actual != expected', text)

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
