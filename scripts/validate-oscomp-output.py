#!/usr/bin/env python3
"""Validate score-facing OSComp evaluator group markers in a console log."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


CANONICAL_GROUPS = {
    "basic",
    "iozone",
    "busybox",
    "netperf",
    "lua",
    "libcbench",
    "libctest",
    "cyclictest",
    "lmbench",
    "iperf",
    "ltp",
    "unixbench",
}

GROUP_RE = re.compile(r"^#### OS COMP TEST GROUP (START|END) ([^ ]+) ####$")
SUFFIX_RE = re.compile(r"-(musl|glibc)$")
CONCLUSION_RE = re.compile(
    r"(QEMU timed out after|OSCOMP RUNNER (?:GLOBAL )?TIMEOUT|poweroff|shutdown|System is shutting down)",
    re.IGNORECASE,
)


def marker_base_group(group: str) -> str:
    return SUFFIX_RE.sub("", group)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="validate score-facing OSComp evaluator output markers"
    )
    parser.add_argument("--log", required=True, help="console log to validate")
    parser.add_argument("--arch", default="", help="optional architecture label")
    parser.add_argument(
        "--require-conclusion",
        action="store_true",
        help="require a visible timeout/shutdown conclusion in the log text",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    log_path = Path(args.log).expanduser()
    if not log_path.is_file():
        print(f"error: log not found: {log_path}", file=sys.stderr)
        return 2

    text = log_path.read_text(encoding="utf-8", errors="replace")
    issues: list[str] = []
    current: tuple[str, int] | None = None
    complete: list[tuple[str, int, int]] = []
    markers: list[tuple[str, str, int]] = []

    for line_no, line in enumerate(text.splitlines(), 1):
        match = GROUP_RE.match(line)
        if not match:
            continue

        action, group = match.group(1), match.group(2)
        markers.append((action, group, line_no))

        base_group = marker_base_group(group)
        if base_group not in CANONICAL_GROUPS:
            issues.append(
                f"line {line_no}: score-facing group marker has unknown base group: {group}"
            )

        if action == "START":
            if current is not None:
                open_group, open_line = current
                issues.append(
                    f"line {line_no}: group {group} starts before {open_group} from line {open_line} ends"
                )
            current = (group, line_no)
            continue

        if current is None:
            issues.append(f"line {line_no}: group {group} ends without a start")
            continue

        open_group, open_line = current
        if group != open_group:
            issues.append(
                f"line {line_no}: group {group} ends but open group is {open_group} from line {open_line}"
            )
            current = None
            continue

        if base_group in CANONICAL_GROUPS:
            complete.append((group, open_line, line_no))
        current = None

    if current is not None:
        open_group, open_line = current
        issues.append(f"line {open_line}: group {open_group} starts without a matching end")

    if text.strip() and not complete:
        issues.append("log has output but zero complete evaluator groups")

    if args.require_conclusion and not CONCLUSION_RE.search(text):
        issues.append("log has no visible timeout/shutdown conclusion")

    label = f" arch={args.arch}" if args.arch else ""
    print(
        f"oscomp-output{label} markers={len(markers)} complete_groups={len(complete)} issues={len(issues)}"
    )
    for group, start_line, end_line in complete:
        print(f"  complete {group} lines={start_line}-{end_line}")

    if issues:
        print("issues:")
        for issue in issues:
            print(f"  - {issue}")
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
