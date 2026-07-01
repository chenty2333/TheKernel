"""Run metadata helpers for manifests and reports."""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

from .paths import repo_root
from .provenance import ProvenanceError, load_official_snapshot


def git_metadata(root: Path | None = None) -> dict[str, Any]:
    repo = root or repo_root()

    def run_git(args: list[str]) -> str:
        return subprocess.check_output(
            ["git", *args],
            cwd=repo,
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()

    try:
        commit = run_git(["rev-parse", "HEAD"])
    except (OSError, subprocess.CalledProcessError):
        return {"available": False, "repo_root": str(repo)}

    try:
        status_short = run_git(["status", "--short"])
    except (OSError, subprocess.CalledProcessError):
        status_short = ""

    return {
        "available": True,
        "repo_root": str(repo),
        "commit": commit,
        "dirty": bool(status_short),
        "status_short": status_short.splitlines(),
    }


def official_snapshot_metadata() -> dict[str, Any]:
    try:
        return load_official_snapshot().to_json_dict()
    except ProvenanceError as error:
        return {
            "schema": "oscomp-eval.official-snapshot.unavailable.v1",
            "error": str(error),
        }


def common_manifest_fields(command: list[str] | None = None) -> dict[str, Any]:
    data: dict[str, Any] = {
        "git": git_metadata(),
        "official_snapshot": official_snapshot_metadata(),
    }
    if command is not None:
        data["command"] = command
    return data
