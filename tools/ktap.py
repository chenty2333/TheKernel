"""Single-source KTAP transcript parsing and validation.

The product system-test gate (tools/thekernel.py) and the Panther Lake DUT
gate (scripts/ci/panther_lake_dut_gate.py) both consume guest KTAP output;
this module is the only place that parses it.  ``validate_ktap_log`` enforces
the complete gating contract while ``reject_ktap_skips`` is the lighter check
for runs whose completion is established by other means.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

COMPLETION_MARKER = "# THEKERNEL_SYSTEM_TEST_COMPLETE"

TEST_LINE = re.compile(r"^(ok|not ok)\s+([1-9][0-9]*)\b", re.IGNORECASE)
PLAN_LINE = re.compile(r"^1\.\.([1-9][0-9]*)\s*$")
SKIP_LINE = re.compile(r"^ok\s+[1-9][0-9]*\b.*\s#\s*SKIP(?:\s|$)", re.IGNORECASE)


class KtapError(ValueError):
    """A KTAP transcript violates a gate."""


@dataclass(frozen=True)
class KtapTranscript:
    """The gate-relevant facts of one KTAP output transcript."""

    version: bool
    plans: tuple[int, ...]
    records: tuple[tuple[bool, int], ...]
    skips: tuple[str, ...]
    failure: bool
    suite_failure: bool
    complete: bool


def parse_ktap(text: str) -> KtapTranscript:
    lines = text.splitlines()
    return KtapTranscript(
        version=any(line.strip() == "KTAP version 1" for line in lines),
        plans=tuple(
            int(match.group(1))
            for line in lines
            if (match := PLAN_LINE.match(line.strip()))
        ),
        records=tuple(
            (match.group(1).lower() == "ok", int(match.group(2)))
            for line in lines
            if (match := TEST_LINE.match(line.strip()))
        ),
        skips=tuple(line for line in lines if SKIP_LINE.match(line)),
        failure=any(line.lower().startswith("not ok ") for line in lines),
        suite_failure=any("KTAP suite failed" in line for line in lines),
        complete=COMPLETION_MARKER in lines,
    )


def reject_ktap_skips(text: str) -> None:
    """Reject SKIP results only; the lighter product system-test gate."""

    skipped = parse_ktap(text).skips
    if skipped:
        raise KtapError(
            f"system test contains {len(skipped)} KTAP SKIP result(s); "
            "pass --allow-skip to inspect a non-gating preview run"
        )


def validate_ktap_log(text: str) -> None:
    """Enforce the complete KTAP contract for a gating serial transcript."""

    transcript = parse_ktap(text)
    if not transcript.version:
        raise KtapError("serial output does not contain KTAP version 1")
    if transcript.skips:
        raise KtapError("serial output contains a KTAP SKIP result")
    if transcript.failure:
        raise KtapError("serial output contains a failing KTAP result")
    if transcript.suite_failure:
        raise KtapError("serial output reports a KTAP suite failure")
    if len(transcript.plans) != 1:
        raise KtapError("serial output must contain exactly one KTAP plan")
    expected = transcript.plans[0]
    if sorted(number for _, number in transcript.records) != list(range(1, expected + 1)):
        raise KtapError("KTAP records do not exactly satisfy the declared plan")
    if not transcript.complete:
        raise KtapError("serial output lacks the system-test completion marker")


def clean_shutdown_attested(status_text: str) -> bool:
    """Whether a DUT serial hook attested a normal guest shutdown."""

    return status_text.strip() == "clean"
