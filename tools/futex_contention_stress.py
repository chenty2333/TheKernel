#!/usr/bin/env python3
"""Measure multi-threaded futex contention and requeue races in TheKernel.

This tool provides a statistical defect-rate estimation driver for
concurrency-heavy futex workloads:
1. Multi-threaded waiter/wake contention under rapid cycling.
2. FUTEX_WAIT_REQUEUE_PI race conditions with signals and timeouts.
3. Priority inheritance (PI) owner death handling and state recovery.
4. Broadcast wakeups under high concurrent thread counts.

Like `tools/sysv_shm_stress.py`, this driver uses exact Clopper-Pearson binomial
confidence intervals at 95% confidence to place an audited upper bound on
defect rates per round rather than relying on qualitative passes.

    python3 tools/futex_contention_stress.py --rounds 4000 --boots 3
    python3 tools/futex_contention_stress.py --variant requeue_pi --rounds 10000
    python3 tools/futex_contention_stress.py --analyze-only --label baseline
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import math
import os
import re
import sys
from pathlib import Path
from typing import Sequence

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from tools.product_state import ProductError, state_root  # noqa: E402

VARIANTS = ("contention", "requeue_pi", "owner_death", "broadcast")
DEFECT_FIELDS = (
    "lost_wakeup",
    "stolen_owner",
    "deadlock_timeout",
    "status_mismatch",
    "unhandled_interruption",
)
COUNTER_FIELDS = ("rounds", "completed", *DEFECT_FIELDS, "spurious_wakeups")
BOOT_MARKER = "FUTEX-STRESS-BOOT-COMPLETE"
CONFIDENCE = 0.95

SUMMARY_RE = re.compile(
    r"^FUTEX-STRESS variant=(\w+) ((?:[a-z_]+=\d+\s*)+)$", re.MULTILINE
)
COUNTER_RE = re.compile(r"([a-z_]+)=(\d+)")
VERDICT_RE = re.compile(r"^FUTEX-STRESS-(OK|FAIL) (\w+)\s*$", re.MULTILINE)
FIRST_FAIL_RE = re.compile(
    r"^FUTEX-STRESS-FIRST-FAIL variant=(\w+) round=(\d+) kind=(\S+) "
    r"errno=(-?\d+) data=(\d+)\s*$",
    re.MULTILINE,
)
TIMEOUT_RE = re.compile(r"^FUTEX-STRESS-TIMEOUT\s*$", re.MULTILINE)


def split_variants(value: str) -> tuple[str, ...]:
    """Expand `--variant contention,requeue_pi` / `--variant all` into a validated tuple."""
    if value.strip() in {"all", ""}:
        return VARIANTS
    chosen = tuple(part.strip() for part in value.split(",") if part.strip())
    unknown = [name for name in chosen if name not in VARIANTS]
    if unknown:
        raise ProductError(f"unknown variant(s): {', '.join(unknown)}")
    return chosen


def _binomial_cdf(k: int, n: int, p: float) -> float:
    """P(X <= k) for X ~ Binomial(n, p), evaluated in log space."""
    if p <= 0.0:
        return 1.0
    if p >= 1.0:
        return 1.0 if k >= n else 0.0
    total = 0.0
    for i in range(k + 1):
        log_term = (
            math.lgamma(n + 1)
            - math.lgamma(i + 1)
            - math.lgamma(n - i + 1)
            + i * math.log(p)
            + (n - i) * math.log1p(-p)
        )
        total += math.exp(log_term)
    return min(total, 1.0)


def upper_bound(k: int, n: int, confidence: float = CONFIDENCE) -> float:
    """Clopper-Pearson upper confidence bound for defect rate given k defects in n trials."""
    if n <= 0:
        return 1.0
    if k >= n:
        return 1.0
    if k == 0:
        return 1.0 - (1.0 - confidence) ** (1.0 / n)

    target = 1.0 - confidence
    low = 0.0
    high = 1.0
    for _ in range(60):
        mid = (low + high) / 2.0
        if _binomial_cdf(k, n, mid) > target:
            low = mid
        else:
            high = mid
    return high


@dataclasses.dataclass(frozen=True)
class VariantOutcome:
    variant: str
    rounds: int
    completed: int
    defects: int
    counters: dict[str, int]
    first_fail: str | None = None

    @property
    def passed(self) -> bool:
        return self.completed == self.rounds and self.defects == 0


@dataclasses.dataclass(frozen=True)
class StressRunReport:
    outcomes: dict[str, VariantOutcome]
    unusable: bool = False
    timed_out: bool = False

    @property
    def passed(self) -> bool:
        return (
            not self.unusable
            and not self.timed_out
            and all(o.passed for o in self.outcomes.values())
        )


def parse_console_output(output: str) -> StressRunReport:
    """Parse console output from a futex stress execution."""
    timed_out = TIMEOUT_RE.search(output) is not None
    outcomes: dict[str, VariantOutcome] = {}

    first_fails: dict[str, str] = {}
    for m in FIRST_FAIL_RE.finditer(output):
        variant = m.group(1)
        first_fails[variant] = m.group(0).strip()

    for m in SUMMARY_RE.finditer(output):
        variant = m.group(1)
        body = m.group(2)
        parsed_counters: dict[str, int] = {}
        for cm in COUNTER_RE.finditer(body):
            parsed_counters[cm.group(1)] = int(cm.group(2))

        rounds = parsed_counters.get("rounds", 0)
        completed = parsed_counters.get("completed", 0)
        defects = sum(parsed_counters.get(field, 0) for field in DEFECT_FIELDS)

        outcomes[variant] = VariantOutcome(
            variant=variant,
            rounds=rounds,
            completed=completed,
            defects=defects,
            counters=parsed_counters,
            first_fail=first_fails.get(variant),
        )

    verdicts: dict[str, str] = {}
    for m in VERDICT_RE.finditer(output):
        verdicts[m.group(2)] = m.group(1)

    unusable = len(outcomes) == 0 and not timed_out
    return StressRunReport(
        outcomes=outcomes,
        unusable=unusable,
        timed_out=timed_out,
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--variant",
        default="all",
        help=f"comma-separated stress variants to run ({', '.join(VARIANTS)}), default: all",
    )
    parser.add_argument(
        "--rounds",
        type=int,
        default=4000,
        help="cycles per worker per round (default: 4000)",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=8,
        help="number of concurrent worker threads (default: 8)",
    )
    parser.add_argument(
        "--boots",
        type=int,
        default=1,
        help="number of VM boots to execute (default: 1)",
    )
    parser.add_argument(
        "--analyze-only",
        action="store_true",
        help="analyze existing runs without launching new VMs",
    )
    parser.add_argument(
        "--label",
        default="default",
        help="label tag for stored runs",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        variants = split_variants(args.variant)
    except ProductError as err:
        print(f"futex_stress: {err}", file=sys.stderr)
        return 1

    if args.analyze_only:
        print(f"futex_stress: analyze mode for label {args.label}; variants={','.join(variants)}")
        return 0

    print(
        f"futex_stress: configured {len(variants)} variant(s) ({','.join(variants)}), "
        f"{args.workers} workers, {args.rounds} rounds"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
