from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from tools.qemu_runner.graphics_metrics import (
    GraphicsMetricError,
    INPUT_SAMPLES,
    SAMPLE_FRAMES,
    WARMUP_FRAMES,
    parse_graphics_metrics,
)


class GraphicsMetricsTests(unittest.TestCase):
    def write_log(self, root: Path, *, input_indexes: range) -> Path:
        log = root / "console.log"
        events = [
            {"kind": "resources", "values": {"weston_fd": 8}},
            *(
                {"kind": "frame", "index": index, "ns": 16_666_667}
                for index in range(WARMUP_FRAMES, WARMUP_FRAMES + SAMPLE_FRAMES)
            ),
            *(
                {"kind": "input_to_visible", "index": index, "ns": 20_000_000}
                for index in input_indexes
            ),
            {"kind": "resources", "values": {"weston_fd": 8}},
        ]
        log.write_text(
            "THEKERNEL_GRAPHICS_RENDERER software\n"
            + "".join(
                "THEKERNEL_GRAPHICS_METRIC " + json.dumps(event) + "\n"
                for event in events
            ),
            encoding="utf-8",
        )
        return log

    def test_requires_complete_host_input_to_visible_series(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            metrics = parse_graphics_metrics(
                self.write_log(Path(directory), input_indexes=range(INPUT_SAMPLES))
            )
        self.assertEqual(metrics.frames, SAMPLE_FRAMES)
        self.assertAlmostEqual(metrics.input_p99_ms, 20.0)
        self.assertEqual(metrics.renderer, "software")

    def test_rejects_missing_host_input_sample(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = self.write_log(Path(directory), input_indexes=range(INPUT_SAMPLES - 1))
            with self.assertRaisesRegex(GraphicsMetricError, f"exactly {INPUT_SAMPLES}"):
                parse_graphics_metrics(log)

    def test_rejects_noncontiguous_host_input_index(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            log = self.write_log(Path(directory), input_indexes=range(1, INPUT_SAMPLES + 1))
            with self.assertRaisesRegex(GraphicsMetricError, "expected 0, got 1"):
                parse_graphics_metrics(log)


if __name__ == "__main__":
    unittest.main()
