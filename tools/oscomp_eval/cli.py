"""CLI for the local OSComp evaluator."""

from __future__ import annotations

import argparse
import json
import sys
import traceback
from pathlib import Path

from .config import (
    ConfigError,
    JUDGE_TIMEOUT_SECS,
    REPLAY_TIMEOUT_FULL_SECS,
    canonical_arch,
    group_libc_matrix_from_plan,
)
from .replay import evaluate_replay
from .judge_runner import JudgeRunnerError, judge_log
from .markers import MarkerError, compatible_summary, parse_log, write_artifacts
from .provenance import ProvenanceError, refresh_official_snapshot
from .run_inspect import RunInspectError, inspect_run
from .score_logs import score_logs
from .support_image import SupportImageConfigError, SupportImageError, inspect_support_image


def _add_marker_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--log", required=True, help="console log to parse")
    parser.add_argument("--arch", default="", help="optional architecture label")
    parser.add_argument(
        "--require-conclusion",
        action="store_true",
        help="require a visible timeout/shutdown conclusion in the log text",
    )
    parser.add_argument(
        "--out",
        help="optional output directory for marker-validation.json and segments",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="print machine-readable marker validation JSON",
    )


def markers_cmd(args: argparse.Namespace) -> int:
    try:
        result = parse_log(
            Path(args.log).expanduser(),
            arch=args.arch,
            require_conclusion=args.require_conclusion,
        )
    except MarkerError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    if args.out:
        write_artifacts(result, Path(args.out).expanduser())

    if args.json:
        print(json.dumps(result.to_json_dict(include_bodies=False), indent=2, sort_keys=True))
    else:
        print(compatible_summary(result))

    return 1 if result.has_errors else 0


