"""Keep the real graphics PR gate from regressing into a config-only check."""

from __future__ import annotations

import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]


class GraphicsCiGateTests(unittest.TestCase):
    def test_product_job_builds_and_smoke_tests_the_q35_desktop_image(self) -> None:
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")

        self.assertIn("timeout-minutes: 180", workflow)
        self.assertIn("actions/cache@5a3ec84eff668545956fd18022155c47e93e2684", workflow)
        self.assertIn("--flavor q35-software-desktop", workflow)
        self.assertIn("--fetch-buildroot", workflow)
        self.assertIn("--buildroot-dir \"$graphics_state/buildroot/buildroot-2025.02.2\"", workflow)
        self.assertIn("--download-dir \"$PWD/.state/ci/graphics-downloads\"", workflow)
        self.assertIn("--rootfs \"$graphics_state/q35-software-desktop/images/rootfs.ext2\"", workflow)
        self.assertIn("--machine q35 --firmware uefi --smp 4", workflow)
        self.assertIn("--no-build --accel tcg --timeout 300", workflow)
        self.assertIn("--graphics-profile headless", workflow)
        self.assertIn("--screenshot \"$graphics_state/q35-software-desktop.ppm\"", workflow)


if __name__ == "__main__":
    unittest.main()
