"""Keep the real graphics PR gate from regressing into a config-only check."""

from __future__ import annotations

import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[3]


class GraphicsCiGateTests(unittest.TestCase):
    def test_product_job_hands_the_canonical_q35_seatd_image_to_the_pixel_oracle(self) -> None:
        workflow = (ROOT / ".github/workflows/ci.yml").read_text(encoding="utf-8")

        self.assertIn("timeout-minutes: 180", workflow)
        self.assertIn("actions/cache@5a3ec84eff668545956fd18022155c47e93e2684", workflow)
        self.assertIn("--flavor q35-graphics-seatd", workflow)
        self.assertIn("--fetch-buildroot", workflow)
        self.assertIn("--buildroot-dir \"$graphics_state/buildroot/buildroot-2026.05.2\"", workflow)
        self.assertIn("--download-dir \"$PWD/.state/ci/graphics-downloads\"", workflow)
        self.assertIn("--output \"$graphics_state/q35-graphics-seatd\"", workflow)
        self.assertIn('cp "$graphics_state/q35-graphics-seatd/images/rootfs.ext2" "$product_rootfs"', workflow)
        self.assertIn("--machine q35 --firmware uefi --smp 4", workflow)
        self.assertIn("--accel tcg --timeout 300", workflow)
        graphics_step = workflow[workflow.index("Run canonical Q35 seatd Pixman pixel oracle"):]
        graphics_step = graphics_step.split("panther-lake-dut:", 1)[0]
        self.assertNotIn("--no-build", graphics_step)
        self.assertIn('--rootfs "$THEKERNEL_STATE_DIR/out/rootfs/x86/rootfs-x86.img"', graphics_step)
        self.assertIn("--graphics-profile headless", workflow)
        self.assertIn("--screenshot \"$graphics_state/q35-graphics-seatd.ppm\"", workflow)
        self.assertIn("--workdir \"$graphics_state/q35-graphics-seatd-run\"", workflow)


if __name__ == "__main__":
    unittest.main()
