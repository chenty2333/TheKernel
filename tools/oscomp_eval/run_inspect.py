"""Read-only inspection for local OSComp evaluation run directories."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .artifact_index import ARTIFACT_INDEX_SCHEMA
from .schemas import (
    JUDGE_RESULT_SCHEMA,
    JUDGE_SUMMARY_SCHEMA,
    MARKER_ARTIFACT_SCHEMA,
    RUN_INSPECTION_SCHEMA,
    RUN_MANIFEST_SCHEMA,
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


def _artifact_rows(index: dict[str, Any] | None) -> tuple[list[dict[str, Any]], str | None]:
    if index is None:
        return [], None
    artifacts = index.get("artifacts", [])
    if not isinstance(artifacts, list):
        return [], "artifact-index.json artifacts is not a list"
    rows: list[dict[str, Any]] = []
    for artifact in artifacts:
        if not isinstance(artifact, dict):
            return rows, "artifact-index.json contains a non-object artifact row"
        rows.append(artifact)
    return rows, None


def _expected_artifact_schema(relpath: str) -> str | None:
    parts = Path(relpath).parts
    if relpath == "manifest.json":
        return RUN_MANIFEST_SCHEMA
    if relpath == "score.json":
        return SCORE_SUMMARY_SCHEMA
    if relpath == "artifact-index.json":
        return ARTIFACT_INDEX_SCHEMA
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
    manifest_path = run_dir / "manifest.json"
    score_path = run_dir / "score.json"
    report_path = run_dir / "report.md"
    stale_html_path = run_dir / "report.html"
    index_path = run_dir / "artifact-index.json"

    manifest, error = _load_json(manifest_path)
    if error is not None:
        structural_issues.append(f"manifest.json: {error}")
    score, error = _load_json(score_path)
    if error is not None:
        structural_issues.append(f"score.json: {error}")
    index, error = _load_json(index_path)
    if error is not None:
        structural_issues.append(f"artifact-index.json: {error}")
    if not report_path.is_file():
        structural_issues.append("report.md is missing")
    if stale_html_path.exists() or stale_html_path.is_symlink():
        structural_issues.append(
            "report.html is stale; report.md is the only supported human-readable report"
        )

    for path, data, expected in (
        (manifest_path, manifest, RUN_MANIFEST_SCHEMA),
        (score_path, score, SCORE_SUMMARY_SCHEMA),
        (index_path, index, ARTIFACT_INDEX_SCHEMA),
    ):
        issue = _schema_issue(path, data, expected)
        if issue is not None:
            structural_issues.append(issue)

    artifact_rows, artifact_issue = _artifact_rows(index)
    if artifact_issue is not None:
        structural_issues.append(artifact_issue)

    seen_paths: set[str] = set()
    for artifact in artifact_rows:
        relpath = artifact.get("path")
        if not isinstance(relpath, str) or not relpath:
            structural_issues.append("artifact row has missing path")
            continue
        if relpath == "report.html":
            structural_issues.append("artifact-index contains unsupported HTML report: report.html")
        expected_schema = _expected_artifact_schema(relpath)
        if expected_schema is not None:
            actual_schema = artifact.get("schema")
            if actual_schema != expected_schema:
                shown = actual_schema if actual_schema is not None else "<missing>"
                structural_issues.append(
                    f"artifact-index schema {relpath} {shown} != {expected_schema}"
                )
        if relpath in seen_paths:
            structural_issues.append(f"duplicate artifact path: {relpath}")
        seen_paths.add(relpath)
        path = run_dir / relpath
        if not path.is_file():
            structural_issues.append(f"artifact is missing: {relpath}")
            continue
        size = artifact.get("size_bytes")
        if isinstance(size, int) and size != path.stat().st_size:
            structural_issues.append(
                f"artifact size mismatch: {relpath} index={size} actual={path.stat().st_size}"
            )
        if expected_schema is not None:
            file_schema, schema_error = _artifact_file_schema(path)
            if schema_error is not None:
                structural_issues.append(f"artifact schema unreadable: {relpath}: {schema_error}")
            elif file_schema != expected_schema:
                shown = file_schema if file_schema is not None else "<missing>"
                structural_issues.append(
                    f"artifact file schema {relpath} {shown} != {expected_schema}"
                )

    if index is not None:
        artifact_count = index.get("artifact_count")
        if isinstance(artifact_count, int) and artifact_count != len(artifact_rows):
            structural_issues.append(
                f"artifact_count mismatch: index={artifact_count} actual={len(artifact_rows)}"
            )

    for required in ("manifest.json", "score.json", "report.md", "artifact-index.json"):
        if required not in seen_paths:
            structural_issues.append(f"artifact-index missing required path: {required}")

    score_issue_count = 0
    if score is not None:
        issues = score.get("issues", [])
        if isinstance(issues, list):
            score_issue_count = len(issues)
        else:
            structural_issues.append("score.json issues is not a list")

    run_status = "<unknown>"
    if manifest is not None:
        run_status = str(manifest.get("status", "<unknown>"))

    return RunInspectResult(
        run_dir=run_dir,
        run_status=run_status,
        artifact_count=len(artifact_rows),
        structural_issues=tuple(structural_issues),
        score_issue_count=score_issue_count,
    )
