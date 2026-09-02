#!/usr/bin/env python3
"""Run the Linux 6.12.107 graphics benchmark through the product QEMU runner."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from tools.qemu_runner import Interaction, RunConfig, RunLimits, RunnerError, run
from tools.qemu_runner.graphics_benchmark import (
    BENCHMARK_COMPLETE_MARKER,
    BENCHMARK_FAULTS,
    benchmark_checkpoints,
    renderer_for_profile,
)
from tools.qemu_runner.graphics_metrics import (
    GraphicsMetricError,
    enforce_graphics_metrics,
    parse_graphics_metrics,
)
from tools.qemu_runner.model import QmpControls


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run the Linux 6.12.107 graphics oracle on the fixed Q35 topology"
    )
    parser.add_argument("--kernel", required=True, type=Path)
    parser.add_argument("--esp", required=True, type=Path)
    parser.add_argument("--rootfs", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument(
        "--graphics-profile",
        required=True,
        choices=("headless", "virgl-headless", "virgl-interactive", "venus-interactive"),
    )
    parser.add_argument("--fault", choices=tuple(sorted(BENCHMARK_FAULTS)))
    parser.add_argument("--timeout", type=float, default=1800.0)
    parser.add_argument("--qemu")
    args = parser.parse_args(argv)
    if args.timeout <= 0:
        parser.error("--timeout must be positive")

    output = args.output.expanduser().resolve()
    output.mkdir(parents=True, exist_ok=True)
    try:
        result = run(
            RunConfig(
                arch="x86_64",
                kernel=args.kernel,
                esp=args.esp,
                rootfs=args.rootfs,
                rootfs_transport="drive",
                rootfs_mode="snapshot",
                workdir=output,
                log_path=output / "console.log",
                limits=RunLimits(total_timeout_secs=args.timeout),
                interaction=Interaction(stop_after_marker=BENCHMARK_COMPLETE_MARKER),
                memory="4G",
                cpus=4,
                accel="kvm",
                qemu_binary=args.qemu,
                graphics_profile=args.graphics_profile,
                graphics_width=3840,
                graphics_height=2160,
                qmp=QmpControls(
                    socket=output / "graphics-linux-oracle.qmp",
                    checkpoints=benchmark_checkpoints(args.fault),
                    timeout_secs=300.0,
                ),
            )
        )
    except RunnerError as error:
        print(f"graphics-linux-oracle: {error}", file=sys.stderr)
        return 2
    if not result.intentionally_stopped:
        print(
            f"graphics-linux-oracle: QEMU did not reach {BENCHMARK_COMPLETE_MARKER}: "
            f"exit={result.returncode} log={result.log_path}",
            file=sys.stderr,
        )
        return 1
    try:
        metrics = parse_graphics_metrics(result.log_path)
        # Linux must pass the complete absolute policy before it can serve as
        # the relative baseline for TheKernel.
        enforce_graphics_metrics(
            metrics,
            expected_renderer=renderer_for_profile(args.graphics_profile),
        )
    except GraphicsMetricError as error:
        print(f"graphics-linux-oracle: {error}", file=sys.stderr)
        return 1
    print(metrics.json())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
