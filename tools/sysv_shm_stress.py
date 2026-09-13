#!/usr/bin/env python3
"""Measure the SysV SHM exit-retirement window inside TheKernel.

`tests/guest/portable/sysv-shm-exit-stress.c` loops the window that
`tests/guest/system-init.c::test_sysv_shm` exercises once per boot: a parent
marks a segment IPC_RMID, forks a child that inherits the parent's
attachments, reaps the child, and then requires `shmat()` on the removed id to
fail with EINVAL.  The guest suite cannot loop that topology because it costs
one boot per case, so this driver boots the shell-profile guest once per boot
and runs the stress program inside it, attacking the window thousands of times
per boot.

Every boot is dispatched through the shared heavy-command serializer
(`heavy-run.sh`), one slot per boot, and each boot's console log and runner log
are kept under `$THEKERNEL_STATE_DIR/runs/<label>-<n>/` so the numbers can be
audited without repeating the run.

    python3 tools/sysv_shm_stress.py --rounds 4000 --boots 3
    python3 tools/sysv_shm_stress.py --variant wait --rounds 20000 --boots 2
    python3 tools/sysv_shm_stress.py --analyze-only --label baseline

The first invocation after a tree change builds the shell-profile kernel and
rootfs; pass `--no-build` to reuse them.  `--host-control` runs the same
program on host Linux instead of in the guest: that is a check on the harness,
not on TheKernel.

A boot that ran has one of three states.  `ok` means every requested variant
completed its rounds and no counter moved.  `DEFECT` means it completed and a
round failed: that is the result worth having, it stays in the pooled
denominator, and its first-failure line and console log are printed.  `UNUSABLE`
means it did not measure what was asked (no console, no summary for a variant,
a short round count, an alarm timeout) and is therefore excluded from the pool
and retried; a `--rounds` mismatch is how a cut-short loop surfaces, so a boot
can never pass by running fewer rounds than requested.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import math
import os
import re
import shlex
import subprocess
import sys
import time
from pathlib import Path
from typing import Sequence

REPO_ROOT = Path(__file__).resolve().parents[1]
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from tools.product_state import ProductError, state_root  # noqa: E402

GUEST_TOOL = "/opt/thekernel-tests/portable/sysv-shm-exit-stress"
DEFAULT_HEAVY_RUN = Path("/home/ava/.cache/thekernel-targets/heavy-run.sh")
DEFAULT_SOURCE_CACHE = Path("/home/ava/.cache/thekernel-targets/source-cache")
VARIANTS = ("wait", "pipe", "multi", "nowait", "shared")
# The tool's own defect taxonomy: every counter that makes an invocation fail.
DEFECT_FIELDS = (
    "setup_fail",
    "child_fail",
    "value_fail",
    "attach_fail",
    "errno_fail",
    "retire_early_fail",
    "grandchild_fail",
)
# The fields the program prints today, in print order.  Parsing does not
# depend on this list: a summary line is read as generic key=value pairs, so
# logs written by an older or newer revision of the program still parse and
# unknown counters are preserved for the JSON report.
COUNTER_FIELDS = ("rounds", "completed", *DEFECT_FIELDS, "alive_probe")
BOOT_MARKER = "SYSV-SHM-STRESS-BOOT-COMPLETE"
CONFIDENCE = 0.95

SUMMARY_RE = re.compile(
    r"^SYSV-SHM-STRESS variant=(\w+) ((?:[a-z_]+=\d+\s*)+)$", re.MULTILINE
)
COUNTER_RE = re.compile(r"([a-z_]+)=(\d+)")
# The guest command line a boot recorded, used to recover which variants it
# actually asked for when stored logs are re-analysed.
COMMAND_VARIANT_RE = re.compile(rf"{re.escape(GUEST_TOOL)} (\w+) \d+ \d+")
VERDICT_RE = re.compile(r"^SYSV-SHM-STRESS-(OK|FAIL) (\w+)\s*$", re.MULTILINE)
FIRST_FAIL_RE = re.compile(
    r"^SYSV-SHM-STRESS-FIRST-FAIL variant=(\w+) round=(\d+) kind=(\S+) "
    r"errno=(-?\d+) data=(\d+)\s*$",
    re.MULTILINE,
)
TIMEOUT_RE = re.compile(r"^SYSV-SHM-STRESS-TIMEOUT\s*$", re.MULTILINE)


# --------------------------------------------------------------------------
# Counting and confidence intervals
# --------------------------------------------------------------------------


def split_variants(value: str) -> tuple[str, ...]:
    """Expand `--variant wait,pipe` / `--variant all` into a validated tuple."""

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


def upper_bound(defects: int, trials: int, confidence: float = CONFIDENCE) -> float:
    """One-sided exact (Clopper-Pearson) upper bound on a binomial rate.

    With zero defects this is the closed form 1 - confidence**(1/trials),
    familiar as the rule of three.  With defects it is the root of
    P(X <= defects; trials, p) == 1 - confidence.
    """

    if trials <= 0:
        return float("nan")
    if defects <= 0:
        return 1.0 - (1.0 - confidence) ** (1.0 / trials)
    if defects >= trials:
        return 1.0
    alpha = 1.0 - confidence
    low, high = defects / trials, 1.0
    for _ in range(200):
        middle = (low + high) / 2.0
        if _binomial_cdf(defects, trials, middle) > alpha:
            low = middle
        else:
            high = middle
        if high - low < 1e-15:
            break
    return (low + high) / 2.0


def _rate_text(defects: int, trials: int) -> str:
    if trials <= 0:
        return "no trials"
    bound = upper_bound(defects, trials)
    per = "1 in %.0f" % (1.0 / bound) if bound > 0 else "never"
    return (
        f"{defects}/{trials} = {defects / trials:.3g} per trial, "
        f"one-sided 95% upper bound {bound:.3g} (about {per})"
    )


# --------------------------------------------------------------------------
# Guest console parsing
# --------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Invocation:
    """One summary line the stress program printed."""

    variant: str
    counters: dict[str, int]
    ok: bool
    first_fail: str | None
    timed_out: bool
    # True when the round counters moved or the program printed FAIL.  A
    # missing verdict line is not a defect: it is truncated output, which the
    # marker rule below treats as unusable evidence instead.
    failed: bool = False

    @property
    def rounds(self) -> int:
        return self.counters["rounds"]

    @property
    def completed(self) -> int:
        return self.counters["completed"]

    @property
    def defects(self) -> int:
        return sum(self.counters[field] for field in DEFECT_FIELDS)

    @property
    def alive_probe(self) -> int:
        """Descriptive only: an unsynchronised probe finding a live child."""

        return self.counters["alive_probe"]


@dataclasses.dataclass(frozen=True)
class BootParse:
    """What one boot's console log says, judged against what was planned.

    Two different things can go wrong and they must not be confused:

    * `problems` are *evidence* problems.  A boot whose console lost the
      marker, whose summary line never appeared, or whose loop was cut short
      did not measure what was asked, so it is dropped from the pool and
      retried.
    * `defect_notes` are *defects*.  The boot ran to completion and the
      program reported that the window failed; that is the result the whole
      exercise is looking for, it stays in the pool, and it is printed loudly.
    """

    invocations: tuple[Invocation, ...]
    missing: tuple[str, ...]
    marker: bool
    timed_out: bool
    problems: tuple[str, ...]
    defect_notes: tuple[str, ...] = ()

    @property
    def valid(self) -> bool:
        """True when the boot is usable evidence, defects or not."""

        return not self.problems

    @property
    def defects(self) -> int:
        return sum(invocation.defects for invocation in self.invocations)

    @property
    def rounds(self) -> int:
        return sum(invocation.rounds for invocation in self.invocations)

    @property
    def failed_invocations(self) -> tuple[Invocation, ...]:
        return tuple(
            invocation for invocation in self.invocations if invocation.failed
        )


def parse_boot_text(
    text: str, planned: Sequence[str], expected_rounds: int | None
) -> BootParse:
    """Parse one console log against the variants and round count requested.

    `expected_rounds=None` accepts whatever round count each summary reports,
    which is what re-analysing stored logs of runs with different `--rounds`
    needs; a live boot always passes the number it asked for.
    """

    problems: list[str] = []
    timed_out = bool(TIMEOUT_RE.search(text))
    marker = any(line.strip() == BOOT_MARKER for line in text.splitlines())

    first_fails: dict[str, str] = {}
    for match in FIRST_FAIL_RE.finditer(text):
        first_fails[match.group(1)] = (
            f"variant={match.group(1)} round={match.group(2)} "
            f"kind={match.group(3)} errno={match.group(4)} data={match.group(5)}"
        )
    verdicts: dict[str, str] = {}
    for match in VERDICT_RE.finditer(text):
        verdicts[match.group(2)] = match.group(1)

    invocations: list[Invocation] = []
    seen: dict[str, int] = {}
    for match in SUMMARY_RE.finditer(text):
        variant = match.group(1)
        counters = dict.fromkeys(COUNTER_FIELDS, 0)
        for field, value in COUNTER_RE.findall(match.group(2)):
            counters[field] = int(value)
        seen[variant] = seen.get(variant, 0) + 1
        defects = sum(counters[field] for field in DEFECT_FIELDS)
        invocations.append(
            Invocation(
                variant=variant,
                counters=counters,
                ok=verdicts.get(variant) == "OK",
                first_fail=first_fails.get(variant),
                timed_out=timed_out,
                failed=defects > 0 or verdicts.get(variant) == "FAIL",
            )
        )

    missing = tuple(variant for variant in planned if variant not in seen)
    defect_notes: list[str] = []
    for variant, count in seen.items():
        if count > 1:
            problems.append(f"{variant} printed {count} summary lines")
    if missing:
        problems.append(f"no summary for {', '.join(missing)}")
    for invocation in invocations:
        if expected_rounds is not None and invocation.rounds != expected_rounds:
            problems.append(
                f"{invocation.variant} ran {invocation.rounds} rounds, "
                f"expected {expected_rounds}"
            )
        if invocation.failed:
            detail = f" ({invocation.first_fail})" if invocation.first_fail else ""
            defect_notes.append(
                f"{invocation.variant} FAIL: {invocation.defects} defective round(s)"
                f"{detail}"
            )
    if timed_out:
        problems.append("SYSV-SHM-STRESS-TIMEOUT marker present")
    # The marker is printed by the guest shell only when every invocation
    # exited zero.  A missing marker is therefore expected when a variant
    # failed; it is evidence against the boot only when nothing explains it.
    if not marker and not defect_notes:
        problems.append(f"missing completion marker {BOOT_MARKER}")
    return BootParse(
        invocations=tuple(invocations),
        missing=missing,
        marker=marker,
        timed_out=timed_out,
        problems=tuple(problems),
        defect_notes=tuple(defect_notes),
    )


@dataclasses.dataclass(frozen=True)
class BootRun:
    """One attempted boot: what was asked for and what came back."""

    name: str
    attempt: int
    runner_rc: int
    console: Path | None
    runner_log: Path | None
    parse: BootParse
    seconds: float = 0.0

    @property
    def valid(self) -> bool:
        return self.parse.valid


# --------------------------------------------------------------------------
# Running one boot
# --------------------------------------------------------------------------


def guest_command(variants: Sequence[str], rounds: int, alarm: int) -> str:
    """The single guest shell line whose success gates the completion marker.

    Every requested variant runs even when an earlier one reports a defect:
    the first failing variant must not hide the others from the same boot, and
    a defect is a result to keep, not a reason to discard the boot.  The line
    ends with `test $ok -eq 0`, which is what the caller's `&& echo MARKER`
    gates on, so the marker still means "every invocation exited zero".
    """

    runs = " ; ".join(
        f"{GUEST_TOOL} {variant} {rounds} {alarm} || ok=1" for variant in variants
    )
    return f"ok=0 ; {runs} ; test $ok -eq 0"


def heavy_command(inner: Sequence[str], heavy_run: Path) -> list[str]:
    """Wrap one command in the shared serializer, as its own single argument."""

    return [str(heavy_run), " ".join(shlex.quote(part) for part in inner)]


def single_boot(args: argparse.Namespace) -> int:
    """Boot once, run the stress invocations, and report where the log landed."""

    from tools import thekernel  # imported lazily: a boot is the only user

    if not args.workdir:
        print("--single-boot requires --workdir", file=sys.stderr)
        return 2
    workdir = Path(args.workdir).expanduser().resolve()
    workdir.mkdir(parents=True, exist_ok=True)
    variants = split_variants(args.variant)
    command = guest_command(variants, args.rounds, args.alarm)
    namespace = argparse.Namespace(
        memory=args.memory,
        smp=args.smp,
        platform=args.platform,
        asid_fast_switch=False,
        m5_candidate=False,
        io_submit_batch=False,
        io_notify_fastpath=False,
        no_build=args.no_build,
        workdir=str(workdir),
        timeout=args.timeout,
        qemu_debug=None,
    )
    failure = ""
    try:
        thekernel.guest_tool_run(namespace, command, BOOT_MARKER, args.smp)
    except ProductError as error:
        failure = str(error)
    logs = sorted(workdir.glob("*/console.log"), key=lambda path: path.stat().st_mtime)
    if not logs:
        print(f"STRESS-SINGLE-BOOT-NO-LOG {failure}", file=sys.stderr)
        return 1
    console = logs[-1]
    parsed = parse_boot_text(
        console.read_text(encoding="utf-8", errors="replace"), variants, args.rounds
    )
    print(f"STRESS-SINGLE-BOOT-LOG {console}")
    if failure:
        print(f"STRESS-SINGLE-BOOT-FAILURE {failure}", file=sys.stderr)
    for problem in parsed.problems:
        print(f"STRESS-SINGLE-BOOT-PROBLEM {problem}", file=sys.stderr)
    for note in parsed.defect_notes:
        print(f"STRESS-SINGLE-BOOT-DEFECT {note}", file=sys.stderr)
    return 0 if parsed.valid and not parsed.defects else 1


def _child_env(args: argparse.Namespace) -> dict[str, str]:
    env = dict(os.environ)
    env["THEKERNEL_STATE_DIR"] = str(args.state_dir)
    env["THEKERNEL_SOURCE_CACHE"] = str(args.source_cache)
    return env


def run_boot(args: argparse.Namespace, name: str, attempt: int) -> BootRun:
    """Dispatch one boot through the serializer and parse its console log."""

    workdir = Path(args.state_dir) / "runs" / name
    workdir.mkdir(parents=True, exist_ok=True)
    inner = [
        sys.executable,
        str(Path(__file__).resolve()),
        "--single-boot",
        "--variant",
        ",".join(split_variants(args.variant)),
        "--rounds",
        str(args.rounds),
        "--alarm",
        str(args.alarm),
        "--smp",
        str(args.smp),
        "--memory",
        args.memory,
        "--platform",
        args.platform,
        "--timeout",
        str(args.timeout),
        "--workdir",
        str(workdir),
    ]
    if args.no_build:
        inner.append("--no-build")
    started = time.monotonic()
    completed = subprocess.run(
        heavy_command(inner, Path(args.heavy_run)),
        env=_child_env(args),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=False,
    )
    seconds = time.monotonic() - started
    runner_log = workdir / "runner.log"
    runner_log.write_text(completed.stdout, encoding="utf-8")
    logs = sorted(workdir.glob("*/console.log"), key=lambda path: path.stat().st_mtime)
    console = logs[-1] if logs else None
    text = (
        console.read_text(encoding="utf-8", errors="replace") if console is not None else ""
    )
    parsed = parse_boot_text(text, split_variants(args.variant), args.rounds)
    if console is None:
        parsed = dataclasses.replace(
            parsed, problems=(*parsed.problems, "boot produced no console log")
        )
    return BootRun(
        name=name,
        attempt=attempt,
        runner_rc=completed.returncode,
        console=console,
        runner_log=runner_log,
        parse=parsed,
        seconds=seconds,
    )


def host_control(args: argparse.Namespace) -> tuple[BootRun, ...]:
    """Run the same program on host Linux, as a control on the harness."""

    binary = Path(args.host_binary).expanduser()
    if not binary.is_file():
        raise ProductError(
            f"host control binary is missing: {binary}; build it with "
            "gcc -O2 -o <path> tests/guest/portable/sysv-shm-exit-stress.c"
        )
    runs = []
    planned = split_variants(args.variant)
    for index, variant in enumerate(planned, start=1):
        name = f"{args.label}-host-{variant}-{index}"
        workdir = Path(args.state_dir) / "runs" / name
        workdir.mkdir(parents=True, exist_ok=True)
        inner = [str(binary), variant, str(args.rounds), str(args.alarm)]
        started = time.monotonic()
        completed = subprocess.run(
            heavy_command(inner, Path(args.heavy_run)),
            env=_child_env(args),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            check=False,
        )
        seconds = time.monotonic() - started
        console = workdir / "console.log"
        console.write_text(completed.stdout, encoding="utf-8")
        parsed = parse_boot_text(completed.stdout, [variant], args.rounds)
        runs.append(
            BootRun(
                name=name,
                attempt=1,
                runner_rc=completed.returncode,
                console=console,
                runner_log=console,
                parse=parsed,
                seconds=seconds,
            )
        )
    return tuple(runs)


# --------------------------------------------------------------------------
# Aggregation and report
# --------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Totals:
    boots: int
    invocations: int
    rounds: int
    defects: int
    alive_probe: int

    @property
    def upper(self) -> float:
        return upper_bound(self.defects, self.rounds)


def totals_for(runs: Sequence[BootRun]) -> dict[str, Totals]:
    """Pool valid boots by variant, keeping per-variant N explicit."""

    pooled: dict[str, list[Invocation]] = {}
    boots_per_variant: dict[str, int] = {}
    for run in runs:
        for invocation in run.parse.invocations:
            pooled.setdefault(invocation.variant, []).append(invocation)
            boots_per_variant[invocation.variant] = (
                boots_per_variant.get(invocation.variant, 0) + 1
            )
    return {
        variant: Totals(
            boots=boots_per_variant.get(variant, 0),
            invocations=len(invocations),
            rounds=sum(item.rounds for item in invocations),
            defects=sum(item.defects for item in invocations),
            alive_probe=sum(item.alive_probe for item in invocations),
        )
        for variant, invocations in pooled.items()
    }


def render_report(
    runs: Sequence[BootRun], attempts: int, heading: str
) -> str:
    """Render the whole measurement: per boot, per variant, and pooled."""

    valid = [run for run in runs if run.valid]
    lines = [heading, f"boots valid: {len(valid)} of {attempts} attempted", ""]
    header = (
        f"{'boot':28} {'rc':>3} {'inv':>3} {'rounds':>8} {'defects':>7} "
        f"{'secs':>6}  state"
    )
    lines.append(header)
    lines.append("-" * len(header))
    for run in runs:
        state = "ok" if run.valid else "UNUSABLE"
        if run.parse.defects:
            state = "DEFECT" if run.valid else "UNUSABLE+DEFECT"
        lines.append(
            f"{run.name:28} {run.runner_rc:3d} {len(run.parse.invocations):3d} "
            f"{run.parse.rounds:8d} {run.parse.defects:7d} {run.seconds:6.1f}  {state}"
        )
        for problem in run.parse.problems:
            lines.append(f"{'':28}   ! {problem}")
        for note in run.parse.defect_notes:
            lines.append(f"{'':28}   X {note}")
    lines.append("")

    totals = totals_for(valid)
    header = (
        f"{'variant':8} {'boots':>5} {'inv':>4} {'rounds':>8} {'defects':>7} "
        f"{'alive':>7} {'ub95/round':>11}"
    )
    lines.append(header)
    lines.append("-" * len(header))
    for variant in VARIANTS:
        if variant not in totals:
            lines.append(f"{variant:8} {'-':>5} {'-':>4} {'-':>8} {'-':>7} {'-':>7} {'-':>11}")
            continue
        row = totals[variant]
        lines.append(
            f"{variant:8} {row.boots:5d} {row.invocations:4d} {row.rounds:8d} "
            f"{row.defects:7d} {row.alive_probe:7d} {row.upper:11.3g}"
        )
    pooled_rounds = sum(row.rounds for row in totals.values())
    pooled_defects = sum(row.defects for row in totals.values())
    lines.append("-" * len(header))
    lines.append(
        f"{'TOTAL':8} {len(valid):5d} "
        f"{sum(row.invocations for row in totals.values()):4d} {pooled_rounds:8d} "
        f"{pooled_defects:7d} "
        f"{sum(row.alive_probe for row in totals.values()):7d} "
        f"{upper_bound(pooled_defects, pooled_rounds):11.3g}"
    )
    lines.append("")
    pool_notes = [
        note
        for run in valid
        for note in run.parse.defect_notes
    ]
    excluded = sum(run.parse.defects for run in runs if not run.valid)
    if pool_notes:
        lines.append(
            f"*** DEFECTS OBSERVED: {pooled_defects} defective round(s) in "
            f"{pooled_rounds}; the program did not complete every round cleanly. "
            f"Which assertion failed decides what that means for the fix. ***"
        )
        for note in pool_notes:
            lines.append(f"    {note}")
        for run in valid:
            if run.parse.defects and run.console is not None:
                lines.append(f"    console: {run.console}")
        lines.append("")
    if excluded:
        lines.append(
            f"defects in boots excluded as unusable evidence (not in the "
            f"denominator above): {excluded}"
        )
        lines.append("")
    lines.append(f"per round:  {_rate_text(pooled_defects, pooled_rounds)}")
    bad_boots = sum(1 for run in valid if run.parse.defects)
    if valid:
        boot_text = (
            f"{bad_boots}/{len(valid)} boots had a defect, one-sided 95% upper "
            f"bound {upper_bound(bad_boots, len(valid)):.3g} per boot"
        )
    else:
        boot_text = "no valid boots"
    lines.append(f"per boot:   {boot_text}")
    lines.append("")
    lines.append(
        "note: rounds inside one boot share one kernel instance, so the pooled "
        "interval is narrower than boot-to-boot variation alone would give; "
        "N per variant above is the honest denominator."
    )
    return "\n".join(lines)


def summary_json(runs: Sequence[BootRun], attempts: int) -> dict[str, object]:
    """Machine-readable copy of the measurement for the design note."""

    valid = [run for run in runs if run.valid]
    totals = totals_for(valid)
    return {
        "attempts": attempts,
        "valid_boots": len(valid),
        "defects": sum(run.parse.defects for run in valid),
        "verdict": (
            "defects observed"
            if any(run.parse.defects for run in valid)
            else "no defects observed"
        ),
        "boots": [
            {
                "name": run.name,
                "attempt": run.attempt,
                "runner_rc": run.runner_rc,
                "seconds": round(run.seconds, 2),
                "valid": run.valid,
                "problems": list(run.parse.problems),
                "defect_notes": list(run.parse.defect_notes),
                "console": str(run.console) if run.console else None,
                "runner_log": str(run.runner_log) if run.runner_log else None,
                "invocations": [
                    {
                        "variant": invocation.variant,
                        "rounds": invocation.rounds,
                        "completed": invocation.completed,
                        "defects": invocation.defects,
                        "alive_probe": invocation.alive_probe,
                        "ok": invocation.ok,
                        "first_fail": invocation.first_fail,
                        "counters": invocation.counters,
                    }
                    for invocation in run.parse.invocations
                ],
            }
            for run in runs
        ],
        "variants": {
            variant: {
                "boots": row.boots,
                "invocations": row.invocations,
                "rounds": row.rounds,
                "defects": row.defects,
                "alive_probe": row.alive_probe,
                "upper_bound_95_per_round": row.upper,
            }
            for variant, row in totals.items()
        },
        "pooled": {
            "rounds": sum(row.rounds for row in totals.values()),
            "defects": sum(row.defects for row in totals.values()),
            "upper_bound_95_per_round": upper_bound(
                sum(row.defects for row in totals.values()),
                sum(row.rounds for row in totals.values()),
            ),
        },
    }


# --------------------------------------------------------------------------
# Analyze-only mode: re-read the logs a previous run left behind
# --------------------------------------------------------------------------


def analyze_only(args: argparse.Namespace) -> tuple[tuple[BootRun, ...], int]:
    """Re-parse existing `runs/<label>-<n>/` boots without booting anything.

    `--label` may be a comma list, which is how the boots of several runs with
    different `--rounds` are pooled into one table; each summary's own round
    count is the denominator.
    """

    runs_root = Path(args.state_dir) / "runs"
    labels = [part.strip() for part in args.label.split(",") if part.strip()]
    directories = sorted(
        (
            path
            for path in runs_root.iterdir()
            if path.is_dir()
            and any(
                path.name.startswith(f"{label}-") and not path.name.startswith(f"{label}-host-")
                for label in labels
            )
        ),
        key=lambda path: path.name,
    )
    runs = []
    for directory in directories:
        logs = sorted(
            directory.glob("*/console.log"), key=lambda path: path.stat().st_mtime
        )
        console = logs[-1] if logs else None
        text = (
            console.read_text(encoding="utf-8", errors="replace")
            if console is not None
            else ""
        )
        # The boot's own commands file records what was actually requested, so
        # a run that exercised one variant is not judged against all five.
        planned = split_variants(args.variant)
        for commands in directory.glob("*/commands"):
            requested = COMMAND_VARIANT_RE.findall(
                commands.read_text(encoding="utf-8", errors="replace")
            )
            if requested:
                planned = tuple(requested)
                break
        parsed = parse_boot_text(text, planned, None)
        runs.append(
            BootRun(
                name=directory.name,
                attempt=1,
                runner_rc=0 if parsed.valid else 1,
                console=console,
                runner_log=directory / "runner.log",
                parse=parsed,
            )
        )
    return tuple(runs), len(runs)


# --------------------------------------------------------------------------
# Command line
# --------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Measure the SysV SHM exit-retirement window inside TheKernel by "
            "running tests/guest/portable/sysv-shm-exit-stress.c in the guest."
        )
    )
    parser.add_argument("--variant", default="all", help="all, one name, or a comma list")
    parser.add_argument(
        "--rounds", type=int, default=4000, help="rounds per variant per boot"
    )
    parser.add_argument("--boots", type=int, default=3, help="valid boots wanted")
    parser.add_argument(
        "--max-attempts",
        type=int,
        help="boot attempts allowed in total (default: twice --boots)",
    )
    parser.add_argument("--label", default="stress", help="run directory prefix")
    parser.add_argument("--smp", type=int, default=4)
    parser.add_argument("--memory", default="512M")
    parser.add_argument("--platform", default="q35-uefi", choices=("n305", "q35-uefi"))
    parser.add_argument(
        "--timeout", type=float, default=900.0, help="whole-boot wall clock limit"
    )
    parser.add_argument(
        "--alarm", type=int, default=300, help="per-invocation alarm seconds in the guest"
    )
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument(
        "--state-dir",
        default=os.environ.get("THEKERNEL_STATE_DIR") or None,
        help="artifact state directory (default: $THEKERNEL_STATE_DIR or the product default)",
    )
    parser.add_argument(
        "--source-cache",
        default=os.environ.get("THEKERNEL_SOURCE_CACHE") or str(DEFAULT_SOURCE_CACHE),
    )
    parser.add_argument("--heavy-run", default=str(DEFAULT_HEAVY_RUN))
    parser.add_argument("--json", help="write the machine-readable summary here")
    parser.add_argument(
        "--analyze-only",
        action="store_true",
        help=(
            "re-parse runs/<label>-*/ console logs instead of booting; "
            "--label may be a comma list and each summary's own round count "
            "is used"
        ),
    )
    parser.add_argument(
        "--host-control",
        action="store_true",
        help="run the same program on host Linux instead of in the guest",
    )
    parser.add_argument(
        "--host-binary",
        default=None,
        help="host build of the stress program (default: <state-dir>/bin/sysv-shm-exit-stress)",
    )
    parser.add_argument("--single-boot", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--workdir", help=argparse.SUPPRESS)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.rounds < 1 or args.boots < 1:
        print("--rounds and --boots must be positive", file=sys.stderr)
        return 2
    if args.single_boot:
        return single_boot(args)
    state_dir = Path(args.state_dir).expanduser() if args.state_dir else state_root()
    args.state_dir = str(state_dir)
    if args.host_binary is None:
        args.host_binary = str(state_dir / "bin" / "sysv-shm-exit-stress")

    if args.host_control:
        heading = (
            "SysV SHM exit-retirement stress - host Linux control\n"
            f"binary    : {args.host_binary}\n"
            f"rounds    : {args.rounds} per variant"
        )
        runs = host_control(args)
        attempts = len(runs)
    elif args.analyze_only:
        heading = (
            "SysV SHM exit-retirement stress - re-analysis of stored logs\n"
            f"label     : {args.label}"
        )
        runs, attempts = analyze_only(args)
    else:
        planned = split_variants(args.variant)
        attempts_allowed = args.max_attempts or args.boots * 2
        heading_lines = [
            "SysV SHM exit-retirement stress - guest measurement",
            f"state dir : {args.state_dir}",
            f"source    : {args.source_cache}",
            f"guest     : profile shell, --smp {args.smp}, memory {args.memory}, "
            f"platform {args.platform}, accel kvm",
            f"plan      : {', '.join(planned)} x {args.rounds} rounds x "
            f"{args.boots} valid boots (at most {attempts_allowed} attempts)",
        ]
        heading = "\n".join(heading_lines)
        runs_list: list[BootRun] = []
        valid = 0
        attempt = 0
        index = 0
        while valid < args.boots and attempt < attempts_allowed:
            attempt += 1
            index += 1
            name = f"{args.label}-{index}"
            print(f"[stress] boot {name} (attempt {attempt}/{attempts_allowed})", flush=True)
            run = run_boot(args, name, attempt)
            runs_list.append(run)
            if run.valid:
                valid += 1
            else:
                print(
                    f"[stress] {name} unusable: {'; '.join(run.parse.problems)}",
                    file=sys.stderr,
                    flush=True,
                )
            for note in run.parse.defect_notes:
                print(f"[stress] {name} DEFECT: {note}", file=sys.stderr, flush=True)
        runs = tuple(runs_list)
        attempts = attempt

    report = render_report(runs, attempts, heading)
    print()
    print(report)
    if args.json:
        Path(args.json).expanduser().write_text(
            json.dumps(summary_json(runs, attempts), indent=2) + "\n", encoding="utf-8"
        )
        print(f"\njson summary: {args.json}")
    defects = sum(run.parse.defects for run in runs if run.valid)
    return 0 if all(run.valid for run in runs) and defects == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
