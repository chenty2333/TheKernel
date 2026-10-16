from __future__ import annotations

import json
import subprocess
import tempfile
from tests.support import test_tmpdir
import unittest
from pathlib import Path

from tools.qemu_runner.graphics_metrics import (
    GraphicsMetricError,
    INPUT_SAMPLES,
    SAMPLE_FRAMES,
    WARMUP_FRAMES,
    parse_graphics_metrics,
    enforce_graphics_metrics,
)


class GraphicsMetricsTests(unittest.TestCase):
    def write_log(
        self,
        root: Path,
        *,
        input_indexes: range,
        resources: tuple[dict, dict] = ({"weston_fd": 8}, {"weston_fd": 8}),
    ) -> Path:
        log = root / "console.log"
        events = [
            {"kind": "resources", "values": resources[0]},
            *(
                {"kind": "frame", "index": index, "ns": 16_666_667}
                for index in range(WARMUP_FRAMES, WARMUP_FRAMES + SAMPLE_FRAMES)
            ),
            *(
                {"kind": "input_to_visible", "index": index, "ns": 20_000_000}
                for index in input_indexes
            ),
            {"kind": "resources", "values": resources[1]},
        ]
        log.write_text(
            "THEKERNEL_GRAPHICS_RENDERER software\n"
            + "THEKERNEL_GRAPHICS_FAULT none\n"
            + "".join(
                "THEKERNEL_GRAPHICS_METRIC " + json.dumps(event) + "\n"
                for event in events
            ) + "THEKERNEL_GRAPHICS_BENCHMARK_COMPLETE\n",
            encoding="utf-8",
        )
        return log

    def test_requires_complete_host_input_to_visible_series(self) -> None:
        with test_tmpdir() as directory:
            metrics = parse_graphics_metrics(
                self.write_log(Path(directory), input_indexes=range(INPUT_SAMPLES))
            )
        self.assertEqual(metrics.frames, SAMPLE_FRAMES)
        self.assertAlmostEqual(metrics.input_p99_ms, 20.0)
        self.assertEqual(metrics.renderer, "software")

    def test_rejects_wrong_fault_oracle_and_missing_fault_identity(self) -> None:
        with test_tmpdir() as directory:
            log = self.write_log(Path(directory), input_indexes=range(INPUT_SAMPLES))
            metrics = parse_graphics_metrics(log)
            with self.assertRaisesRegex(GraphicsMetricError, "does not match requested"):
                enforce_graphics_metrics(metrics, expected_fault="client-crash")
            log.write_text(log.read_text().replace("THEKERNEL_GRAPHICS_FAULT none\n", ""))
            with self.assertRaisesRegex(GraphicsMetricError, "fault marker"):
                parse_graphics_metrics(log)

    def test_rejects_incomplete_or_replayed_transcript(self) -> None:
        with test_tmpdir() as directory:
            log = self.write_log(Path(directory), input_indexes=range(INPUT_SAMPLES))
            contents = log.read_text()
            for broken in (contents.replace("THEKERNEL_GRAPHICS_BENCHMARK_COMPLETE\n", ""),
                           contents + "THEKERNEL_GRAPHICS_BENCHMARK_COMPLETE\n"):
                log.write_text(broken)
                with self.assertRaisesRegex(GraphicsMetricError, "completion marker"):
                    parse_graphics_metrics(log)

    def test_crash_diagnostics_do_not_replay_live_protocol(self) -> None:
        script = (Path(__file__).resolve().parents[2] / "config/graphics/overlay/q35-graphics-benchmark/etc/init.d/S90q35-graphics-benchmark").read_text()
        command = next(line.strip() for line in script.splitlines() if line.strip().startswith("sed ") and '"$crash_log"' in line)
        with test_tmpdir() as directory:
            root = Path(directory)
            crash = root / "crash.log"
            crash.write_text('THEKERNEL_GRAPHICS_BENCHMARK_READY\nTHEKERNEL_GRAPHICS_METRIC {"kind":"frame","index":60,"ns":1}\n')
            result = subprocess.run(["sh", "-c", 'crash_log=$1\n' + command, "sh", str(crash)], check=True, capture_output=True, text=True)
            self.assertNotIn("THEKERNEL_GRAPHICS_BENCHMARK_READY", result.stdout.splitlines())
            log = self.write_log(root, input_indexes=range(INPUT_SAMPLES))
            log.write_text(result.stdout + log.read_text())
            self.assertEqual(parse_graphics_metrics(log).frames, SAMPLE_FRAMES)

    def test_rejects_missing_host_input_sample(self) -> None:
        with test_tmpdir() as directory:
            log = self.write_log(Path(directory), input_indexes=range(INPUT_SAMPLES - 1))
            with self.assertRaisesRegex(GraphicsMetricError, f"exactly {INPUT_SAMPLES}"):
                parse_graphics_metrics(log)

    def test_rejects_noncontiguous_host_input_index(self) -> None:
        with test_tmpdir() as directory:
            log = self.write_log(Path(directory), input_indexes=range(1, INPUT_SAMPLES + 1))
            with self.assertRaisesRegex(GraphicsMetricError, "expected 0, got 1"):
                parse_graphics_metrics(log)

    def test_accepts_schema_two_input_counters_without_drm_keys(self) -> None:
        schema_two = {"schema": 2, "input_devices": 3, "input_clients": 1}
        with test_tmpdir() as directory:
            log = self.write_log(
                Path(directory),
                input_indexes=range(INPUT_SAMPLES),
                resources=(schema_two, {**schema_two, "input_clients": 2}),
            )
            metrics = parse_graphics_metrics(log)
        self.assertEqual(metrics.resource_growth, {})
        self.assertEqual(metrics.resource_backlog_growth, {})
        self.assertEqual(metrics.monotonic_backlogs, ())
        self.assertEqual(metrics.resource_final["input_clients"], 2)
        enforce_graphics_metrics(metrics)

    def test_rejects_unknown_resource_schema(self) -> None:
        with test_tmpdir() as directory:
            log = self.write_log(
                Path(directory),
                input_indexes=range(INPUT_SAMPLES),
                resources=({"schema": 3}, {"schema": 3}),
            )
            with self.assertRaisesRegex(GraphicsMetricError, "unsupported graphics resource schema: 3"):
                parse_graphics_metrics(log)

    def test_schema_one_backlog_growth_still_fails_the_gate(self) -> None:
        pre = {"schema": 1, "atomic_commits": 10, "fences_pending": 0}
        post = {"schema": 1, "atomic_commits": 900, "fences_pending": 4}
        with test_tmpdir() as directory:
            log = self.write_log(
                Path(directory), input_indexes=range(INPUT_SAMPLES), resources=(pre, post)
            )
            metrics = parse_graphics_metrics(log)
        self.assertEqual(metrics.resource_backlog_growth, {"fences_pending": 4})
        with self.assertRaisesRegex(GraphicsMetricError, "resource backlog growth: fences_pending"):
            enforce_graphics_metrics(metrics)


if __name__ == "__main__":
    unittest.main()
