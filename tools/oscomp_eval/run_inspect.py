"""Read-only inspection for local OSComp evaluation run directories."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .schemas import (
    JUDGE_RESULT_SCHEMA,
    JUDGE_SUMMARY_SCHEMA,
    MARKER_ARTIFACT_SCHEMA,
    RUN_INSPECTION_SCHEMA,
    SCORE_SUMMARY_SCHEMA,
    SEGMENT_RECORD_SCHEMA,
)


class RunInspectError(RuntimeError):
    """Raised when a run directory cannot be inspected."""


@dataclass(frozen=True)
class RunInspectResult:
    run_dir: Path
    run_status: str
    artifact_count: int
    structural_issues: tuple[str, ...]
    score_issue_count: int

    @property
    def ok(self) -> bool:
        return (
            not self.structural_issues
            and self.score_issue_count == 0
            and self.run_status == "complete"
        )

    def to_json_dict(self) -> dict[str, object]:
        return {
            "schema": RUN_INSPECTION_SCHEMA,
            "ok": self.ok,
            "run_dir": str(self.run_dir),
            "status": self.run_status,
            "artifact_count": self.artifact_count,
            "structural_issue_count": len(self.structural_issues),
            "structural_issues": list(self.structural_issues),
            "score_issue_count": self.score_issue_count,
        }


def _load_json(path: Path) -> tuple[dict[str, Any] | None, str | None]:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        return None, str(error)
    except json.JSONDecodeError as error:
        return None, f"invalid JSON: {error}"
    if not isinstance(data, dict):
        return None, "JSON root is not an object"
    return data, None


def _schema_issue(path: Path, data: dict[str, Any] | None, expected: str) -> str | None:
    if data is None:
        return None
    actual = data.get("schema")
    if actual != expected:
        shown = actual if actual is not None else "<missing>"
        return f"{path.name} schema {shown} != {expected}"
    return None


def _expected_file_schema(relpath: str) -> str | None:
    parts = Path(relpath).parts
    if relpath == "score.json":
        return SCORE_SUMMARY_SCHEMA
    if len(parts) == 2 and parts[0] in {"rv", "la"} and parts[1] == "marker-validation.json":
        return MARKER_ARTIFACT_SCHEMA
    if len(parts) == 2 and parts[0] in {"rv", "la"} and parts[1] == "segments.jsonl":
        return SEGMENT_RECORD_SCHEMA
    if len(parts) == 2 and parts[0] in {"rv", "la"} and parts[1] == "judge-summary.json":
        return JUDGE_SUMMARY_SCHEMA
    if len(parts) == 3 and parts[0] in {"rv", "la"} and parts[1] == "judges" and parts[2].endswith(".json"):
        return JUDGE_RESULT_SCHEMA
    return None


def _artifact_file_schema(path: Path) -> tuple[str | None, str | None]:
    try:
        if path.suffix == ".jsonl":
            with path.open("r", encoding="utf-8") as file:
                for line in file:
                    if line.strip():
                        data = json.loads(line)
                        break
                else:
                    return None, "JSONL has no rows"
        else:
            data = json.loads(path.read_text(encoding="utf-8"))
    except OSError as error:
        return None, str(error)
    except json.JSONDecodeError as error:
        return None, f"invalid JSON: {error}"
    if not isinstance(data, dict):
        return None, "JSON root is not an object"
    schema = data.get("schema")
    return str(schema) if schema is not None else None, None


def inspect_run(run_dir: Path) -> RunInspectResult:
    if not run_dir.is_dir():
        raise RunInspectError(f"run directory not found: {run_dir}")

    structural_issues: list[str] = []
    score_path = run_dir / "score.json"

    score, error = _load_json(score_path)
    if error is not None:
        structural_issues.append(f"score.json: {error}")

    issue = _schema_issue(score_path, score, SCORE_SUMMARY_SCHEMA)
    if issue is not None:
        structural_issues.append(issue)

    artifact_count = 0
    for path in sorted(item for item in run_dir.rglob("*") if item.is_file()):
        relpath = path.relative_to(run_dir).as_posix()
        if relpath in {"manifest.json", "artifact-index.json"}:
            continue
        artifact_count += 1
        expected_schema = _expected_file_schema(relpath)
        if expected_schema is not None:
            file_schema, schema_error = _artifact_file_schema(path)
            if schema_error is not None:
                structural_issues.append(f"artifact schema unreadable: {relpath}: {schema_error}")
            elif file_schema != expected_schema:
                shown = file_schema if file_schema is not None else "<missing>"
                structural_issues.append(
                    f"artifact file schema {relpath} {shown} != {expected_schema}"
                )

    score_issue_count = 0
    if score is not None:
        issues = score.get("issues", [])
        if isinstance(issues, list):
            score_issue_count = len(issues)
        else:
            structural_issues.append("score.json issues is not a list")

    run_status = "incomplete" if score_issue_count else "complete"
    if score is None:
        run_status = "<unknown>"

    return RunInspectResult(
        run_dir=run_dir,
        run_status=run_status,
        artifact_count=artifact_count,
        structural_issues=tuple(structural_issues),
        score_issue_count=score_issue_count,
    )
