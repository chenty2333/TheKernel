#!/usr/bin/env python3
"""Validate score-facing OSComp evaluator group markers in a console log."""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO_ROOT))

from tools.oscomp_eval.markers import MarkerError, compatible_summary, parse_log


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
    if os.environ.get("OSCOMP_VALIDATE_OUTPUT_WRAPPER") != "1":
        print(
            "warning: scripts/validate-oscomp-output.py is a compatibility shim; "
            "prefer scripts/oscomp.sh validate-output, or "
            "python3 -m tools.oscomp_eval markers for direct parser debugging.",
            file=sys.stderr,
        )

    args = parse_args()
    log_path = Path(args.log).expanduser()
    try:
        result = parse_log(
            log_path,
            arch=args.arch,
            require_conclusion=args.require_conclusion,
        )
    except MarkerError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    print(compatible_summary(result))
    return 1 if result.has_errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
