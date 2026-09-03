#!/usr/bin/env python3
"""Static contract tests for the non-logind graphics userspace session."""
from __future__ import annotations

import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock

from tests.support import load_script_module, repo_root


ROOT = repo_root()
GRAPHICS = ROOT / "config" / "graphics"


class GraphicsRootfsConfigTests(unittest.TestCase):
    def read(self, relative: str) -> str:
        return (GRAPHICS / relative).read_text()

    def test_common_config_builds_an_ext2_image_against_linux_6_12_headers(self) -> None:
        common = self.read("common.config")
        self.assertIn("BR2_TARGET_ROOTFS_EXT2=y", common)
        self.assertNotIn("BR2_ROOTFS_EXT2=y", common)
        # The guest UAPI oracles must compile against the same 6.12 headers as
        # the standalone Linux oracle kernel.
        self.assertIn("BR2_KERNEL_HEADERS_6_12=y", common)
        self.assertNotIn("SYSTEMD_LOGIND=y", common)

    def test_flavor_manifest_matches_the_checked_in_fragments_and_overlays(self) -> None:
        manifest: dict[str, str] = {}
        for line in self.read("flavors.env").splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            key, separator, value = line.partition("=")
            self.assertEqual(separator, "=", line)
            manifest[key] = value.strip().strip('"')
        flavors = manifest["FLAVORS"].split()
        self.assertEqual(flavors, [
            "headless-abi-smoke",
            "q35-graphics-seatd",
            "q35-software-desktop",
            "q35-graphics-benchmark",
            "q35-venus-desktop",
            "q35-graphics-logind",
        ])
        for flavor in flavors:
            self.assertTrue((GRAPHICS / f"{flavor}.fragment").is_file(), flavor)
            key = flavor.upper().replace("-", "_")
            overlay = manifest[f"FLAVOR_{key}_OVERLAY"]
            self.assertTrue((GRAPHICS / "overlay" / overlay).is_dir(), flavor)
            self.assertIn(manifest[f"FLAVOR_{key}_SESSION"], ("seatd", "logind"))
            backend = manifest[f"FLAVOR_{key}_BACKEND"]
            self.assertIn(
                backend,
                ("headless-backend.so", "drm-backend.so", "none"),
            )
            if backend != "none":
                link = GRAPHICS / "overlay" / overlay / "etc" / "weston" / "weston.ini"
                self.assertTrue(link.is_symlink(), flavor)
                expected = "weston-headless.ini" if backend == "headless-backend.so" else "weston-drm.ini"
                self.assertEqual(link.readlink(), pathlib.Path(expected), flavor)
        for flavor in manifest["SMOKE_FLAVORS"].split():
            self.assertIn(flavor, flavors)
        for flavor in manifest["CI_CHECK_FLAVORS"].split():
            self.assertIn(flavor, flavors)

    def test_seatd_init_waits_for_the_socket_before_weston_starts(self) -> None:
        seatd = self.read("overlay/common/etc/init.d/S70seatd")
        self.assertIn("while [ ! -S /run/seatd.sock ]", seatd)

    def test_guest_uapi_oracles_fail_loudly_and_run_before_the_ready_marker(self) -> None:
        smoke = self.read("overlay/common/usr/local/bin/graphics-abi-smoke")
        self.assertLess(smoke.index("/usr/local/bin/drm-uapi-oracle"), smoke.index("THEKERNEL_GRAPHICS_ABI_SMOKE_READY"))
        self.assertLess(smoke.index("/usr/local/bin/evdev-uapi-oracle"), smoke.index("THEKERNEL_GRAPHICS_ABI_SMOKE_READY"))
        q35_smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertLess(q35_smoke.index("/usr/local/bin/drm-uapi-oracle"), q35_smoke.index("q35-wayland-shm-client"))
        self.assertLess(q35_smoke.index("/usr/local/bin/evdev-uapi-oracle"), q35_smoke.index("q35-wayland-shm-client"))
        drm_oracle = (ROOT / "tests/guest/graphics/drm-uapi-oracle.c").read_text()
        self.assertIn('strcmp(state, "FAIL") == 0', drm_oracle)
        self.assertIn("return failures == 0 ? 0 : 1", drm_oracle)
        evdev_oracle = (ROOT / "tests/guest/graphics/evdev-uapi-oracle.c").read_text()
        self.assertIn('result("evdev.open", "FAIL"', evdev_oracle)
        self.assertIn("bit_bytes == (int)sizeof(unsigned long)", evdev_oracle)
        self.assertIn("return failures == 0 ? 0 : 1", evdev_oracle)

    def test_graphics_smoke_hands_an_existing_rootfs_to_the_drive_transport_without_building(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        with tempfile.TemporaryDirectory() as directory:
            rootfs = pathlib.Path(directory) / "graphics-rootfs.ext2"
            rootfs.write_bytes(b"rootfs")
            screenshot = pathlib.Path(directory) / "graphics.ppm"
            args = module.build_parser().parse_args([
                "graphics-smoke", "--no-build", "--rootfs", str(rootfs),
                "--screenshot", str(screenshot),
            ])
            calls: dict[str, object] = {}
            module.build_kernel = lambda *_args, **_kwargs: self.fail("--no-build invoked build_kernel")
            module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = calls["spec"]
        self.assertEqual(spec.rootfs, rootfs.resolve())
        self.assertEqual(spec.rootfs_transport, "drive")

    def test_piglit_desktop_gl_contract_keeps_complete_tests_and_uses_buildroots_cli(self) -> None:
        piglit = self.read("overlay/q35-software-desktop/usr/local/bin/q35-piglit-quick")
        self.assertIn("runner=/usr/bin/piglit", piglit)
        self.assertIn('-p wayland -1 --timeout 60 quick "$results"', piglit)
        self.assertIn("q35-piglit-result-check", piglit)
        self.assertNotIn("piglit-runner", piglit)

    def test_software_graphics_profiles_use_softpipe_without_triggering_buildroot_legacy(self) -> None:
        profiles = (
            "common.config",
            "q35-software-desktop.fragment",
            "q35-graphics-seatd.fragment",
            "q35-graphics-benchmark.fragment",
        )
        for profile in profiles:
            contents = self.read(profile)
            self.assertIn("BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SOFTPIPE=y", contents)
            self.assertNotIn("BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SWRAST", contents)
            self.assertNotIn("BR2_LEGACY=y", contents)

    def test_logind_preflight_accepts_indented_vt_handoff_commands(self) -> None:
        cycle = self.read("overlay/q35-graphics-logind/usr/local/bin/thekernel-logind-cycle")
        session = self.read("overlay/q35-graphics-logind/usr/local/bin/thekernel-sway-session")
        self.assertIn('    chvt "$tty"', cycle)
        self.assertIn('    loginctl activate "$session"', cycle)
        self.assertIn('while [ "$cycle" -lt 1 ]', cycle)
        self.assertNotIn("inactive_fd_not_revoked", cycle)
        self.assertIn('rotate_lease "$active_state"', cycle)
        self.assertIn('lease_request "$active_old_dir"', cycle)
        self.assertIn('lease_request "$fresh_lease_dir"', cycle)
        self.assertIn("start_lease()", session)
        self.assertIn('lease.current.next', session)
        subprocess.run(
            [
                ROOT / "scripts/build-graphics-rootfs.sh",
                "--flavor",
                "q35-graphics-logind",
                "--check",
            ],
            cwd=ROOT,
            check=True,
        )

    def test_product_cli_accepts_an_existing_graphics_rootfs(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "run", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
        ])
        self.assertEqual(args.rootfs, "/tmp/graphics-rootfs.ext2")

    def test_graphics_smoke_configures_one_marker_for_qmp_and_stop(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
        with mock.patch.object(pathlib.Path, "is_file", lambda _self: True):
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = calls["spec"]
        self.assertEqual(spec.stop_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(spec.graphics_profile, "headless")
        self.assertEqual(spec.rootfs_transport, "drive")

    def test_q35_headless_graphics_smoke_keeps_the_software_marker_and_pixel_oracle(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-graphics-seatd",
            "--graphics-profile", "headless",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
        with mock.patch.object(pathlib.Path, "is_file", lambda _self: True):
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = calls["spec"]
        self.assertEqual(spec.stop_after_marker, "THEKERNEL_Q35_WESTON_READY")
        self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_Q35_WESTON_READY")
        self.assertEqual(spec.qmp_screenshot_size, (800, 600))
        self.assertEqual(spec.qmp_screenshot_color_blocks[0].rgb, (255, 0, 0))

    def test_virgl_headless_graphics_smoke_is_rejected_without_a_qmp_pixel_oracle(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-graphics-seatd",
        ])
        args.graphics_profile = "virgl-headless"
        with mock.patch.object(pathlib.Path, "is_file", lambda _self: True):
            with self.assertRaisesRegex(module.ProductError, "no QMP pixel-oracle surface"):
                module.graphics_smoke_cmd(args)

    def test_virgl_graphics_smoke_uses_the_virgl_marker_and_pixel_oracle(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-graphics-seatd",
            "--graphics-profile", "virgl-interactive",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda _artifacts, spec: calls.update(spec=spec) or 0
        with mock.patch.object(pathlib.Path, "is_file", lambda _self: True):
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        spec = calls["spec"]
        self.assertEqual(spec.stop_after_marker, "THEKERNEL_Q35_VIRGL_READY")
        self.assertEqual(spec.qmp_screenshot_after_marker, "THEKERNEL_Q35_VIRGL_READY")
        self.assertEqual(spec.qmp_screenshot_size, (800, 600))
        self.assertEqual(spec.qmp_screenshot_color_blocks[0].rgb, (255, 0, 0))

    def test_marker_gated_screenshot_is_forwarded_to_qemu_runner(self) -> None:
        module = load_script_module("thekernel_product", "tools/thekernel.py")
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            artifacts = module.Artifacts(root / "state", module.Variant(memory="128M"))
            for path in (artifacts.kernel, artifacts.esp):
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"artifact")
            rootfs = root / "graphics.ext2"
            rootfs.write_bytes(b"rootfs")
            screenshot = root / "graphics.ppm"
            seen: dict[str, object] = {}

            def fake_run(config):
                seen["config"] = config
                return type("Result", (), {
                    "returncode": 75, "error_message": None, "log_path": config.log_path,
                    "intentionally_stopped": True,
                    "guest_clean_shutdown": False,
                })()

            module.run = fake_run
            self.assertEqual(module.run_product(
                artifacts,
                module.RunSpec(
                    accel="tcg", timeout=30, workdir=root / "run", interactive=False,
                    input_after_marker=None, stop_after_marker="THEKERNEL_GRAPHICS_ABI_SMOKE_READY",
                    commands=None, extra_block=None, rootfs=rootfs, qmp_screenshot=screenshot,
                    qmp_screenshot_after_marker="THEKERNEL_GRAPHICS_ABI_SMOKE_READY",
                    run_cpus=1,
                ),
            ), 0)
        config = seen["config"]
        self.assertEqual(config.qmp.screenshot, screenshot.resolve())
        self.assertEqual(config.qmp.screenshot_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(config.qmp.socket.name, "graphics-smoke.qmp")


if __name__ == "__main__":
    unittest.main()
