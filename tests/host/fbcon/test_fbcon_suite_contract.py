#!/usr/bin/env python3
"""Contracts the fbcon acceptance suite makes against the tree it describes.

These are the host-side half of the suite: they fail before a boot can burn a
run discovering the same thing, and they pin the marker, the profile, the
artifact variant, and the post-run acceptance check.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from tests.support import load_script_module, repo_root, test_tmpdir
from tools.qemu_runner.model import QmpTextCells

ROOT = repo_root()
FBCON = ROOT / "tests/qemu_runner/test_fbcon_oracle.py"


def _helpers():
    """Reuse the oracle module's image builders rather than duplicating them."""

    return load_script_module("test_fbcon_oracle_helpers", "tests/qemu_runner/test_fbcon_oracle.py")


class FbconSuiteContractTests(unittest.TestCase):
    """The suite's declared marker and profile against the tree they describe."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_script_module("thekernel_product", "tools/thekernel.py")

    def test_fbcon_marker_is_what_the_guest_prints(self) -> None:
        """A marker the guest cannot print must fail on the host, not in QEMU.

        `screenshot_after_marker` compares a whole ANSI-stripped console line,
        so a stale marker burns an entire boot before anyone learns it was
        wrong.  The guest's first suite case is the source of that line.
        """

        source = (ROOT / "tests/guest/system-init.c").read_text(encoding="utf-8")
        begin = re.search(r'printf\("# THEKERNEL_TEST_BEGIN %zu %s timeout_seconds=%u\\n",'
                          r"\s*\n\s*index \+ 1, suite\[index\]\.name,"
                          r" suite\[index\]\.timeout_seconds\);", source)
        self.assertIsNotNone(begin, "the guest no longer prints the KTAP begin marker")
        first = re.search(r'\{\s*"(\w[\w-]*)",\s*\w+,\s*(\d+)\s*\},', source)
        self.assertIsNotNone(first, "the guest suite no longer declares cases")
        expected = f"# THEKERNEL_TEST_BEGIN 1 {first.group(1)} timeout_seconds={first.group(2)}"
        self.assertEqual(self.module.FBCON_MARKER, expected)
        # The runner's own gate must accept the literal, or the marker can
        # never fire however correct the guest is.
        self.assertIsNotNone(re.fullmatch(
            r"# THEKERNEL_TEST_BEGIN (\d+) (\S+) timeout_seconds=(\d+)",
            self.module.FBCON_MARKER,
        ))

    def test_fbcon_suite_boots_the_firmware_profile_without_a_build(self) -> None:
        """The suite's run must be the serial-less configuration, not a rebuild."""

        module = self.module
        calls: list[object] = []
        self.assertTrue(module.FBCON_PROFILE == "firmware-fb")
        from tools.qemu_runner.profiles import GRAPHICS_PROFILES
        topology = GRAPHICS_PROFILES[module.FBCON_PROFILE]
        # No virtio-gpu: a firmware framebuffer is the only scanout available.
        self.assertNotIn("virtio", topology.device)
        self.assertEqual((topology.display, topology.device), ("none", "bochs-display"))
        with test_tmpdir() as directory, \
             mock.patch.object(module, "build_kernel") as kernel, \
             mock.patch.object(module, "build_rootfs") as rootfs, \
             mock.patch.object(module, "run_product",
                               side_effect=lambda artifacts, spec: calls.append(spec) or 1):
            args = SimpleNamespace(
                accel="tcg", smp=4, memory="512M", profile="system", no_build=True,
                timeout=240.0, asid_fast_switch=False, m5_candidate=False,
                io_submit_batch=False, io_notify_fastpath=False, run_cpus=None,
                workdir=directory, graphics_profile="firmware-fb",
                screenshot=str(Path(directory) / "console.ppm"), qemu_debug=None,
            )
            self.assertEqual(module.fbcon_suite_cmd(args), 1)
            kernel.assert_not_called()
            rootfs.assert_not_called()
        spec = calls[0]
        self.assertEqual(spec.graphics_profile, "firmware-fb")
        self.assertEqual(spec.stop_after_marker, module.FBCON_MARKER)
        self.assertEqual(spec.qmp_screenshot_after_marker, module.FBCON_MARKER)
        self.assertEqual(spec.qmp_screenshot_text_cells, module.FBCON_TEXT_CELLS)
        self.assertIsNone(spec.qmp_screenshot_size)

    def test_fbcon_suite_refuses_a_display_it_cannot_assert_on(self) -> None:
        module = self.module
        args = SimpleNamespace(graphics_profile="headless", screenshot="out.ppm", profile="system")
        with self.assertRaisesRegex(module.ProductError, "requires --graphics-profile firmware-fb"):
            module.fbcon_suite_cmd(args)

    def test_fbcon_suite_refuses_a_guest_without_the_marker(self) -> None:
        """A shell guest never prints the KTAP line this suite gates on."""

        module = self.module
        args = SimpleNamespace(graphics_profile="firmware-fb", screenshot="out.ppm", profile="shell")
        with self.assertRaisesRegex(module.ProductError, "boots the system profile"):
            module.fbcon_suite_cmd(args)

    def test_missing_artifacts_name_the_memory_size_that_was_built(self) -> None:
        """A `make build` at its own default must not produce a confusing boot.

        Artifact directories are keyed by memory size, so the message has to
        say which size the run wanted.
        """

        module = self.module
        with test_tmpdir() as directory, \
             mock.patch.object(module, "state_root", return_value=Path(directory)):
            args = SimpleNamespace(smp=4, memory="512M", no_build=True,
                                   asid_fast_switch=False, m5_candidate=False,
                                   io_submit_batch=False, io_notify_fastpath=False)
            with self.assertRaisesRegex(module.ProductError,
                                        r"no mem512m kernel and ESP.*--memory 512m"):
                module.fbcon_artifacts(args)

    def test_marker_gate_reports_the_literal_line_it_waited_for(self) -> None:
        """A marker that never arrives must say which line was missing."""

        source = (ROOT / "tools/qemu_runner/process.py").read_text(encoding="utf-8")
        self.assertIn('f"QMP timeout waiting for {description}"', source)
        self.assertIn('f"screenshot marker: {checkpoint.screenshot_after_marker}"', source)


