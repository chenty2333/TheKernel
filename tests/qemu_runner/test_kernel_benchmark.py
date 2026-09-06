from __future__ import annotations

from dataclasses import replace
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch

from tests.support import test_tmpdir
from tools.qemu_runner.kernel_benchmark import (
    BenchmarkConfig, BenchmarkTarget, COMPLETE_MARKER, PRESSURES,
    paired_improvement, parse_benchmark_log, run_benchmark_experiment, _metrics, _benchmark_commands, SHELL_MARKER,
)
from tools.qemu_runner.model import RunResult
from tools.qemu_runner.runner import RunnerError


def scheduler_rows(iterations=32, workers=1):
    rows = []
    for pressure in PRESSURES:
        kinds = []
        if pressure in {"cpu", "mixed"}:
            kinds += ["lcg_65536"] * workers
        if pressure in {"io", "mixed"}:
            kinds += ["write_fsync_read_4k"] * workers
        rows.append({
            "suite": "scheduler", "workload": "pipe_handoff", "pressure": pressure,
            "iterations": iterations, "elapsed_ns": 32000, "workers_per_kind": workers,
            "wake_trace": {"scope": "handoff_child", "clock": "monotonic", "samples": 32,
                           "wake_to_run_p50_ns": 50, "wake_to_run_p95_ns": 100, "wake_to_run_p99_ns": 150},
            "handoff_p50_ns": 100, "handoff_p95_ns": 200, "handoff_p99_ns": 300,
            "background_cpu_workers": kinds.count("lcg_65536"),
            "background_io_workers": kinds.count("write_fsync_read_4k"),
            "zero_progress_workers": 0, "pressure_elapsed_ns": 250000000,
            "measurement": {"scope": "foreground_caller", "cpu_user_ns": 1000,
                            "cpu_system_ns": 2000, "voluntary_switches": 32,
                            "involuntary_switches": 0, "cpu_migrations": 0, "maxrss_kib": 1024},
            "workers": [{"kind": kind, "units": 100, "units_per_second": 400, "max_progress_gap_ns": 1000000}
                        for kind in kinds],
        })
    return rows


def write_log(path, rows, version="7.2.3", complete=True):
    path.write_text(f"Linux version {version} (builder)\n" +
                    "\n".join(json.dumps(row, separators=(",", ":")) for row in rows) +
                    (f"\n{COMPLETE_MARKER}\n" if complete else "\n"))


