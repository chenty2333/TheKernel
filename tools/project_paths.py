"""Repository path helpers shared by TheKernel development tools."""

from __future__ import annotations

from pathlib import Path


def repo_root() -> Path:
    """Return the checkout root without relying on the caller's cwd."""

    root = Path(__file__).resolve().parents[1]
    if not (root / "Cargo.toml").is_file() or not (root / "kernel").is_dir():
        raise RuntimeError(f"could not identify TheKernel repository root from {__file__}")
    return root