def judge_log_cmd(args: argparse.Namespace) -> int:
    try:
        arch = canonical_arch(args.arch)
    except ConfigError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    judge_dir = Path(args.judge_dir).expanduser() if args.judge_dir else None
    try:
        group_libc_matrix = _group_libc_matrix_from_args(args)
    except ConfigError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    try:
        summary = judge_log(
            log_path=Path(args.log).expanduser(),
            arch=arch,
            out_dir=Path(args.out).expanduser(),
            judge_dir=judge_dir,
            judge_timeout_secs=args.judge_timeout,
            fail_fast=args.fail_fast,
            group_libc_matrix=group_libc_matrix,
        )
    except (MarkerError, JudgeRunnerError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    print(
        "judge-log "
        f"arch={summary.arch} results={len(summary.results)} "
        f"ok={sum(1 for result in summary.results if result.ok)} "
        f"errors={sum(1 for result in summary.results if not result.ok)}"
    )
    for result in summary.results:
        if result.ok:
            continue
        print(f"  {result.group_id}: {result.status}")

    return 1 if summary.has_errors else 0


def _group_libc_matrix_from_args(args: argparse.Namespace):
    plan = getattr(args, "plan", None)
    if not plan:
        return None
    return group_libc_matrix_from_plan(Path(plan).expanduser())


def _run_score_logs_from_args(args: argparse.Namespace, *, label: str) -> int:
    if not args.rv_log and not args.la_log:
        print(f"error: {label} requires --rv-log and/or --la-log", file=sys.stderr)
        return 2

    judge_dir = Path(args.judge_dir).expanduser() if args.judge_dir else None
    run_dir = Path(args.out).expanduser() if args.out else None
    try:
        group_libc_matrix = _group_libc_matrix_from_args(args)
    except ConfigError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    try:
        result = score_logs(
            name=args.name,
            run_dir=run_dir,
            rv_log=Path(args.rv_log).expanduser() if args.rv_log else None,
            la_log=Path(args.la_log).expanduser() if args.la_log else None,
            judge_dir=judge_dir,
            judge_timeout_secs=args.judge_timeout,
            fail_fast=args.fail_fast,
            replace=args.replace,
            group_libc_matrix=group_libc_matrix,
        )
    except (ValueError, FileExistsError, MarkerError, JudgeRunnerError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    print(
        f"{label} "
        f"run_dir={result.run_dir} "
        f"status={result.status} "
        f"total={result.score.total_score:.6g} "
        f"issues={len(result.score.issues)}"
    )
    return 1 if result.score.has_errors else 0


def score_logs_cmd(args: argparse.Namespace) -> int:
    return _run_score_logs_from_args(args, label="score-logs")


def evaluate_exit_code(result: object) -> int:
    interrupted = bool(getattr(result, "interrupted", False))
    if interrupted:
        return 130

    timed_out = bool(getattr(result, "timed_out"))
    if timed_out:
        return 124

    replay_failures = int(getattr(result, "replay_failures"))
    judge_summaries = tuple(getattr(result, "judge_summaries"))
    if replay_failures and not judge_summaries:
        return 3

    score = getattr(result, "score")
    if bool(getattr(score, "has_errors")) or replay_failures:
        return 1
    return 0


def evaluate_cmd(args: argparse.Namespace) -> int:
    if args.rv_log or args.la_log:
        if args.ltp_list:
            print("error: --ltp-list only applies to replay-launch evaluate mode", file=sys.stderr)
            return 2
        if args.support_image:
            print("error: --support-image only applies to replay-launch evaluate mode", file=sys.stderr)
            return 2
        return _run_score_logs_from_args(args, label="evaluate")

    try:
        group_libc_matrix = _group_libc_matrix_from_args(args)
    except ConfigError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    try:
        result = evaluate_replay(
            name=args.name,
            arch=args.arch,
            run_dir=Path(args.out).expanduser() if args.out else None,
            timeout_secs=args.timeout
            if args.timeout is not None
            else REPLAY_TIMEOUT_FULL_SECS,
            idle_timeout_secs=args.idle_timeout,
            image=Path(args.image).expanduser() if args.image else None,
            support_image=Path(args.support_image).expanduser()
            if args.support_image
            else None,
            ltp_list=Path(args.ltp_list).expanduser() if args.ltp_list else None,
            plan_path=Path(args.plan).expanduser() if args.plan else None,
            skip_kernel_build=args.skip_kernel_build,
            keep_workdir=False,
            judge_dir=Path(args.judge_dir).expanduser() if args.judge_dir else None,
            judge_timeout_secs=args.judge_timeout,
            fail_fast=args.fail_fast,
            replace=args.replace,
            group_libc_matrix=group_libc_matrix,
            verbose=args.verbose,
        )
    except (
        ValueError,
        FileExistsError,
        OSError,
        MarkerError,
        JudgeRunnerError,
        SupportImageConfigError,
        SupportImageError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        if isinstance(error, (OSError, SupportImageError)) and not isinstance(
            error,
            SupportImageConfigError,
        ):
            return 3
        return 2

    print(
        "evaluate "
        f"run_dir={result.run_dir} "
        f"status={result.status} "
        f"total={result.score.total_score:.6g} "
        f"issues={len(result.score.issues)} "
        f"replay_failures={result.replay_failures}"
    )
    return evaluate_exit_code(result)


def inspect_run_cmd(args: argparse.Namespace) -> int:
    try:
        result = inspect_run(Path(args.run_dir).expanduser())
    except RunInspectError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(result.to_json_dict(), indent=2, sort_keys=True))
        return 0 if result.ok else 1

    print(
        "inspect-run "
        f"run_dir={result.run_dir} "
        f"status={result.run_status} "
        f"artifacts={result.artifact_count} "
        f"structural_issues={len(result.structural_issues)} "
        f"score_issues={result.score_issue_count}"
    )
    for issue in result.structural_issues:
        print(f"  - {issue}")
    return 0 if result.ok else 1


def official_refresh_cmd(args: argparse.Namespace) -> int:
    try:
        snapshot = refresh_official_snapshot(
            Path(args.source).expanduser(),
            repo=args.repo,
            commit=args.commit,
            allow_dirty=args.allow_dirty,
        )
    except ProvenanceError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    changes = snapshot.changes or {"added": (), "removed": (), "changed": ()}
    added_count = len(changes.get("added", ()))
    removed_count = len(changes.get("removed", ()))
    changed_count = len(changes.get("changed", ()))
    print(
        "official-refresh "
        f"source={snapshot.source_path} "
        f"commit={snapshot.commit or '<unknown>'} "
        f"status={snapshot.source_status or '<unknown>'} "
        f"files={len(snapshot.files)} "
        f"added={added_count} "
        f"removed={removed_count} "
        f"changed={changed_count}"
    )
    return 0


def support_check_cmd(args: argparse.Namespace) -> int:
    try:
        result = inspect_support_image(
            arch=args.arch,
            image=Path(args.image).expanduser(),
        )
    except SupportImageConfigError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(result.to_json_dict(), indent=2, sort_keys=True))
    else:
        status = "ok" if result.ok else "bad"
        print(f"support-check arch={result.arch} image={result.image} status={status}")
        for issue in result.issues:
            print(f"  - {issue}")
    return 0 if result.ok else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python3 -m tools.oscomp_eval",
        description="local OSComp evaluator utilities",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    markers_parser = subparsers.add_parser(
        "markers",
        help="parse and validate OSComp TEST GROUP markers",
    )
    _add_marker_args(markers_parser)
    markers_parser.set_defaults(func=markers_cmd)

    judge_log_parser = subparsers.add_parser(
        "judge-log",
        help="parse one log and run official-compatible judges",
    )
    judge_log_parser.add_argument("--arch", required=True, help="rv or la")
    judge_log_parser.add_argument("--log", required=True, help="console log to judge")
    judge_log_parser.add_argument(
        "--out",
        required=True,
        help="output directory for marker and judge artifacts",
    )
    judge_log_parser.add_argument(
        "--judge-dir",
        help="override official judge directory",
    )
    judge_log_parser.add_argument(
        "--plan",
        help="plan file defining the expected group/libc matrix for this log",
    )
    judge_log_parser.add_argument(
        "--judge-timeout",
        type=float,
        default=JUDGE_TIMEOUT_SECS,
        help="per-judge timeout in seconds",
    )
    judge_log_parser.add_argument(
        "--fail-fast",
        action="store_true",
        help="stop after the first missing segment or judge failure",
    )
    judge_log_parser.set_defaults(func=judge_log_cmd)

    score_logs_parser = subparsers.add_parser(
        "score-logs",
        help="run offline marker parsing, judging, and scoring from existing logs",
    )
    score_logs_parser.add_argument("--rv-log", help="RISC-V console log")
    score_logs_parser.add_argument("--la-log", help="LoongArch console log")
    score_logs_parser.add_argument(
        "--name",
        default="manual-score",
        help="run name when --out is not supplied",
    )
    score_logs_parser.add_argument(
        "--out",
        help="explicit run directory",
    )
    score_logs_parser.add_argument("--judge-dir", help="override official judge directory")
    score_logs_parser.add_argument(
        "--plan",
        help="plan file defining the expected group/libc matrix for these logs",
    )
    score_logs_parser.add_argument(
        "--judge-timeout",
        type=float,
        default=JUDGE_TIMEOUT_SECS,
        help="per-judge timeout in seconds",
    )
    score_logs_parser.add_argument(
        "--fail-fast",
        action="store_true",
        help="stop each arch after the first missing segment or judge failure",
    )
    score_logs_parser.add_argument(
        "--replace",
        action="store_true",
        help="allow replacing an existing run directory",
    )
    score_logs_parser.set_defaults(func=score_logs_cmd)

    evaluate_parser = subparsers.add_parser(
        "evaluate",
        help="local evaluation entrypoint",
    )
    evaluate_parser.add_argument("--rv-log", help="RISC-V console log")
    evaluate_parser.add_argument("--la-log", help="LoongArch console log")
    evaluate_parser.add_argument(
        "--arch",
        choices=("rv", "la", "both"),
        default="both",
        help="architecture selection for replay-launch mode",
    )
    evaluate_parser.add_argument("--timeout", type=int, help="whole-QEMU timeout in seconds")
    evaluate_parser.add_argument(
        "--idle-timeout",
        type=int,
        help="replay timeout in seconds since the last console-log write",
    )
    evaluate_parser.add_argument("--image", help="official testsuite image override")
    evaluate_parser.add_argument("--support-image", help="support disk image override")
    evaluate_parser.add_argument(
        "--ltp-list",
        help="build or reuse a content-addressed support image from this LTP list "
        "(stored under .state/build-cache/support-disks/)",
    )
    evaluate_parser.add_argument(
        "--plan",
        help="plan file defining the expected group/libc matrix for judging/scoring",
    )
    evaluate_parser.add_argument("--skip-kernel-build", action="store_true")
    evaluate_parser.add_argument(
        "--name",
        default="manual-evaluate",
        help="run name when --out is not supplied",
    )
    evaluate_parser.add_argument("--out", help="explicit run directory")
    evaluate_parser.add_argument("--judge-dir", help="override official judge directory")
    evaluate_parser.add_argument(
        "--judge-timeout",
        type=float,
        default=JUDGE_TIMEOUT_SECS,
        help="per-judge timeout in seconds",
    )
    evaluate_parser.add_argument("--fail-fast", action="store_true")
    evaluate_parser.add_argument("--replace", action="store_true")
    evaluate_parser.add_argument("--verbose", action="store_true")
    evaluate_parser.set_defaults(func=evaluate_cmd)

    inspect_run_parser = subparsers.add_parser(
        "inspect-run",
        help="inspect an existing run directory without mutating it",
    )
    inspect_run_parser.add_argument(
        "--json",
        action="store_true",
        help="print machine-readable inspection JSON",
    )
    inspect_run_parser.add_argument("run_dir", help="run directory to inspect")
    inspect_run_parser.set_defaults(func=inspect_run_cmd)

    official_refresh_parser = subparsers.add_parser(
        "official-refresh",
        help="refresh vendored official judge snapshot from an explicit checkout",
    )
    official_refresh_parser.add_argument(
        "--source",
        required=True,
        help="local autotest-for-oskernel checkout containing kernel/judge",
    )
    official_refresh_parser.add_argument(
        "--repo",
        help="source repository URL override for manifest.json",
    )
    official_refresh_parser.add_argument(
        "--commit",
        help="source commit override for manifest.json",
    )
    official_refresh_parser.add_argument(
        "--allow-dirty",
        action="store_true",
        help="allow and record a dirty source checkout",
    )
    official_refresh_parser.set_defaults(func=official_refresh_cmd)

    support_check_parser = subparsers.add_parser(
        "support-check",
        help="validate a local support disk image against the current runner",
    )
    support_check_parser.add_argument("--arch", required=True, choices=("rv", "la"))
    support_check_parser.add_argument("--image", required=True, help="support disk image")
    support_check_parser.add_argument(
        "--json",
        action="store_true",
        help="print machine-readable support image inspection JSON",
    )
    support_check_parser.set_defaults(func=support_check_cmd)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
        return args.func(args)
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130
    except Exception:
        traceback.print_exc(file=sys.stderr)
        return 4
