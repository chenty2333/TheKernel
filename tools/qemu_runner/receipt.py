"""Versioned QEMU receipt helpers."""

from __future__ import annotations

import hashlib
import json
import os
import subprocess
import tempfile
from pathlib import Path
from typing import Any

from .model import InputForwarding
from scripts.ci import source_combination


RECEIPT_SCHEMA_VERSION = 4


class ReceiptError(ValueError):
    """Raised when a QEMU receipt transition or input record is invalid."""


def _git_identity(source_root: Path) -> dict[str, str | bool]:
    """Capture one checkout's actual identity without mutating it."""

    def git(*arguments: str) -> str:
        completed = subprocess.run(
            ("git", "-C", str(source_root), *arguments),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if completed.returncode:
            detail = completed.stderr.strip() or "unknown git failure"
            raise ReceiptError(f"cannot capture source identity: {detail}")
        return completed.stdout.strip()

    repository_root = git("rev-parse", "--show-toplevel")
    commit = git("rev-parse", "HEAD^{commit}")
    tree = git("rev-parse", "HEAD^{tree}")
    status = git("status", "--porcelain=v1", "--untracked-files=all")
    if not repository_root or not commit or not tree:
        raise ReceiptError("cannot capture source identity: empty git identity")
    return {
        "repository_root": str(Path(repository_root).resolve()),
        "commit": commit,
        "tree": tree,
        "worktree_dirty": bool(status),
    }


def source_identity() -> dict[str, Any]:
    """Record actual checkouts against the repository's declared combination."""

    source_root = Path(__file__).resolve().parents[2]
    configuration = source_root / "config" / "source-combination.toml"
    try:
        declared_sources = source_combination.load(configuration)
    except source_combination.SourceCombinationError as error:
        raise ReceiptError(f"cannot capture source identity: {error}") from error

    actual_sources: dict[str, dict[str, str | bool]] = {}
    thekernel = _git_identity(source_root)
    actual_sources["thekernel"] = {
        **thekernel,
        "match_declared": True,
    }
    for name, declared in sorted(declared_sources.items()):
        actual = _git_identity(source_root.parent / declared.path)
        actual_sources[name] = {
            **actual,
            "match_declared": actual["commit"] == declared.ref,
        }

    return {
        "schema": 1,
        "combination_id": source_combination.combination_id(
            declared_sources, str(thekernel["commit"])
        ),
        "sources": actual_sources,
    }


def atomic_write_receipt(path: Path, payload: dict[str, Any]) -> None:
    path = path.expanduser().resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
    )
    temporary = Path(temporary_name)
    try:
        output = os.fdopen(descriptor, "w", encoding="utf-8")
        descriptor = -1
        with output:
            json.dump(payload, output, indent=2, sort_keys=True)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        temporary.unlink(missing_ok=True)


def command_stream_evidence(path: Path) -> dict[str, str | int]:
    """Hash the command artifact and count logical input lines."""

    path = path.expanduser().resolve()
    digest = hashlib.sha256()
    byte_count = 0
    newline_count = 0
    last_byte: int | None = None
    try:
        with path.open("rb") as source:
            while chunk := source.read(1024 * 1024):
                digest.update(chunk)
                byte_count += len(chunk)
                newline_count += chunk.count(b"\n")
                last_byte = chunk[-1]
    except OSError as error:
        raise ReceiptError(f"cannot read command stream {path}: {error}") from error
    line_count = newline_count
    if byte_count > 0 and last_byte != ord("\n"):
        line_count += 1
    return {
        "path": str(path),
        "sha256": digest.hexdigest(),
        "bytes": byte_count,
        "line_count": line_count,
    }


def input_forwarding_payload(
    forwarding: InputForwarding,
    *,
    source: dict[str, str | int],
    source_unchanged: bool,
) -> dict[str, Any]:
    """Bind a command artifact to the bytes accepted by QEMU stdin."""

    return {
        "source": source,
        "forwarded": {
            "sha256": forwarding.sha256,
            "bytes": forwarding.bytes_forwarded,
            "line_count": forwarding.line_count,
        },
        "source_unchanged": source_unchanged,
        "source_eof": forwarding.source_eof,
        "broken_pipe": forwarding.broken_pipe,
        "relay_complete": forwarding.relay_complete,
    }