class FbconAcceptanceCheckTests(unittest.TestCase):
    """`_check_fbcon_run`: the assertions made after the guest has stopped."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.module = load_script_module("thekernel_product", "tools/thekernel.py")

    def run_check(self, directory: Path, *, console_lines: list[str], kernel_lines: list[str],
                  width: int = 16, height: int = 2, inked: int = 32) -> int:
        (directory / "console.log").write_text("\n".join(console_lines) + "\n", encoding="utf-8")
        (directory / "kernel.log").write_text("\n".join(kernel_lines) + "\n", encoding="utf-8")
        helpers = _helpers()
        image_width, image_height, pixels = helpers.console(width, height, inked_cells=inked)
        screenshot = directory / "console.ppm"
        screenshot.write_bytes(helpers.ppm(image_width, image_height, bytes(pixels)))
        artifacts = SimpleNamespace(variant=SimpleNamespace(name="mem512m"))
        return self.module._check_fbcon_run(directory, screenshot, artifacts)

    def test_accepts_a_kernel_log_drawn_on_the_surface_it_reported(self) -> None:
        with test_tmpdir() as temporary:
            directory = Path(temporary)
            with mock.patch.object(self.module, "FBCON_TEXT_CELLS",
                                   QmpTextCells(min_ink_pixels=1, min_inked_cells=1)):
                self.assertEqual(self.run_check(
                    directory,
                    console_lines=["KTAP version 1", self.module.FBCON_MARKER],
                    kernel_lines=["MB2 framebuffer: addr=0x80000000 128x32 bpp=32 pitch=512 "
                                  "len=0x3000 rgb=(8@16/8@8/8@0)"],
                ), 0)

    def test_rejects_a_run_whose_marker_never_arrived(self) -> None:
        with test_tmpdir() as temporary:
            with self.assertRaisesRegex(self.module.ProductError, "never saw its marker line"):
                self.run_check(Path(temporary), console_lines=["KTAP version 1"],
                               kernel_lines=["MB2 framebuffer: addr=0x80000000 128x32 bpp=32 pitch=512"])

    def test_rejects_a_declined_firmware_framebuffer_and_quotes_the_verdict(self) -> None:
        with test_tmpdir() as temporary:
            with self.assertRaisesRegex(self.module.ProductError,
                                        r"declined reason=UnusableAddress"):
                self.run_check(Path(temporary), console_lines=[self.module.FBCON_MARKER],
                               kernel_lines=["MB2 framebuffer: declined reason=UnusableAddress"])

    def test_rejects_a_boot_that_reported_no_framebuffer_at_all(self) -> None:
        with test_tmpdir() as temporary:
            with self.assertRaisesRegex(self.module.ProductError, "no framebuffer report"):
                self.run_check(Path(temporary), console_lines=[self.module.FBCON_MARKER],
                               kernel_lines=["MB2 tag inventory: protocol=Multiboot2 count=4 "
                                             "truncated=0"])

    def test_rejects_a_screendump_of_a_different_surface(self) -> None:
        with test_tmpdir() as temporary:
            with self.assertRaisesRegex(self.module.ProductError, "not the surface the kernel"):
                self.run_check(Path(temporary), console_lines=[self.module.FBCON_MARKER],
                               kernel_lines=["MB2 framebuffer: addr=0x80000000 1280x800 bpp=32 "
                                             "pitch=5120"])

    def test_rejects_a_pitch_too_small_for_the_reported_depth(self) -> None:
        with test_tmpdir() as temporary:
            with self.assertRaisesRegex(self.module.ProductError, "unusable firmware framebuffer"):
                self.run_check(Path(temporary), console_lines=[self.module.FBCON_MARKER],
                               kernel_lines=["MB2 framebuffer: addr=0x80000000 128x32 bpp=32 pitch=8"])


if __name__ == "__main__":
    unittest.main()
