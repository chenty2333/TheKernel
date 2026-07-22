"""Command-line interface for the product-level QEMU runner."""

from __future__ import annotations

import argparse
import math
import sys
from pathlib import Path

from .command import CommandError
from .evidence import EvidenceError
from .images import ImageError
from .model import INTENTIONAL_STOP_RETURN_CODE, Interaction, RunLimits
from .process import ProcessError
from .receipt import ReceiptError, finalize_external_input_receipt
from .runner import RunConfig, RunnerError, normalize_arch, run


def _optional_timeout(value: float | None) -> float | None:
    if value is None or value == 0:
        return None
    if value < 0 or not math.isfinite(value):
        raise RunnerError("timeouts must be finite and non-negative")
    return value


def run_cmd(args: argparse.Namespace) -> int:
    arch = normalize_arch(args.arch)
    workdir = Path(args.workdir).expanduser() if args.workdir else Path(".state/qemu-runner") / arch
    log_path = Path(args.log).expanduser() if args.log else workdir / "console.log"
    cache_dir = (
        Path(args.image_cache).expanduser()
        if args.image_cache
        else Path(".state/qemu-runner/image-cache")
    )
    ready_timeout = _optional_timeout(args.ready_timeout)
    if args.input_after_marker is not None and ready_timeout is None:
        ready_timeout = 120.0
    config = RunConfig(
        arch=arch,
        kernel=Path(args.kernel),
        rootfs=Path(args.rootfs),
        rootfs_mode=args.rootfs_mode,
        extra_block=Path(args.extra_block) if args.extra_block else None,
        extra_block_mode=args.extra_block_mode,
        workdir=workdir,
        log_path=log_path,
        cache_dir=cache_dir,
        limits=RunLimits(
            total_timeout_secs=_optional_timeout(args.timeout),
            idle_timeout_secs=_optional_timeout(args.idle_timeout),
            ready_timeout_secs=ready_timeout,
        ),
        interaction=Interaction(
            interactive=args.interactive,
            input_after_marker=args.input_after_marker,
            stop_after_marker=args.stop_after_marker,
        ),
        memory=args.memory,
        cpus=args.cpus,
        qemu_binary=args.qemu_binary,
        receipt_path=Path(args.receipt) if args.receipt else None,
        external_input_producer=args.external_input_producer,
    )
    result = run(config)
    print(
        f"qemu-runner arch={result.arch} exit={result.returncode} "
        f"duration_ms={result.duration_ms} log={result.log_path}",
        file=sys.stderr,
    )
    if result.error_message:
        print(f"qemu-runner: {result.error_message}", file=sys.stderr)
    if result.interrupted:
        return 130
    if result.timed_out:
        return 124
    if result.intentionally_stopped:
        return INTENTIONAL_STOP_RETURN_CODE
    if result.returncode < 0:
        return 1
    return result.returncode


def finalize_input_cmd(args: argparse.Namespace) -> int:
    accepted = finalize_external_input_receipt(
        receipt_path=Path(args.receipt),
        commands_path=Path(args.commands),
        expected_sha256=args.expected_sha256,
        expected_bytes=args.expected_bytes,
        expected_line_count=args.expected_line_count,
        producer_status=args.producer_status,
    )
    state = "accepted" if accepted else "rejected"
    print(
        f"qemu-runner input-receipt={state} producer_status={args.producer_status} "
        f"receipt={Path(args.receipt).expanduser().resolve()}",
        file=sys.stderr,
    )
    return 0 if accepted else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="python3 -m tools.qemu_runner")
    subparsers = parser.add_subparsers(dest="command", required=True)
    run_parser = subparsers.add_parser("run", help="boot one explicit kernel and root filesystem")
    run_parser.add_argument(
        "--arch",
        required=True,
        choices=("rv", "la", "riscv64", "loongarch64"),
    )
    run_parser.add_argument("--kernel", required=True, help="kernel ELF or platform boot image")
    run_parser.add_argument(
        "--rootfs",
        required=True,
        help="raw root filesystem image, optionally .xz/.gz",
    )
    run_parser.add_argument("--extra-block", help="optional additional raw block image")
    run_parser.add_argument(
        "--rootfs-mode",
        choices=("snapshot", "readonly", "rw"),
        default="snapshot",
    )
    run_parser.add_argument(
        "--extra-block-mode",
        choices=("snapshot", "readonly", "rw"),
        default="rw",
    )
    run_parser.add_argument("--workdir", help="run directory; default .state/qemu-runner/ARCH")
    run_parser.add_argument("--log", help="serial log; default WORKDIR/console.log")
    run_parser.add_argument("--image-cache", help="compressed-image cache directory")
    run_parser.add_argument(
        "--timeout",
        type=float,
        default=300.0,
        help="total seconds; zero disables",
    )
    run_parser.add_argument(
        "--idle-timeout",
        type=float,
        default=0.0,
        help="seconds without console output; zero disables",
    )
    run_parser.add_argument(
        "--ready-timeout",
        type=float,
        default=0.0,
        help="seconds to wait for --input-after-marker; default 120, zero selects default",
    )
    run_parser.add_argument(
        "--interactive",
        action="store_true",
        help="mirror serial output and accept stdin",
    )
    run_parser.add_argument(
        "--input-after-marker",
        help="forward stdin only after this exact console line",
    )
    run_parser.add_argument(
        "--stop-after-marker",
        help="kill QEMU after this exact console line and return 75",
    )
    run_parser.add_argument("--memory", default="1G")
    run_parser.add_argument("--cpus", type=int, default=1)
    run_parser.add_argument("--qemu-binary", help="explicit QEMU executable")
    run_parser.add_argument(
        "--receipt",
        help="write an atomic JSON receipt before launch and after completion",
    )
    run_parser.add_argument(
        "--external-input-producer",
        action="store_true",
        help="leave the receipt awaiting a wrapper-recorded stdin producer status",
    )
    run_parser.set_defaults(func=run_cmd)

    finalize_parser = subparsers.add_parser(
        "finalize-input",
        help="atomically bind an external stdin producer status to a QEMU receipt",
    )
    finalize_parser.add_argument("--receipt", required=True)
    finalize_parser.add_argument("--commands", required=True)
    finalize_parser.add_argument("--expected-sha256", required=True)
    finalize_parser.add_argument("--expected-bytes", required=True, type=int)
    finalize_parser.add_argument("--expected-line-count", required=True, type=int)
    finalize_parser.add_argument("--producer-status", required=True, type=int)
    finalize_parser.set_defaults(func=finalize_input_cmd)
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
        return int(args.func(args))
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130
    except (
        RunnerError,
        EvidenceError,
        ImageError,
        CommandError,
        ProcessError,
        ReceiptError,
        OSError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
