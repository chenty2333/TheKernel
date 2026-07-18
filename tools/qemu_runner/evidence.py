"""Stable file identity and content evidence for QEMU runs."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path


class EvidenceError(ValueError):
    """Raised when a file changes while its evidence is captured."""


def file_evidence(path: Path) -> dict[str, str | int]:
    path = path.expanduser().resolve()
    digest = hashlib.sha256()
    with path.open("rb") as source:
        before = os.fstat(source.fileno())
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
        after = os.fstat(source.fileno())
        path_after = path.stat()

    stable_fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
    if any(getattr(before, field) != getattr(after, field) for field in stable_fields):
        raise EvidenceError(f"file changed while hashing: {path}")
    if any(getattr(after, field) != getattr(path_after, field) for field in stable_fields):
        raise EvidenceError(f"file path changed while hashing: {path}")
    return {
        "path": str(path),
        "size_bytes": after.st_size,
        "sha256": digest.hexdigest(),
    }
