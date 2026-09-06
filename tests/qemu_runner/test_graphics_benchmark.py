from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

from tools.qemu_runner.graphics_benchmark import (
    BENCHMARK_INPUT_HOTPLUG_READY_MARKER,
    BENCHMARK_INPUT_SAMPLES,
    BENCHMARK_READY_MARKER,
    benchmark_checkpoints,
    renderer_for_profile,
)
from tools.qemu_runner.profiles import BENCHMARK_FAULTS, INPUT_SAMPLES


REPO_ROOT = Path(__file__).resolve().parents[2]


class GraphicsBenchmarkProtocolTests(unittest.TestCase):
    def test_hotplug_hook_emits_exact_runner_markers(self) -> None:
        script = (REPO_ROOT / "config/graphics/overlay/q35-graphics-benchmark/etc/init.d/S90q35-graphics-benchmark").read_text()
        # Execute the hook's success emissions, including diagnostic lines,
        # against the runner's exact-line protocol rather than prefix matching.
        lines = [line.strip() for line in script.splitlines()
                 if line.strip().startswith('echo "$hotplug_') and "reason=" not in line]
        assignments = '\n'.join(line for line in script.splitlines()
                                if line.startswith(("hotplug_removed_marker=", "hotplug_ready_marker=")))
        result = subprocess.run(["sh", "-c", assignments + '\nold_event=/dev/input/event1\nevent=/dev/input/event2\n' + '\n'.join(lines)],
                                check=True, text=True, capture_output=True)
        checkpoints = benchmark_checkpoints("input-hotplug")
        for checkpoint in checkpoints[1:3]:
            self.assertIn(checkpoint.input_after_marker, result.stdout.splitlines())

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

    def test_fault_matrix_and_input_samples_are_single_sourced(self) -> None:
        self.assertEqual(BENCHMARK_FAULTS, frozenset({
            "modeset", "client-crash", "vt-switch", "weston-restart", "input-hotplug",
        }))
        self.assertEqual(INPUT_SAMPLES, 10)
        self.assertEqual(BENCHMARK_INPUT_SAMPLES, INPUT_SAMPLES)

    def test_list_faults_prints_the_fault_matrix_as_json(self) -> None:
        completed = subprocess.run(
            [sys.executable, "-m", "tools.qemu_runner.graphics_benchmark", "--list-faults"],
            cwd=REPO_ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
        self.assertEqual(set(json.loads(completed.stdout)), set(BENCHMARK_FAULTS))


if __name__ == "__main__":
    unittest.main()
