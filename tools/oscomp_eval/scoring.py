"""Aggregate judge outputs into local OSComp scores."""

from __future__ import annotations

import json
import math
from pathlib import Path, PurePosixPath
from typing import Any, Iterable

from .markers import write_json
from .schemas import JudgeSummary, ScoreSummary


ROW_NORMALIZED_FIELDS = ("pass", "all", "result", "res", "baseline", "score")


class ScoringError(RuntimeError):
    """Raised for malformed scoring inputs."""


def ltp_curve(raw: float) -> float:
    clamped = max(0.0, min(float(raw), 10000.0))
    return 500.0 * math.log10(1.0 + 9.0 * clamped / 10000.0)


def numeric_value(value: Any) -> float | None:
    if isinstance(value, bool):
        return float(value)
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError:
            return None
    return None


def normalize_row(row: dict[str, Any]) -> dict[str, Any]:
    normalized = dict(row)
    for field in ROW_NORMALIZED_FIELDS:
        if field not in normalized:
            continue
        value = numeric_value(normalized[field])
        if value is not None:
            normalized[field] = value
    return normalized


def row_score(row: Any) -> tuple[float, str | None]:
    if not isinstance(row, dict):
        return 0.0, "judge row is not an object"

    row = normalize_row(row)

    if "score" in row:
        score = numeric_value(row["score"])
        if score is None:
            return 0.0, f"row {row.get('name', '<unnamed>')} has nonnumeric score"
        return score, None

    if "pass" in row:
        score = numeric_value(row["pass"])
        if score is None:
            return 0.0, f"row {row.get('name', '<unnamed>')} has nonnumeric pass"
        return score, "used pass as score because row has no explicit score"

    return 0.0, f"row {row.get('name', '<unnamed>')} has no score or pass field"


def _summary_to_dict(summary: JudgeSummary | dict[str, Any]) -> dict[str, Any]:
    if isinstance(summary, JudgeSummary):
        return summary.to_json_dict()
    if isinstance(summary, dict):
        return summary
    raise ScoringError(f"unsupported judge summary type: {type(summary).__name__}")


def _run_relative_artifact_path(arch: str, value: Any) -> str | None:
    if not isinstance(value, str) or not value:
        return None
    normalized = value.replace("\\", "/")
    if normalized.startswith("/") or "://" in normalized:
        return normalized
    parts = PurePosixPath(normalized).parts
    if parts and parts[0] in {"rv", "la"}:
        return normalized
    return str(PurePosixPath(arch) / normalized)


