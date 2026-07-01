"""Official evaluator snapshot provenance helpers."""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .paths import official_root


OFFICIAL_SNAPSHOT_SCHEMA = "oscomp-eval.official-snapshot.v1"


class ProvenanceError(RuntimeError):
    """Raised for malformed official snapshot provenance."""


@dataclass(frozen=True)
class OfficialSnapshot:
    schema: str
    repo: str
    commit: str
    source_path: str
    imported_at: str
    files: tuple[str, ...]
    local_patches: tuple[str, ...]
    license_note: str
    source_status: str = ""
    changes: dict[str, tuple[str, ...]] | None = None

    def to_json_dict(self) -> dict[str, Any]:
        source = {
            "repo": self.repo,
            "commit": self.commit,
            "source_path": self.source_path,
            "imported_at": self.imported_at,
            "license_note": self.license_note,
        }
        if self.source_status:
            source["source_status"] = self.source_status
        data: dict[str, Any] = {
            "schema": self.schema,
            "source": source,
            "files": list(self.files),
            "local_patches": list(self.local_patches),
        }
        if self.changes is not None:
            data["changes"] = {
                key: list(value)
                for key, value in self.changes.items()
            }
        return data


def manifest_path(root: Path | None = None) -> Path:
    return official_root(root) / "manifest.json"


def _write_json_atomic(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = path.with_name(f".{path.name}.tmp")
    tmp_path.write_text(
        json.dumps(data, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    tmp_path.replace(path)


def load_official_snapshot(path: Path | None = None) -> OfficialSnapshot:
    manifest = path or manifest_path()
    try:
        data = json.loads(manifest.read_text(encoding="utf-8"))
    except OSError as error:
        raise ProvenanceError(f"could not read official manifest: {manifest}") from error
    except json.JSONDecodeError as error:
        raise ProvenanceError(f"official manifest is not valid JSON: {manifest}") from error

    if data.get("schema") != OFFICIAL_SNAPSHOT_SCHEMA:
        raise ProvenanceError(
            f"unsupported official manifest schema: {data.get('schema')}"
        )

    source = data.get("source")
    if not isinstance(source, dict):
        raise ProvenanceError("official manifest source must be an object")

    files = data.get("files", [])
    if not isinstance(files, list) or not all(isinstance(item, str) for item in files):
        raise ProvenanceError("official manifest files must be a list of strings")

    patches = data.get("local_patches", [])
    if not isinstance(patches, list) or not all(isinstance(item, str) for item in patches):
        raise ProvenanceError("official manifest local_patches must be a list of strings")

    return OfficialSnapshot(
        schema=data["schema"],
        repo=str(source.get("repo", "")),
        commit=str(source.get("commit", "")),
        source_path=str(source.get("source_path", "")),
        imported_at=str(source.get("imported_at", "")),
        files=tuple(files),
        local_patches=tuple(patches),
        license_note=str(source.get("license_note", "")),
        source_status=str(source.get("source_status", "")),
        changes=_parse_changes(data.get("changes")),
    )


def _parse_changes(value: Any) -> dict[str, tuple[str, ...]] | None:
    if value is None:
        return None
    if not isinstance(value, dict):
        raise ProvenanceError("official manifest changes must be an object")
    changes: dict[str, tuple[str, ...]] = {}
    for key in ("added", "removed", "changed"):
        items = value.get(key, [])
        if not isinstance(items, list) or not all(isinstance(item, str) for item in items):
            raise ProvenanceError(f"official manifest changes.{key} must be a list of strings")
        changes[key] = tuple(items)
    return changes


def _git_value(source_path: Path, args: list[str]) -> str:
    try:
        return subprocess.check_output(
            ["git", "-C", str(source_path), *args],
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (OSError, subprocess.CalledProcessError):
        return ""


def _source_repo(source_path: Path) -> str:
    return _git_value(source_path, ["config", "--get", "remote.origin.url"])


def _source_commit(source_path: Path) -> str:
    return _git_value(source_path, ["rev-parse", "HEAD"])


def _source_status(source_path: Path) -> str:
    if _git_value(source_path, ["rev-parse", "--is-inside-work-tree"]) != "true":
        return "unknown"
    status = _git_value(source_path, ["status", "--short"])
    return "dirty" if status else "clean"


def _license_note(source_path: Path) -> str:
    license_files = [
        name
        for name in ("LICENSE", "COPYING", "NOTICE")
        if (source_path / name).is_file()
    ]
    if license_files:
        return "Source checkout contains top-level notice files: " + ", ".join(license_files) + "."
    return "The inspected source checkout did not contain a top-level LICENSE, COPYING, or NOTICE file."


def _source_judge_files(source_path: Path) -> list[Path]:
    judge_dir = source_path / "kernel" / "judge"
    if not judge_dir.is_dir():
        raise ProvenanceError(f"official judge directory not found: {judge_dir}")

    files = sorted(judge_dir.glob("judge_*.py"))
    config = judge_dir / "config.json"
    if config.is_file():
        files.insert(0, config)
    if not files:
        raise ProvenanceError(f"no official judge files found in: {judge_dir}")
    return files


def refresh_official_snapshot(
    source_path: Path,
    *,
    root: Path | None = None,
    repo: str | None = None,
    commit: str | None = None,
    imported_at: str | None = None,
    allow_dirty: bool = False,
) -> OfficialSnapshot:
    source_path = source_path.expanduser().resolve()
    source_files = _source_judge_files(source_path)
    source_status = _source_status(source_path)
    if source_status == "dirty" and not allow_dirty:
        raise ProvenanceError(
            f"official source checkout is dirty: {source_path}; pass --allow-dirty to record it"
        )

    destination_root = official_root(root)
    destination_judge_dir = destination_root / "judge"
    old_files = set(load_official_snapshot(manifest_path(root)).files) if manifest_path(root).is_file() else set()
    old_bytes: dict[str, bytes] = {}
    for rel in old_files:
        path = destination_root / rel
        if path.is_file():
            old_bytes[rel] = path.read_bytes()

    destination_judge_dir.mkdir(parents=True, exist_ok=True)
    new_files: list[str] = []
    changed: list[str] = []
    added: list[str] = []
    for source_file in source_files:
        rel = f"judge/{source_file.name}"
        target = destination_root / rel
        data = source_file.read_bytes()
        if rel not in old_files:
            added.append(rel)
        elif old_bytes.get(rel) != data:
            changed.append(rel)
        target.write_bytes(data)
        new_files.append(rel)

    removed = sorted(old_files - set(new_files))
    for rel in removed:
        target = destination_root / rel
        if target.is_file():
            target.unlink()

    snapshot = OfficialSnapshot(
        schema=OFFICIAL_SNAPSHOT_SCHEMA,
        repo=repo or _source_repo(source_path),
        commit=commit or _source_commit(source_path),
        source_path=str(source_path),
        imported_at=imported_at or datetime.now(timezone.utc).date().isoformat(),
        files=tuple(sorted(new_files)),
        local_patches=(),
        license_note=_license_note(source_path),
        source_status=source_status,
        changes={
            "added": tuple(sorted(added)),
            "removed": tuple(removed),
            "changed": tuple(sorted(changed)),
        },
    )
    _write_json_atomic(manifest_path(root), snapshot.to_json_dict())
    return snapshot
