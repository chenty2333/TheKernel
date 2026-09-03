"""Shared helpers for the repository's Python test suite.

Run everything from the repository root with one command:

    python3 -m unittest discover -s tests -t .
"""

from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
from pathlib import Path
from types import ModuleType


def repo_root() -> Path:
    """Return the repository root that contains this tests package."""
    return Path(__file__).resolve().parents[1]


def load_script_module(name: str, relative_path: str) -> ModuleType:
    """Load a standalone repository script as a module by path.

    Repository scripts are not importable packages; register the loaded
    module in sys.modules so dataclasses and similar helpers resolve it.
    """
    spec = importlib.util.spec_from_file_location(name, repo_root() / relative_path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_tmpdir() -> tempfile.TemporaryDirectory[str]:
    """Return a temporary directory on the host cache, never on tmpfs.

    Honors THEKERNEL_TEST_TMPDIR and defaults to ~/.cache/thekernel-test-tmp.
    """
    root = Path(
        os.environ.get("THEKERNEL_TEST_TMPDIR", Path.home() / ".cache" / "thekernel-test-tmp")
    )
    root.mkdir(parents=True, exist_ok=True)
    return tempfile.TemporaryDirectory(dir=root)
