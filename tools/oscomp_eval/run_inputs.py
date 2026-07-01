"""Capture small score-run input files into the run directory."""

from __future__ import annotations

from pathlib import Path


class RunInputError(ValueError):
    """Raised when an input file cannot be captured into a run directory."""


def _relative(path: Path, base: Path) -> str:
    return str(path.relative_to(base))


def capture_input_file(
    *,
    run_dir: Path,
    source: Path,
    name: str,
) -> str:
    if not name or "/" in name or "\\" in name:
        raise RunInputError(f"input capture name must be a plain filename: {name}")
    if not source.is_file():
        raise RunInputError(f"input file does not exist: {source}")

    inputs_dir = run_dir / "inputs"
    inputs_dir.mkdir(parents=True, exist_ok=True)
    target = inputs_dir / name
    if source.resolve() != target.resolve():
        tmp = target.with_name(f".{target.name}.tmp")
        tmp.write_bytes(source.read_bytes())
        tmp.replace(target)
    return _relative(target, run_dir)
