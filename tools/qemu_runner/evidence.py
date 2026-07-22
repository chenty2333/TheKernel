"""Stable file identity and content evidence for QEMU runs."""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
from typing import Any


class EvidenceError(ValueError):
    """Raised when file evidence cannot be captured or validated."""


def validate_file_evidence(value: Any, label: str) -> dict[str, Any]:
    """Validate the portable shape of a captured file-evidence record."""

    if not isinstance(value, dict):
        raise EvidenceError(f"missing {label} evidence")
    path = value.get("path")
    size = value.get("size_bytes")
    digest = value.get("sha256")
    if not isinstance(path, str) or not path:
        raise EvidenceError(f"invalid {label} path")
    try:
        canonical_path = str(Path(path).expanduser().resolve())
    except (OSError, RuntimeError) as error:
        raise EvidenceError(f"invalid {label} path: {error}") from error
    if path != canonical_path:
        raise EvidenceError(f"non-canonical {label} path")
    if type(size) is not int or size < 0:
        raise EvidenceError(f"invalid {label} size")
    if not (
        isinstance(digest, str)
        and len(digest) == 64
        and all(character in "0123456789abcdef" for character in digest)
    ):
        raise EvidenceError(f"invalid {label} SHA-256")
    return value


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
