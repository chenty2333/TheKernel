"""Build a machine-readable index for one OSComp evaluation run."""

from __future__ import annotations

import json
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

from .markers import write_json


ARTIFACT_INDEX_SCHEMA = "oscomp-eval.artifact-index.v1"


@dataclass(frozen=True)
class ArtifactIndexResult:
    path: Path
    artifact_count: int


def _json_schema(path: Path) -> str | None:
    try:
        if path.suffix == ".jsonl":
            with path.open("r", encoding="utf-8") as file:
                for line in file:
                    if line.strip():
                        data = json.loads(line)
                        break
                else:
                    return None
        else:
            data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return None
    if not isinstance(data, dict):
        return None
    schema = data.get("schema")
    return str(schema) if schema is not None else None


def _artifact_row(run_dir: Path, path: Path, kind: str) -> dict[str, Any] | None:
    try:
        stat = path.stat()
    except OSError:
        return None
    relpath = path.relative_to(run_dir).as_posix()
    row: dict[str, Any] = {
        "path": relpath,
        "kind": kind,
        "size_bytes": stat.st_size,
    }
    if path.suffix in {".json", ".jsonl"}:
        schema = _json_schema(path)
        if schema is not None:
            row["schema"] = schema
    parts = path.relative_to(run_dir).parts
    if parts and parts[0] in {"rv", "la"}:
        row["arch"] = parts[0]
    if len(parts) >= 3 and parts[1] == "judges":
        row["group_id"] = Path(parts[-1]).stem
    if len(parts) >= 3 and parts[1] == "segments":
        row["group_id"] = Path(parts[-1]).stem
    return row


def _existing(paths: Iterable[tuple[Path, str]]) -> list[tuple[Path, str]]:
    return [(path, kind) for path, kind in paths if path.is_file()]


def collect_artifacts(run_dir: Path) -> list[dict[str, Any]]:
    """Return indexed artifact rows without scanning replay workdirs."""

    candidates: list[tuple[Path, str]] = [
        (run_dir / "manifest.json", "run-manifest"),
        (run_dir / "score.json", "score-summary"),
        (run_dir / "report.md", "markdown-report"),
        (run_dir / "artifact-index.json", "artifact-index"),
    ]

    inputs_dir = run_dir / "inputs"
    if inputs_dir.is_dir():
        candidates.extend((path, "input") for path in sorted(inputs_dir.iterdir()))

    for arch_dir in sorted(path for path in run_dir.iterdir() if path.is_dir()):
        if arch_dir.name not in {"rv", "la"}:
            continue
        candidates.extend(
            [
                (arch_dir / "console.log", "console-log"),
                (arch_dir / "marker-validation.json", "marker-validation"),
                (arch_dir / "segments.jsonl", "segments-jsonl"),
                (arch_dir / "judge-summary.json", "judge-summary"),
            ]
        )
        segments_dir = arch_dir / "segments"
        if segments_dir.is_dir():
            candidates.extend((path, "segment") for path in sorted(segments_dir.iterdir()))
        judges_dir = arch_dir / "judges"
        if judges_dir.is_dir():
            for path in sorted(judges_dir.iterdir()):
                if path.suffix == ".stdout":
                    kind = "judge-stdout"
                elif path.suffix == ".stderr":
                    kind = "judge-stderr"
                elif path.suffix == ".json":
                    kind = "judge-json"
                else:
                    kind = "judge-artifact"
                candidates.append((path, kind))

    rows = [
        row
        for path, kind in _existing(candidates)
        for row in [_artifact_row(run_dir, path, kind)]
        if row is not None
    ]
    return sorted(rows, key=lambda row: str(row["path"]))


def write_artifact_index(run_dir: Path) -> ArtifactIndexResult:
    path = run_dir / "artifact-index.json"
    created_at = datetime.now(timezone.utc).isoformat()
    payload: dict[str, object] | None = None
    artifacts: list[dict[str, Any]] = []

    for _ in range(5):
        artifacts = collect_artifacts(run_dir)
        payload = {
            "schema": ARTIFACT_INDEX_SCHEMA,
            "created_at": created_at,
            "artifact_count": len(artifacts),
            "artifacts": artifacts,
        }
        write_json(path, payload)
        try:
            current_size = path.stat().st_size
        except OSError:
            continue
        self_row = next(
            (
                artifact
                for artifact in artifacts
                if artifact.get("path") == "artifact-index.json"
            ),
            None,
        )
        if self_row is not None and self_row.get("size_bytes") == current_size:
            break

    if payload is None:
        artifacts = collect_artifacts(run_dir)
        payload = {
            "schema": ARTIFACT_INDEX_SCHEMA,
            "created_at": created_at,
            "artifact_count": len(artifacts),
            "artifacts": artifacts,
        }
        write_json(path, payload)
    return ArtifactIndexResult(path=path, artifact_count=len(artifacts))
