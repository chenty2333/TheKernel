"""JSON-facing schemas for the local OSComp evaluator.

Keep these dataclasses explicit. The serialized shape is part of the local
debugging contract and should not change accidentally when implementation
details move around.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Literal


RUN_MANIFEST_SCHEMA = "oscomp-eval.run-manifest.v1"
MARKER_RESULT_SCHEMA = "oscomp-eval.marker-result.v1"
MARKER_ARTIFACT_SCHEMA = "oscomp-eval.marker-artifacts.v1"
SEGMENT_RECORD_SCHEMA = "oscomp-eval.segment-record.v1"
JUDGE_RESULT_SCHEMA = "oscomp-eval.judge-result.v1"
JUDGE_SUMMARY_SCHEMA = "oscomp-eval.judge-summary.v1"
SCORE_SUMMARY_SCHEMA = "oscomp-eval.score-summary.v1"
RUN_INSPECTION_SCHEMA = "oscomp-eval.run-inspection.v1"

IssueSeverity = Literal["error", "warning", "info"]
SegmentStatus = Literal["complete", "incomplete"]
JudgeStatus = Literal[
    "ok",
    "missing-segment",
    "missing-judge",
    "timeout",
    "bad-json",
    "nonzero-exit",
]


@dataclass(frozen=True)
class MarkerIssue:
    kind: str
    message: str
    line: int | None = None
    group: str | None = None
    severity: IssueSeverity = "error"

    def to_json_dict(self) -> dict[str, Any]:
        data: dict[str, Any] = {
            "kind": self.kind,
            "severity": self.severity,
            "message": self.message,
        }
        if self.line is not None:
            data["line"] = self.line
        if self.group is not None:
            data["group"] = self.group
        return data

    def compatible_text(self) -> str:
        if self.line is None:
            return self.message
        return f"line {self.line}: {self.message}"


@dataclass(frozen=True)
class LogEvent:
    kind: str
    line: int
    text: str

    def to_json_dict(self) -> dict[str, Any]:
        return {
            "kind": self.kind,
            "line": self.line,
            "text": self.text,
        }


@dataclass(frozen=True)
class MarkerEvent:
    action: Literal["START", "END"]
    group: str
    line: int

    def to_json_dict(self) -> dict[str, Any]:
        return {
            "action": self.action,
            "group": self.group,
            "line": self.line,
        }


@dataclass(frozen=True)
class Segment:
    arch: str
    group: str
    base_group: str
    libc: str | None
    sequence: int
    status: SegmentStatus
    start_line: int
    end_line: int | None
    body_start_line: int
    body_end_line: int
    body: str

    @property
    def identity(self) -> str:
        return self.group

    def to_json_dict(
        self,
        *,
        include_body: bool = False,
        body_path: str | None = None,
    ) -> dict[str, Any]:
        data: dict[str, Any] = {
            "arch": self.arch,
            "group": self.group,
            "base_group": self.base_group,
            "libc": self.libc,
            "sequence": self.sequence,
            "status": self.status,
            "start_line": self.start_line,
            "end_line": self.end_line,
            "body_start_line": self.body_start_line,
            "body_end_line": self.body_end_line,
            "body_line_count": max(0, self.body_end_line - self.body_start_line + 1),
        }
        if body_path is not None:
            data["body_path"] = body_path
        if include_body:
            data["body"] = self.body
        return data


@dataclass(frozen=True)
class MarkerParseResult:
    arch: str
    log_path: str | None
    marker_events: tuple[MarkerEvent, ...]
    segments: tuple[Segment, ...]
    issues: tuple[MarkerIssue, ...]
    log_events: tuple[LogEvent, ...]
    conclusion_found: bool

    @property
    def marker_count(self) -> int:
        return len(self.marker_events)

    @property
    def complete_count(self) -> int:
        return sum(1 for segment in self.segments if segment.status == "complete")

    @property
    def has_errors(self) -> bool:
        return any(issue.severity == "error" for issue in self.issues)

    def complete_segments(self) -> list[Segment]:
        return [segment for segment in self.segments if segment.status == "complete"]

    def to_json_dict(self, *, include_bodies: bool = False) -> dict[str, Any]:
        return {
            "schema": MARKER_RESULT_SCHEMA,
            "arch": self.arch,
            "log_path": self.log_path,
            "marker_count": self.marker_count,
            "complete_group_count": self.complete_count,
            "conclusion_found": self.conclusion_found,
            "markers": [event.to_json_dict() for event in self.marker_events],
            "segments": [
                segment.to_json_dict(include_body=include_bodies)
                for segment in self.segments
            ],
            "issues": [issue.to_json_dict() for issue in self.issues],
            "log_events": [event.to_json_dict() for event in self.log_events],
        }


@dataclass(frozen=True)
class JudgeGroupResult:
    arch: str
    group: str
    libc: str
    group_id: str
    status: JudgeStatus
    command: tuple[str, ...]
    judge_path: str | None
    segment_sequence: int | None
    exit_code: int | None
    duration_ms: int
    stdout_path: str | None
    stderr_path: str | None
    json_path: str | None
    rows: tuple[Any, ...]
    warnings: tuple[str, ...] = ()

    @property
    def ok(self) -> bool:
        return self.status == "ok"

    def to_json_dict(self) -> dict[str, Any]:
        return {
            "arch": self.arch,
            "group": self.group,
            "libc": self.libc,
            "group_id": self.group_id,
            "status": self.status,
            "command": list(self.command),
            "judge_path": self.judge_path,
            "segment_sequence": self.segment_sequence,
            "exit_code": self.exit_code,
            "duration_ms": self.duration_ms,
            "stdout_path": self.stdout_path,
            "stderr_path": self.stderr_path,
            "json_path": self.json_path,
            "rows": list(self.rows),
            "warnings": list(self.warnings),
        }


@dataclass(frozen=True)
class JudgeSummary:
    arch: str
    judge_dir: str
    results: tuple[JudgeGroupResult, ...]
    marker_issues: tuple[dict[str, Any], ...] = ()

    @property
    def has_errors(self) -> bool:
        return any(not result.ok for result in self.results)

    def to_json_dict(self) -> dict[str, Any]:
        return {
            "schema": JUDGE_SUMMARY_SCHEMA,
            "arch": self.arch,
            "judge_dir": self.judge_dir,
            "result_count": len(self.results),
            "ok_count": sum(1 for result in self.results if result.ok),
            "error_count": sum(1 for result in self.results if not result.ok),
            "marker_issues": list(self.marker_issues),
            "results": [result.to_json_dict() for result in self.results],
        }


@dataclass(frozen=True)
class ScoreSummary:
    total_score: float
    non_ltp_score: float
    ltp_raw_total: float
    ltp_score: float
    arch_totals: dict[str, dict[str, float]]
    libc_totals: dict[str, dict[str, float]]
    ltp_group_totals: dict[str, dict[str, float]]
    group_totals: dict[str, dict[str, Any]]
    issues: tuple[dict[str, Any], ...]

    @property
    def has_errors(self) -> bool:
        return bool(self.issues)

    def to_json_dict(self) -> dict[str, Any]:
        return {
            "schema": SCORE_SUMMARY_SCHEMA,
            "total_score": self.total_score,
            "non_ltp_score": self.non_ltp_score,
            "ltp_raw_total": self.ltp_raw_total,
            "ltp_score": self.ltp_score,
            "arch_totals": self.arch_totals,
            "libc_totals": self.libc_totals,
            "ltp_group_totals": self.ltp_group_totals,
            "group_totals": self.group_totals,
            "issues": list(self.issues),
        }
