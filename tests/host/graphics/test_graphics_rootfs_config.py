#!/usr/bin/env python3
"""Static contract tests for the non-logind graphics userspace session."""
from __future__ import annotations

import pathlib
import importlib.util
import sys
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]
GRAPHICS = ROOT / "config" / "graphics"


class GraphicsRootfsConfigTests(unittest.TestCase):
    def read(self, relative: str) -> str:
        return (GRAPHICS / relative).read_text()

    def test_busybox_init_orders_udev_before_seatd_before_weston(self) -> None:
        common = self.read("common.config")
        self.assertIn("BR2_INIT_BUSYBOX=y", common)
        self.assertIn("BR2_TARGET_ROOTFS_EXT2=y", common)
        self.assertNotIn("BR2_ROOTFS_EXT2=y", common)
        self.assertNotIn("BR2_INIT_NONE=y", common)
        init = GRAPHICS / "overlay/common/etc/init.d"
        self.assertTrue((init / "S70seatd").is_file())
        self.assertTrue((init / "S80weston").is_file())
        self.assertLess("S10udev", "S70seatd")
        self.assertLess("S70seatd", "S80weston")

    def test_wrapper_uses_host_perl_unmodified_unless_a_prefix_is_explicit(self) -> None:
        wrapper = (ROOT / "scripts/build-graphics-rootfs.sh").read_text()
        self.assertIn('if [ -n "$host_deps_dir" ]; then', wrapper)
        self.assertLess(
            wrapper.index('if [ -n "$perl_module_root" ]; then'),
            wrapper.index('if [ -z "$host_deps_dir" ]; then host_deps_dir='),
        )
        self.assertIn('export PATH="$host_deps_dir/bin:$PATH"', wrapper)

    def test_seatd_socket_and_device_policy_are_auditable(self) -> None:
        seatd = self.read("overlay/common/etc/init.d/S70seatd")
        rules = self.read("overlay/common/etc/udev/rules.d/71-thekernel-graphics.rules")
        self.assertIn("-g seat", seatd)
        self.assertIn('KERNEL=="card[0-9]*", GROUP="video", MODE="0660"', rules)
        self.assertIn('KERNEL=="renderD[0-9]*", GROUP="render", MODE="0660"', rules)
        self.assertIn('KERNEL=="fb[0-9]*", GROUP="video", MODE="0660"', rules)
        self.assertIn('KERNEL=="event[0-9]*", GROUP="input", MODE="0660"', rules)
        self.assertNotIn('KERNEL=="fb[0-9]*", GROUP="video", MODE="0666"', rules)

    def test_compositor_is_nonroot_and_uses_private_runtime_directory(self) -> None:
        users = self.read("users.table")
        init = self.read("overlay/common/etc/init.d/S80weston")
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        self.assertIn("weston -1 weston -1 !* /var/lib/weston /bin/sh seat,render", users)
        self.assertIn('-c "$USER"', init)
        self.assertIn("chmod 0700", init)
        self.assertIn('export XDG_RUNTIME_DIR="$runtime_dir"', session)
        self.assertIn('[ "$(id -u)" -ne 0 ]', session)

    def test_headless_init_profile_selects_headless_backend_without_a_login_shell(self) -> None:
        common = self.read("common.config")
        init = self.read("overlay/common/etc/init.d/S80weston")
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        self.assertNotIn("SYSTEMD_LOGIND=y", common)
        self.assertEqual(
            self.read("overlay/headless/etc/thekernel-graphics-flavor").strip(),
            "headless-abi-smoke",
        )
        self.assertIn("FLAVOR_FILE=/etc/thekernel-graphics-flavor", init)
        self.assertNotIn("THEKERNEL_GRAPHICS_FLAVOR", init)
        self.assertIn('"$flavor"', init)
        self.assertIn("headless-abi-smoke", session)
        self.assertIn("exec weston --config=/etc/weston/weston-headless.ini", session)

    def test_q35_init_profile_selects_drm_backend_without_a_login_shell(self) -> None:
        init = self.read("overlay/common/etc/init.d/S80weston")
        session = self.read("overlay/common/usr/local/bin/graphics-session")
        self.assertEqual(
            self.read("overlay/q35-software-desktop/etc/thekernel-graphics-flavor").strip(),
            "q35-software-desktop",
        )
        self.assertIn('"$flavor"', init)
        self.assertIn("q35-software-desktop", session)
        self.assertIn("exec weston --config=/etc/weston/weston-drm.ini", session)

    def test_q35_smoke_sets_the_weston_session_environment_before_dropping_privileges(self) -> None:
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertIn("export HOME=/var/lib/weston", smoke)
        self.assertIn("export XDG_RUNTIME_DIR=/run/user/$weston_uid", smoke)
        self.assertIn("export WAYLAND_DISPLAY=wayland-0", smoke)
        self.assertIn('stat -c %u "$XDG_RUNTIME_DIR"', smoke)
        self.assertIn('[ "$(stat -c %a "$XDG_RUNTIME_DIR")" = 700 ]', smoke)
        self.assertIn("runtime_dir_is_private ||", smoke)
        self.assertIn('start-stop-daemon -S -x /usr/local/bin/q35-wayland-color-client -c weston', smoke)

    def test_q35_virgl_smoke_requires_the_render_node_and_forces_the_driver(self) -> None:
        fragment = self.read("q35-software-desktop.fragment")
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        client = self.read("q35-wayland-color-client.c")
        build = self.read("build-q35-wayland-client.sh")
        self.assertIn("BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_SWRAST=y", fragment)
        self.assertIn("BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_VIRGL=y", fragment)
        self.assertIn("/dev/dri/renderD128", smoke)
        self.assertIn('[ -c /dev/dri/renderD128 ] || exit 0', smoke)
        self.assertIn("MESA_LOADER_DRIVER_OVERRIDE=virgl", smoke)
        self.assertIn("THEKERNEL_Q35_VIRGL_READY", smoke)
        self.assertIn("THEKERNEL_Q35_VIRGL_READY", client)
        self.assertIn("wl_surface_frame", client)
        self.assertLess(client.index("wl_surface_frame"), client.index("eglSwapBuffers"))
        self.assertNotIn("wl_surface_commit(a->s); return 0", client)
        self.assertIn("-Wl,-z,defs", build)
        self.assertIn("-lwayland-egl", build)
        self.assertIn("-lEGL", build)
        self.assertIn("-lGLESv2", build)

    def test_q35_smoke_closes_the_eudev_input_classification_chain(self) -> None:
        smoke = self.read("overlay/q35-software-desktop/etc/init.d/S90q35-weston-smoke")
        self.assertIn('udevadm info -q property -n "$event"', smoke)
        self.assertIn("ID_INPUT=1", smoke)
        self.assertIn("ID_INPUT_(KEY|MOUSE)=1", smoke)
        self.assertIn("reason=input_classification", smoke)

    def test_product_cli_accepts_an_existing_graphics_rootfs(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_graphics_cli", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "run", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
        ])
        self.assertEqual(args.rootfs, "/tmp/graphics-rootfs.ext2")

    def test_graphics_smoke_configures_one_marker_for_qmp_and_stop(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_graphics_smoke", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda *_args, **kwargs: calls.update(kwargs) or 0
        original_is_file = pathlib.Path.is_file
        try:
            pathlib.Path.is_file = lambda _self: True
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        finally:
            pathlib.Path.is_file = original_is_file
        self.assertEqual(calls["stop_after_marker"], "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(calls["qmp_screenshot_after_marker"], "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(calls["graphics_profile"], "headless")

    def test_q35_headless_graphics_smoke_keeps_the_software_marker_and_pixel_oracle(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_q35_headless_smoke", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-software-desktop",
            "--graphics-profile", "headless",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda *_args, **kwargs: calls.update(kwargs) or 0
        original_is_file = pathlib.Path.is_file
        try:
            pathlib.Path.is_file = lambda _self: True
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        finally:
            pathlib.Path.is_file = original_is_file
        self.assertEqual(calls["stop_after_marker"], "THEKERNEL_Q35_WESTON_READY")
        self.assertEqual(calls["qmp_screenshot_after_marker"], "THEKERNEL_Q35_WESTON_READY")
        self.assertEqual(calls["qmp_screenshot_size"], (800, 600))
        self.assertEqual(calls["qmp_screenshot_color_blocks"][0].rgb, (255, 0, 0))

    def test_virgl_headless_graphics_smoke_is_rejected_without_a_qmp_pixel_oracle(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_virgl_headless_smoke", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-software-desktop",
        ])
        args.graphics_profile = "virgl-headless"
        original_is_file = pathlib.Path.is_file
        try:
            pathlib.Path.is_file = lambda _self: True
            with self.assertRaisesRegex(module.ProductError, "no QMP pixel-oracle surface"):
                module.graphics_smoke_cmd(args)
        finally:
            pathlib.Path.is_file = original_is_file

    def test_virgl_graphics_smoke_uses_the_virgl_marker_and_pixel_oracle(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_virgl_smoke", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        args = module.build_parser().parse_args([
            "graphics-smoke", "--no-build", "--rootfs", "/tmp/graphics-rootfs.ext2",
            "--screenshot", "/tmp/graphics.ppm", "--flavor", "q35-software-desktop",
            "--graphics-profile", "virgl-interactive",
        ])
        calls: dict[str, object] = {}
        module.run_product = lambda *_args, **kwargs: calls.update(kwargs) or 0
        original_is_file = pathlib.Path.is_file
        try:
            pathlib.Path.is_file = lambda _self: True
            self.assertEqual(module.graphics_smoke_cmd(args), 0)
        finally:
            pathlib.Path.is_file = original_is_file
        self.assertEqual(calls["stop_after_marker"], "THEKERNEL_Q35_VIRGL_READY")
        self.assertEqual(calls["qmp_screenshot_after_marker"], "THEKERNEL_Q35_VIRGL_READY")
        self.assertEqual(calls["qmp_screenshot_size"], (800, 600))
        self.assertEqual(calls["qmp_screenshot_color_blocks"][0].rgb, (255, 0, 0))

    def test_marker_gated_screenshot_is_forwarded_to_qemu_runner(self) -> None:
        spec = importlib.util.spec_from_file_location("thekernel_graphics_qmp", ROOT / "tools/thekernel.py")
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            artifacts = module.Artifacts(root / "state", module.Variant(cpus=1, memory="128M"))
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
                    "returncode": 75, "log_path": config.log_path, "intentionally_stopped": True,
                    "guest_clean_shutdown": False,
                })()

            module.run = fake_run
            self.assertEqual(module.run_product(
                artifacts, accel="tcg", timeout=30, workdir=root / "run", interactive=False,
                input_after_marker=None, stop_after_marker="THEKERNEL_GRAPHICS_ABI_SMOKE_READY",
                commands=None, extra_block=None, rootfs=rootfs, qmp_screenshot=screenshot,
                qmp_screenshot_after_marker="THEKERNEL_GRAPHICS_ABI_SMOKE_READY",
            ), 0)
        config = seen["config"]
        self.assertEqual(config.qmp.screenshot, screenshot.resolve())
        self.assertEqual(config.qmp.screenshot_after_marker, "THEKERNEL_GRAPHICS_ABI_SMOKE_READY")
        self.assertEqual(config.qmp.socket.name, "graphics-smoke.qmp")


if __name__ == "__main__":
    unittest.main()
