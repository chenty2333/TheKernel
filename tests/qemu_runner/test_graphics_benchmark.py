from __future__ import annotations

import unittest

from tools.qemu_runner.graphics_benchmark import (
    BENCHMARK_INPUT_HOTPLUG_READY_MARKER,
    BENCHMARK_INPUT_SAMPLES,
    BENCHMARK_READY_MARKER,
    benchmark_checkpoints,
    renderer_for_profile,
)


class GraphicsBenchmarkProtocolTests(unittest.TestCase):
    def test_normal_protocol_has_one_hundred_sequential_latency_samples(self) -> None:
        checkpoints = benchmark_checkpoints()
        self.assertEqual(len(checkpoints), BENCHMARK_INPUT_SAMPLES)
        self.assertEqual(checkpoints[0].input_after_marker, BENCHMARK_READY_MARKER)
        for index, checkpoint in enumerate(checkpoints):
            self.assertEqual(checkpoint.latency_index, index)
            self.assertEqual(
                checkpoint.latency_after_marker,
                f"THEKERNEL_GRAPHICS_INPUT_VISIBLE_{index:03d}",
            )

    def test_hotplug_protocol_gates_latency_after_guest_reenumeration(self) -> None:
        checkpoints = benchmark_checkpoints("input-hotplug")
        self.assertEqual(len(checkpoints), BENCHMARK_INPUT_SAMPLES + 2)
        self.assertEqual(checkpoints[2].input_after_marker, BENCHMARK_INPUT_HOTPLUG_READY_MARKER)
        self.assertEqual(checkpoints[2].latency_index, 0)
        self.assertTrue(checkpoints[2].input_events)
        self.assertTrue(all(
            event["type"] == "abs"
            for batch in checkpoints[2].input_events
            for event in batch
        ))

    def test_profiles_require_exact_guest_renderers(self) -> None:
        self.assertEqual(renderer_for_profile("headless"), "software")
        self.assertEqual(renderer_for_profile("virgl-headless"), "virgl")
        self.assertEqual(renderer_for_profile("virgl-interactive"), "virgl")
        self.assertEqual(renderer_for_profile("venus-interactive"), "venus")
        with self.assertRaisesRegex(ValueError, "unsupported"):
            renderer_for_profile("interactive")


if __name__ == "__main__":
    unittest.main()
