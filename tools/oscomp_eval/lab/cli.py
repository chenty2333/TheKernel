"""Focused OSComp lab CLI."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from ..config import REPLAY_TIMEOUT_FOCUSED_SECS, canonical_arch
from ..paths import repo_root
from ..replay import evaluate_replay, exit_code_for_replay
from .model import PayloadDraft
from .payload import build_focused_support_image, lab_state_root, plan_key, write_payload
from .plugins import GENERIC_GROUPS, plugin_for
from .selection import SelectionError, parse_selections


def build_focus_plan(args: argparse.Namespace, *, build_support: bool):
    root = repo_root()
    arch = canonical_arch(args.arch)
    selections = parse_selections(args.select)
    draft = PayloadDraft()
    for selection in selections:
        plugin_for(selection.group).apply(selection, draft, root=root)
    plan = write_payload(arch=arch, selections=selections, draft=draft, root=root, materialize=build_support)
    support_image = build_focused_support_image(plan, root=root) if build_support else None
    return plan.__class__(
        arch=plan.arch,
        selections=plan.selections,
        group_matrix=plan.group_matrix,
        cases=plan.cases,
        plan_path=plan.plan_path,
        cases_path=plan.cases_path,
        ltp_list_path=plan.ltp_list_path,
        support_image=support_image,
        notes=plan.notes,
    )


def explain_cmd(args: argparse.Namespace) -> int:
    try:
        plan = build_focus_plan(args, build_support=False)
    except (SelectionError, ValueError, RuntimeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(f"arch: {plan.arch}")
    print("selections:")
    for selection in plan.selections:
        print(f"  {selection.text}")
    print("guest plan:")
    for group, libc in plan.group_matrix:
        print(f"  /{libc} {group}")
    if plan.cases:
        print("cases:")
        for case in plan.cases:
            print(f"  {case.group_id} {case.name}")
    print(f"plan: {plan.plan_path}")
    print(f"cases_file: {plan.cases_path}")
    print(f"ltp_list: {plan.ltp_list_path}")
    if plan.support_image is not None:
        print(f"support_image: {plan.support_image}")
    else:
        print("support_image: <built by lab run>")
    return 0


def run_cmd(args: argparse.Namespace) -> int:
    try:
        plan = build_focus_plan(args, build_support=True)
        name = args.name or f"lab-{plan.arch}-{plan_key(plan.arch, plan.selections)}"
        run_dir = Path(args.out).expanduser() if args.out else lab_state_root(repo_root()) / "runs" / name
        result = evaluate_replay(
            name=name,
            arch=plan.arch,
            run_dir=run_dir,
            timeout_secs=args.timeout,
            idle_timeout_secs=args.idle_timeout,
            image=Path(args.image).expanduser() if args.image else None,
            support_image=plan.support_image,
            plan_path=plan.plan_path,
            skip_kernel_build=not args.build_kernel,
            replace=True,
            group_libc_matrix=plan.group_matrix,
            verbose=args.verbose,
        )
    except (SelectionError, ValueError, RuntimeError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(
        "lab-run "
        f"run_dir={result.run_dir} "
        f"status={result.status} "
        f"total={result.score.total_score:.6g} "
        f"issues={len(result.score.issues)}"
    )
    return exit_code_for_replay(result)


def list_cmd(args: argparse.Namespace) -> int:
    print("groups:")
    for group in ("ltp", *GENERIC_GROUPS):
        plugin = plugin_for(group)
        print(f"  {group} ({plugin.selector_help})")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="scripts/lab")
    subparsers = parser.add_subparsers(dest="command", required=True)

    list_parser = subparsers.add_parser("list", help="list focused-lab groups")
    list_parser.set_defaults(func=list_cmd)

    for command, func, help_text in (
        ("explain", explain_cmd, "show the generated focused plan"),
        ("run", run_cmd, "run a focused replay"),
    ):
        sub = subparsers.add_parser(command, help=help_text)
        sub.add_argument("--arch", required=True, choices=("rv", "la", "riscv64", "loongarch64"))
        sub.add_argument("--select", action="append", required=True, help="GROUP-LIBC[:EXPR], e.g. ltp-glibc:openat01")
        sub.add_argument("--image", help="official testsuite image override")
        sub.add_argument("--timeout", type=int, default=REPLAY_TIMEOUT_FOCUSED_SECS)
        sub.add_argument("--idle-timeout", type=int)
        sub.add_argument("--name", help="run name")
        sub.add_argument("--out", help="explicit run directory")
        sub.add_argument("--build-kernel", action="store_true", help="build kernel-ARCH before replay")
        sub.add_argument("--verbose", action="store_true")
        sub.set_defaults(func=func)

    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
        return args.func(args)
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130