class KernelBenchmarkTests(unittest.TestCase):
    def setUp(self):
        validation = patch("tools.qemu_runner.kernel_benchmark.validate_linux_esp_kernel")
        self.validate_linux_boot = validation.start()
        self.addCleanup(validation.stop)
        tk_validation = patch("tools.qemu_runner.kernel_benchmark.validate_thekernel_esp_kernel")
        self.validate_tk_boot = tk_validation.start()
        self.addCleanup(tk_validation.stop)

    def test_parser_rejects_partial_duplicate_and_starved_pressure(self):
        with test_tmpdir() as temporary:
            log = Path(temporary) / "console.log"
            rows = scheduler_rows()
            write_log(log, rows)
            self.assertEqual(len(parse_benchmark_log(log, "scheduler", 32, linux=True, workers=1)), 4)
            for invalid in (rows[:-1], rows + rows[:1]):
                write_log(log, invalid)
                with self.assertRaises(RunnerError):
                    parse_benchmark_log(log, "scheduler", 32)
            rows[1]["zero_progress_workers"] = 1
            write_log(log, rows)
            with self.assertRaisesRegex(RunnerError, "no progress"):
                parse_benchmark_log(log, "scheduler", 32)

    def test_measurements_accept_zero_counts_but_reject_missing_or_invalid_fields(self):
        with test_tmpdir() as temporary:
            log = Path(temporary) / "console.log"
            for field, invalid in (("cpu_migrations", -1), ("cpu_system_ns", True), ("scope", "whole_guest")):
                rows = scheduler_rows()
                rows[0]["measurement"][field] = invalid
                write_log(log, rows)
                with self.assertRaisesRegex(RunnerError, "measurement"):
                    parse_benchmark_log(log, "scheduler", 32)
            rows = scheduler_rows()
            del rows[0]["measurement"]["cpu_migrations"]
            write_log(log, rows)
            with self.assertRaisesRegex(RunnerError, "measurement"):
                parse_benchmark_log(log, "scheduler", 32)
            rows = scheduler_rows()
            rows[1]["workers"][0]["max_progress_gap_ns"] = 250000001
            write_log(log, rows)
            with self.assertRaisesRegex(RunnerError, "gap"):
                parse_benchmark_log(log, "scheduler", 32)

    def test_parser_requires_workload_completion_and_exact_oracle_version(self):
        with test_tmpdir() as temporary:
            log = Path(temporary) / "console.log"
            write_log(log, scheduler_rows(), complete=False)
            with self.assertRaisesRegex(RunnerError, "marker"):
                parse_benchmark_log(log, "scheduler", 32)
            write_log(log, scheduler_rows(), version="7.2.30")
            with self.assertRaisesRegex(RunnerError, "7.2.3"):
                parse_benchmark_log(log, "scheduler", 32, linux=True)
            write_log(log, scheduler_rows())
            with self.assertRaisesRegex(RunnerError, "iteration"):
                parse_benchmark_log(log, "scheduler", 33)
            with self.assertRaisesRegex(RunnerError, "worker count"):
                parse_benchmark_log(log, "scheduler", 32, workers=4)

    def test_wake_trace_requires_exact_scope_clock_samples_and_order(self):
        with test_tmpdir() as temporary:
            log = Path(temporary) / "console.log"
            for field, value in (("clock", "realtime"), ("scope", "foreground_caller"),
                                 ("samples", 0), ("wake_to_run_p50_ns", 1000)):
                rows = scheduler_rows()
                rows[0]["wake_trace"][field] = value
                write_log(log, rows)
                with self.assertRaisesRegex(RunnerError, "wake trace"):
                    parse_benchmark_log(log, "scheduler", 32)

    def test_fairness_and_boundary_gap_are_derived_per_worker_kind(self):
        row = scheduler_rows(workers=2)[1]
        row["workers"][1]["units_per_second"] = 200
        row["workers"][1]["max_progress_gap_ns"] = 9000000
        metrics = _metrics(row)
        self.assertAlmostEqual(metrics["cpu_jain_fairness"][0], 0.9)
        self.assertEqual(metrics["cpu_max_progress_gap_ns"], (9000000, True))
        self.assertEqual(metrics["foreground_cpu_migrations"], (0, True))

    def test_io_primary_metric_uses_nanoseconds_per_operation(self):
        row = {"suite": "io", "elapsed_ns": 64000, "iterations": 32,
               "measurement": scheduler_rows()[0]["measurement"]}
        self.assertEqual(_metrics(row)["elapsed_ns_per_operation"], (2000, True))

    def test_paired_statistics_use_trial_unit_and_direction(self):
        change = paired_improvement([100.0] * 10, [80.0] * 10, smaller_better=True)
        self.assertAlmostEqual(change["improvement_percent"], 20)
        self.assertTrue(change["meets_10_percent_threshold"])
        self.assertFalse(change["point_regression_exceeds_5_percent"])
        throughput = paired_improvement([100.0] * 10, [80.0] * 10, smaller_better=False)
        self.assertAlmostEqual(throughput["improvement_percent"], -20)
        self.assertTrue(throughput["point_regression_exceeds_5_percent"])
        with self.assertRaises(RunnerError):
            paired_improvement([100.0] * 9, [80.0] * 9, smaller_better=True)

    @patch("tools.qemu_runner.kernel_benchmark.os.sched_getaffinity", return_value={0})
    @patch("tools.qemu_runner.kernel_benchmark.run")
    def test_boot_mismatch_fails_before_guest_start(self, run, _get):
        with test_tmpdir() as temporary:
            config = self.config(temporary)
            self.validate_linux_boot.side_effect = RunnerError("ESP kernel mismatch")
            with self.assertRaisesRegex(RunnerError, "ESP kernel mismatch"):
                run_benchmark_experiment(config)
            run.assert_not_called()

    def test_parser_rejects_replay_trailing_data_and_false_boot(self):
        with test_tmpdir() as temporary:
            log = Path(temporary) / "console.log"
            write_log(log, scheduler_rows())
            valid = log.read_text()
            invalid_logs = [
                valid + COMPLETE_MARKER + "\n",
                valid + json.dumps(scheduler_rows()[0], separators=(",", ":")) + "\n",
                valid + "THEKERNEL_BENCH_FAIL\n",
                valid.replace("Linux version 7.2.3", "echo Linux version 7.2.3"),
                valid.replace("Linux version 7.2.3", "Linux version 7.2.3-custom"),
                "Linux version 7.2.3 (reboot)\n" + valid,
            ]
            for invalid in invalid_logs:
                log.write_text(invalid)
                with self.assertRaises(RunnerError):
                    parse_benchmark_log(log, "scheduler", 32, linux=True)

    def config(self, directory, trials=10):
        root = Path(directory)
        for name in ("rootfs", "baseline-kernel", "baseline-esp", "linux-kernel", "linux-esp"):
            (root / name).write_bytes(b"unchanged source image")
        return BenchmarkConfig(
            targets=tuple(BenchmarkTarget(name, root / f"{name}-kernel", root / f"{name}-esp")
                          for name in ("baseline", "linux")),
            rootfs=root / "rootfs", workdir=root / "runs", suite="scheduler",
            iterations=32, trials=trials, cpus=1, host_cpus=(0,),
        )

    def test_uart_commands_fit_independent_prompt_lines_at_max_iterations(self):
        with test_tmpdir() as temporary:
            for suite in ("scheduler", "io", "all"):
                config = replace(self.config(temporary), suite=suite, iterations=1000000, cpus=4)
                commands = _benchmark_commands(config)
                self.assertTrue(all(len(line.encode()) + 1 < 128 for line in commands.splitlines()))
                self.assertNotIn("if ", commands)
                self.assertIn(f'[ "$failed" = 0 ] && echo {COMPLETE_MARKER}', commands)
                self.assertIn('|| failed=1', commands)

    @patch("tools.qemu_runner.kernel_benchmark.os.sched_getaffinity", return_value={0, 1, 2, 3})
    @patch("tools.qemu_runner.kernel_benchmark.os.sched_setaffinity")
    def test_runner_rotates_uses_identical_private_disks_and_restores_environment(self, affinity, _get):
        with test_tmpdir() as temporary:
            config = self.config(temporary)
            calls = []
            original_tmp = os.environ.get("TMPDIR")

            def fake_run(run_config, **kwargs):
                kwargs["console_stream"].write(b"binary UART data\n")
                calls.append(run_config)
                for original in config.targets:
                    original.kernel.write_bytes(b"concurrent rebuilt kernel")
                    original.esp.write_bytes(b"concurrent rebuilt ESP")
                self.assertEqual(run_config.kernel.read_bytes(), b"unchanged source image")
                self.assertEqual(run_config.esp.read_bytes(), b"unchanged source image")
                self.assertEqual(run_config.rootfs.read_bytes(), b"unchanged source image")
                self.assertEqual((run_config.rootfs_transport, run_config.rootfs_mode), ("drive", "rw"))
                self.assertEqual((run_config.accel, run_config.cpus, run_config.memory), ("kvm", 1, "4G"))
                self.assertEqual(run_config.graphics_profile, "headless")
                self.assertEqual(run_config.qmp.vcpu_host_cpus, (0,))
                self.assertEqual(run_config.qmp.socket, run_config.workdir / "qmp.sock")
                commands = run_config.input_path.read_text()
                self.assertIn("KERNEL_BENCH_WORKERS=1", commands)
                self.assertIn("/root/thekernel-bench.data", commands)
                self.assertNotIn("/var/tmp/", commands)
                self.assertTrue(all(len(line.encode()) + 1 < 128 for line in commands.splitlines()))
                self.assertEqual(run_config.interaction.input_line_after_marker, SHELL_MARKER)
                self.assertTrue(Path(os.environ["TMPDIR"]).is_relative_to(config.workdir))
                run_config.rootfs.write_bytes(b"guest modified disk")
                write_log(run_config.log_path, scheduler_rows())
                return RunResult(0, run_config.log_path, vcpu_affinity=((0, 12345, 0),))

            with patch("tools.qemu_runner.kernel_benchmark.run", side_effect=fake_run):
                report = run_benchmark_experiment(config)
            self.assertEqual(len(calls), 22)
            linux = next(target for target in config.targets if target.name == "linux")
            self.assertEqual(self.validate_linux_boot.call_args_list[0].args, (linux.kernel, linux.esp))
            self.assertEqual(self.validate_linux_boot.call_count, 2)
            self.assertEqual(self.validate_tk_boot.call_count, 2)
            self.assertTrue(all(not call.kernel.exists() and not call.esp.exists() for call in calls))
            self.assertEqual(report["order"][:3], [["baseline", "linux"], ["linux", "baseline"], ["baseline", "linux"]])
            self.assertEqual(report["discarded_warmup_rounds"], 1)
            self.assertTrue(all(not call.rootfs.exists() for call in calls))
            self.assertEqual(config.rootfs.read_bytes(), b"unchanged source image")
            self.assertFalse(list(config.workdir.rglob("rootfs-base.img")))
            self.assertEqual(os.environ.get("TMPDIR"), original_tmp)
            self.assertEqual(affinity.call_args.args, (0, {0, 1, 2, 3}))
            self.assertEqual(report["default_policy_selection"], "not_performed")
            zeros = [entry for entry in report["comparisons"] if entry["metric"] == "foreground_cpu_migrations"]
            self.assertTrue(zeros)
            self.assertTrue(all(entry["inference"] == "incomparable_counter_semantics" for entry in zeros))
            self.assertTrue(all("absolute_trial_changes" not in entry and "improvement_percent" not in entry for entry in zeros))
            self.assertTrue(report["configuration"]["per_vcpu_thread_pinning"])
            self.assertEqual(len(report["vcpu_affinity_runs"]), 22)

    @patch("tools.qemu_runner.kernel_benchmark.os.sched_getaffinity", return_value={0})
    @patch("tools.qemu_runner.kernel_benchmark.os.sched_setaffinity")
    def test_single_pair_is_smoke_without_inference_and_failure_cleans_disk(self, _set, _get):
        with test_tmpdir() as temporary:
            config = self.config(temporary, trials=1)
            calls = []

            def fake_run(run_config, **kwargs):
                calls.append(run_config)
                write_log(run_config.log_path, scheduler_rows())
                return RunResult(0, run_config.log_path, vcpu_affinity=((0, 12345, 0),))

            with patch("tools.qemu_runner.kernel_benchmark.run", side_effect=fake_run):
                report = run_benchmark_experiment(config)
            self.assertEqual(len(calls), 2)
            self.assertEqual(report["acceptance"], "smoke_only")
            self.assertEqual(report["discarded_warmup_rounds"], 0)
            self.assertTrue(all(c["inference"] in {"insufficient_trials", "incomparable_counter_semantics"} for c in report["comparisons"]))
            self.assertTrue(all("ci95_improvement_percent" not in c for c in report["comparisons"]))
            with patch("tools.qemu_runner.kernel_benchmark.run", side_effect=RunnerError("timeout")):
                with self.assertRaisesRegex(RunnerError, "timeout"):
                    run_benchmark_experiment(config)
            for failed in (
                RunResult(0, Path("unused"), error_message="timeout"),
                RunResult(-15, Path("unused")),
                RunResult(0, Path("unused"), runner_terminated=True),
                RunResult(0, Path("unused"), runner_termination_reason="total-timeout"),
            ):
                with patch("tools.qemu_runner.kernel_benchmark.run", return_value=failed):
                    with self.assertRaisesRegex(RunnerError, "benchmark guest failed"):
                        run_benchmark_experiment(config)
            self.assertFalse(list(config.workdir.rglob("*.input")))
            self.assertFalse(list(config.workdir.rglob("rootfs*.img")))
            with self.assertRaisesRegex(RunnerError, "outside tmpfs"):
                run_benchmark_experiment(replace(config, workdir=Path("/tmp/thekernel-benchmark")))


if __name__ == "__main__":
    unittest.main()
