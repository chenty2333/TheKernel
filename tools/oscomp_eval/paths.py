"""Path discovery for the local OSComp evaluator."""

from __future__ import annotations

import shutil
from pathlib import Path


class PathError(RuntimeError):
    """Raised when repo-local evaluator paths cannot be resolved."""


def repo_root(start: Path | None = None) -> Path:
    current = (start or Path(__file__)).resolve()
    if current.is_file():
        current = current.parent

    for candidate in (current, *current.parents):
        if (candidate / ".git").exists() and (candidate / "scripts" / "oscomp.sh").is_file():
            return candidate

    raise PathError(f"could not find TheKernel repo root from {current}")


def state_root(root: Path | None = None) -> Path:
    return (root or repo_root()) / ".state" / "oscomp-eval"


def runs_root(root: Path | None = None) -> Path:
    return state_root(root) / "runs"


def official_root(root: Path | None = None) -> Path:
    return (root or repo_root()) / "tools" / "oscomp_eval" / "official"


def official_judge_dir(root: Path | None = None) -> Path:
    return official_root(root) / "judge"


RUN_ARTIFACT_NAMES = (
    "manifest.json",
    "score.json",
    "report.md",
    "report.html",
    "artifact-index.json",
    "inputs",
    "rv",
    "la",
)


def _remove_run_artifacts(run_dir: Path) -> None:
    for name in RUN_ARTIFACT_NAMES:
        path = run_dir / name
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        elif path.exists() or path.is_symlink():
            path.unlink()


def prepare_run_dir(run_dir: Path, *, replace: bool = False) -> Path:
    if run_dir.exists() and not run_dir.is_dir():
        raise FileExistsError(f"run path exists and is not a directory: {run_dir}")
    if run_dir.exists() and not replace:
        raise FileExistsError(f"run directory already exists: {run_dir}")
    if run_dir.exists() and replace:
        _remove_run_artifacts(run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    return run_dir


def create_run_dir(name: str, *, root: Path | None = None, replace: bool = False) -> Path:
    if not name:
        raise PathError("run name must not be empty")
    if "/" in name or "\\" in name:
        raise PathError(f"run name must not contain path separators: {name}")

    run_dir = runs_root(root) / name
    return prepare_run_dir(run_dir, replace=replace)
