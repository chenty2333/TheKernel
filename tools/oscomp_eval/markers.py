"""Parse OSComp evaluator group markers from serial logs."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Iterable

from .schemas import (
    MARKER_ARTIFACT_SCHEMA,
    LogEvent,
    MarkerEvent,
    MarkerIssue,
    MarkerParseResult,
    SEGMENT_RECORD_SCHEMA,
    Segment,
)


CANONICAL_GROUPS = {
    "basic",
    "busybox",
    "cyclictest",
    "iozone",
    "iperf",
    "libcbench",
    "libctest",
    "lmbench",
    "ltp",
    "lua",
    "netperf",
    "unixbench",
}

KNOWN_LIBCS = {"musl", "glibc"}

GROUP_RE = re.compile(r"^#### OS COMP TEST GROUP (START|END) ([^ ]+) ####$")
CONCLUSION_RE = re.compile(
    r"(QEMU timed out after|OSCOMP RUNNER (?:GLOBAL )?TIMEOUT|poweroff|shutdown|System is shutting down)",
    re.IGNORECASE,
)
PANIC_RE = re.compile(
    r"(\bkernel panic\b|\bpanic:|\bpanicked at\b|\bpanicked\b|fatal exception)",
    re.IGNORECASE,
)
TIMEOUT_RE = re.compile(
    r"(QEMU timed out after|OSCOMP RUNNER (?:GLOBAL )?TIMEOUT)",
    re.IGNORECASE,
)


class MarkerError(ValueError):
    """Raised for invalid marker parser inputs."""


def split_group_name(group: str) -> tuple[str, str | None, MarkerIssue | None]:
    """Return base group, libc suffix, and an optional suffix issue."""

    if "-" not in group:
        return group, None, None

    base_group, suffix = group.rsplit("-", 1)
    if suffix in KNOWN_LIBCS:
        return base_group, suffix, None

    if base_group in CANONICAL_GROUPS:
        return (
            base_group,
            suffix,
            MarkerIssue(
                kind="unknown-libc",
                message=f"group {group} has unknown libc suffix: {suffix}",
                group=group,
            ),
        )

    return group, None, None


def _body_text(lines: list[str], start_index: int, end_index: int) -> str:
    if start_index >= end_index:
        return ""
    return "\n".join(lines[start_index:end_index]) + "\n"


def _make_segment(
    *,
    arch: str,
    group: str,
    sequence: int,
    status: str,
    start_line: int,
    end_line: int | None,
    body_start_line: int,
    body_end_line: int,
    body: str,
) -> Segment:
    base_group, libc, _ = split_group_name(group)
    return Segment(
        arch=arch,
        group=group,
        base_group=base_group,
        libc=libc,
        sequence=sequence,
        status=status,  # type: ignore[arg-type]
        start_line=start_line,
        end_line=end_line,
        body_start_line=body_start_line,
        body_end_line=body_end_line,
        body=body,
    )


def parse_text(
    text: str,
    *,
    arch: str = "",
    log_path: str | None = None,
    require_conclusion: bool = False,
) -> MarkerParseResult:
    lines = text.splitlines()
    issues: list[MarkerIssue] = []
    marker_events: list[MarkerEvent] = []
    segments: list[Segment] = []
    log_events: list[LogEvent] = []
    sequence_counts: dict[str, int] = {}
    current: tuple[str, int, int] | None = None

    for index, line in enumerate(lines):
        line_no = index + 1

        if PANIC_RE.search(line):
            log_events.append(LogEvent(kind="panic", line=line_no, text=line))
        elif TIMEOUT_RE.search(line):
            log_events.append(LogEvent(kind="timeout", line=line_no, text=line))

        match = GROUP_RE.match(line)
        if not match:
            continue

        action = match.group(1)
        group = match.group(2)
        marker_events.append(MarkerEvent(action=action, group=group, line=line_no))

        base_group, _libc, suffix_issue = split_group_name(group)
        if suffix_issue is not None:
            issues.append(
                MarkerIssue(
                    kind=suffix_issue.kind,
                    severity=suffix_issue.severity,
                    message=suffix_issue.message,
                    line=line_no,
                    group=group,
                )
            )
        if base_group not in CANONICAL_GROUPS:
            issues.append(
                MarkerIssue(
                    kind="unknown-group",
                    line=line_no,
                    group=group,
                    message=(
                        "score-facing group marker has unknown base group: "
                        f"{group}"
                    ),
                )
            )

        if action == "START":
            if current is not None:
                open_group, open_line, body_start_index = current
                issues.append(
                    MarkerIssue(
                        kind="nested-start",
                        line=line_no,
                        group=group,
                        message=(
                            f"group {group} starts before {open_group} "
                            f"from line {open_line} ends"
                        ),
                    )
                )
                sequence_counts[open_group] = sequence_counts.get(open_group, 0) + 1
                body_end_line = max(open_line, line_no - 1)
                segments.append(
                    _make_segment(
                        arch=arch,
                        group=open_group,
                        sequence=sequence_counts[open_group],
                        status="incomplete",
                        start_line=open_line,
                        end_line=None,
                        body_start_line=open_line + 1,
                        body_end_line=body_end_line,
                        body=_body_text(lines, body_start_index, index),
                    )
                )
            current = (group, line_no, index + 1)
            continue

        if current is None:
            issues.append(
                MarkerIssue(
                    kind="end-without-start",
                    line=line_no,
                    group=group,
                    message=f"group {group} ends without a start",
                )
            )
            continue

        open_group, open_line, body_start_index = current
        if group != open_group:
            issues.append(
                MarkerIssue(
                    kind="mismatched-end",
                    line=line_no,
                    group=group,
                    message=(
                        f"group {group} ends but open group is {open_group} "
                        f"from line {open_line}"
                    ),
                )
            )
            sequence_counts[open_group] = sequence_counts.get(open_group, 0) + 1
            segments.append(
                _make_segment(
                    arch=arch,
                    group=open_group,
                    sequence=sequence_counts[open_group],
                    status="incomplete",
                    start_line=open_line,
                    end_line=None,
                    body_start_line=open_line + 1,
                    body_end_line=max(open_line, line_no - 1),
                    body=_body_text(lines, body_start_index, index),
                )
            )
            current = None
            continue

        sequence_counts[group] = sequence_counts.get(group, 0) + 1
        segments.append(
            _make_segment(
                arch=arch,
                group=group,
                sequence=sequence_counts[group],
                status="complete",
                start_line=open_line,
                end_line=line_no,
                body_start_line=open_line + 1,
                body_end_line=max(open_line, line_no - 1),
                body=_body_text(lines, body_start_index, index),
            )
        )
        current = None

    if current is not None:
        open_group, open_line, body_start_index = current
        issues.append(
            MarkerIssue(
                kind="start-without-end",
                line=open_line,
                group=open_group,
                message=f"group {open_group} starts without a matching end",
            )
        )
        sequence_counts[open_group] = sequence_counts.get(open_group, 0) + 1
        segments.append(
            _make_segment(
                arch=arch,
                group=open_group,
                sequence=sequence_counts[open_group],
                status="incomplete",
                start_line=open_line,
                end_line=None,
                body_start_line=open_line + 1,
                body_end_line=max(open_line, len(lines)),
                body=_body_text(lines, body_start_index, len(lines)),
            )
        )

    if text.strip() and not any(segment.status == "complete" for segment in segments):
        issues.append(
            MarkerIssue(
                kind="zero-complete-groups",
                message="log has output but zero complete evaluator groups",
            )
        )

    conclusion_found = bool(CONCLUSION_RE.search(text))
    if require_conclusion and not conclusion_found:
        issues.append(
            MarkerIssue(
                kind="missing-conclusion",
                message="log has no visible timeout/shutdown conclusion",
            )
        )

    return MarkerParseResult(
        arch=arch,
        log_path=log_path,
        marker_events=tuple(marker_events),
        segments=tuple(segments),
        issues=tuple(issues),
        log_events=tuple(log_events),
        conclusion_found=conclusion_found,
    )


def parse_log(
    log_path: Path,
    *,
    arch: str = "",
    require_conclusion: bool = False,
) -> MarkerParseResult:
    if not log_path.is_file():
        raise MarkerError(f"log not found: {log_path}")

    text = log_path.read_text(encoding="utf-8", errors="replace")
    return parse_text(
        text,
        arch=arch,
        log_path=str(log_path),
        require_conclusion=require_conclusion,
    )


def segment_file_name(segment: Segment) -> str:
    if segment.sequence == 1:
        return f"{segment.group}.txt"
    return f"{segment.group}.{segment.sequence}.txt"


def write_json(path: Path, data: object) -> None:
    tmp_path = path.with_name(f".{path.name}.tmp")
    tmp_path.write_text(
        json.dumps(data, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    tmp_path.replace(path)


def write_artifacts(result: MarkerParseResult, out_dir: Path) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    segments_dir = out_dir / "segments"
    segments_dir.mkdir(parents=True, exist_ok=True)

    segment_rows: list[dict[str, object]] = []
    for segment in result.segments:
        file_name = segment_file_name(segment)
        body_path = segments_dir / file_name
        body_path.write_text(segment.body, encoding="utf-8")
        row = segment.to_json_dict(
            include_body=False,
            body_path=str(Path("segments") / file_name),
        )
        row["schema"] = SEGMENT_RECORD_SCHEMA
        segment_rows.append(row)

    segments_jsonl = out_dir / "segments.jsonl"
    tmp_jsonl = segments_jsonl.with_name(f".{segments_jsonl.name}.tmp")
    with tmp_jsonl.open("w", encoding="utf-8") as file:
        for row in segment_rows:
            file.write(json.dumps(row, sort_keys=True) + "\n")
    tmp_jsonl.replace(segments_jsonl)

    validation = result.to_json_dict(include_bodies=False)
    validation["schema"] = MARKER_ARTIFACT_SCHEMA
    validation["segments_jsonl"] = "segments.jsonl"
    write_json(out_dir / "marker-validation.json", validation)


def compatible_summary(result: MarkerParseResult) -> str:
    label = f" arch={result.arch}" if result.arch else ""
    lines = [
        (
            f"oscomp-output{label} markers={result.marker_count} "
            f"complete_groups={result.complete_count} issues={len(result.issues)}"
        )
    ]
    for segment in result.complete_segments():
        lines.append(f"  complete {segment.group} lines={segment.start_line}-{segment.end_line}")

    if result.issues:
        lines.append("issues:")
        for issue in result.issues:
            lines.append(f"  - {issue.compatible_text()}")

    return "\n".join(lines)


def iter_segment_bodies(result: MarkerParseResult) -> Iterable[tuple[Segment, str]]:
    for segment in result.segments:
        yield segment, segment.body
