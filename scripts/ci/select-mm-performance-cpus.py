#!/usr/bin/env python3
"""Select nested homogeneous host CPU sets for the MM evidence matrix."""

from __future__ import annotations

import argparse
import csv
import sys
from pathlib import Path

from mm_performance_host import (
    CpuSelectionError,
    CpuSelectionUnsupported,
    select_cpu_sets,
)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    result.add_argument("--counts", nargs="+", type=int, required=True)
    result.add_argument("--explicit")
    result.add_argument("--output", type=Path, required=True)
    return result


def main() -> int:
    args = parser().parse_args()
    try:
        selections = select_cpu_sets(args.counts, explicit=args.explicit)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with args.output.open("w", encoding="utf-8", newline="") as output:
            writer = csv.writer(output, delimiter="\t", lineterminator="\n")
            writer.writerow(
                ("requested_cpus", "host_cpu_set", "selection", "cpu_class")
            )
            for selection in selections:
                writer.writerow(
                    (
                        selection.requested_cpus,
                        selection.host_cpu_set,
                        selection.selection,
                        selection.cpu_class,
                    )
                )
    except CpuSelectionUnsupported as error:
        print(f"select-mm-performance-cpus: UNSUPPORTED: {error}", file=sys.stderr)
        return 78
    except (CpuSelectionError, OSError) as error:
        print(f"select-mm-performance-cpus: INVALID: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