def score_judge_summaries(
    summaries: Iterable[JudgeSummary | dict[str, Any]],
) -> ScoreSummary:
    non_ltp_score = 0.0
    ltp_raw_total = 0.0
    arch_totals: dict[str, dict[str, float]] = {}
    libc_totals: dict[str, dict[str, float]] = {}
    ltp_raw_by_group: dict[str, float] = {}
    ltp_libc_by_group: dict[str, str] = {}
    group_totals: dict[str, dict[str, Any]] = {}
    issues: list[dict[str, Any]] = []

    for summary_obj in summaries:
        summary = _summary_to_dict(summary_obj)
        arch = str(summary.get("arch", ""))
        if not arch:
            raise ScoringError("judge summary is missing arch")
        arch_total = arch_totals.setdefault(
            arch,
            {
                "non_ltp_score": 0.0,
                "ltp_raw_total": 0.0,
            },
        )
        marker_issues = summary.get("marker_issues", [])
        if isinstance(marker_issues, list):
            for marker_issue in marker_issues:
                if not isinstance(marker_issue, dict):
                    continue
                issue = dict(marker_issue)
                issue["kind"] = f"marker-{issue.get('kind', 'issue')}"
                issue["arch"] = arch
                issue.setdefault("group_id", issue.get("group", "<run>"))
                issues.append(issue)

        for result in summary.get("results", []):
            if not isinstance(result, dict):
                issues.append(
                    {
                        "kind": "malformed-judge-result",
                        "arch": arch,
                        "message": "judge result is not an object",
                    }
                )
                continue

            group = str(result.get("group", ""))
            group_id = str(result.get("group_id", ""))
            libc = str(result.get("libc", "")) or "<unknown>"
            status = str(result.get("status", ""))
            key = f"{arch}/{group_id}" if group_id else f"{arch}/<unknown>"
            libc_total = libc_totals.setdefault(
                libc,
                {
                    "non_ltp_score": 0.0,
                    "ltp_raw_total": 0.0,
                    "ltp_score": 0.0,
                },
            )
            rows = result.get("rows", [])
            if not isinstance(rows, list):
                rows = []
                issues.append(
                    {
                        "kind": "malformed-rows",
                        "arch": arch,
                        "group_id": group_id,
                        "message": "judge rows field is not a list",
                    }
                )

            raw_score = 0.0
            row_warnings: list[str] = []
            for row in rows:
                score, warning = row_score(row)
                raw_score += score
                if warning is not None:
                    row_warnings.append(warning)

            if status != "ok":
                issues.append(
                    {
                        "kind": "judge-status",
                        "arch": arch,
                        "group_id": group_id,
                        "status": status,
                    }
                )

            if row_warnings:
                issues.append(
                    {
                        "kind": "row-score-warning",
                        "arch": arch,
                        "group_id": group_id,
                        "warnings": row_warnings,
                    }
                )

            is_ltp = group.lower() == "ltp" or group_id.startswith("ltp-")
            contribution = 0.0 if is_ltp else raw_score
            if is_ltp:
                ltp_raw_total += raw_score
                arch_total["ltp_raw_total"] += raw_score
                libc_total["ltp_raw_total"] += raw_score
                ltp_raw_by_group[group_id] = ltp_raw_by_group.get(group_id, 0.0) + raw_score
                ltp_libc_by_group[group_id] = libc
            else:
                non_ltp_score += raw_score
                arch_total["non_ltp_score"] += raw_score
                libc_total["non_ltp_score"] += raw_score

            group_totals[key] = {
                "arch": arch,
                "group": group,
                "group_id": group_id,
                "status": status,
                "row_count": len(rows),
                "raw_score": raw_score,
                "score_contribution": contribution,
                "json_path": _run_relative_artifact_path(arch, result.get("json_path")),
            }

    ltp_group_totals: dict[str, dict[str, float]] = {}
    for group_id, raw_score in sorted(ltp_raw_by_group.items()):
        contribution = ltp_curve(raw_score)
        ltp_group_totals[group_id] = {
            "raw_score": raw_score,
            "score_contribution": contribution,
        }
        libc = ltp_libc_by_group.get(group_id, "<unknown>")
        libc_total = libc_totals.setdefault(
            libc,
            {
                "non_ltp_score": 0.0,
                "ltp_raw_total": 0.0,
                "ltp_score": 0.0,
            },
        )
        libc_total["ltp_score"] += contribution

    ltp_score = sum(group["score_contribution"] for group in ltp_group_totals.values())
    total_score = non_ltp_score + ltp_score
    return ScoreSummary(
        total_score=total_score,
        non_ltp_score=non_ltp_score,
        ltp_raw_total=ltp_raw_total,
        ltp_score=ltp_score,
        arch_totals=arch_totals,
        libc_totals=libc_totals,
        ltp_group_totals=ltp_group_totals,
        group_totals=group_totals,
        issues=tuple(issues),
    )


def load_judge_summary(path: Path) -> dict[str, Any]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        raise ScoringError(f"could not read judge summary: {path}") from error
    except json.JSONDecodeError as error:
        raise ScoringError(f"judge summary is not valid JSON: {path}") from error

    if data.get("schema") != "oscomp-eval.judge-summary.v1":
        raise ScoringError(f"unsupported judge summary schema: {data.get('schema')}")
    return data


def score_summary_files(paths: Iterable[Path]) -> ScoreSummary:
    return score_judge_summaries(load_judge_summary(path) for path in paths)


def write_score_summary(summary: ScoreSummary, out_path: Path) -> None:
    write_json(out_path, summary.to_json_dict())
