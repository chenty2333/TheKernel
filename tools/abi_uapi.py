#!/usr/bin/env python3
"""Validate the pinned Linux UAPI header provenance and materialized tree."""
from __future__ import annotations

import hashlib
import json
import os
import re
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "docs/linux-abi/uapi-headers.json"
VERSION = "v6.12.103"
REF = "v6.12.103"
COMMIT = "25c09b42358e73e1476e517b296edb6344f2e4bd"
ARCH = "x86_64"
MATERIALIZED_PATH = ".state/linux-6.12.103/uapi/include"
TREE_SHA256 = "53f5a259b0daa68e5dde797ccc94d5c5dc8aba1c3d8cd76e45f68a572e0a98a9"
HEX_64 = re.compile(r"^[0-9a-f]{64}$")


class UapiError(RuntimeError):
    pass


def repository_path(root: Path, value: object, field: str) -> Path:
    if not isinstance(value, str) or not value:
        raise UapiError(f"{field} must be a non-empty relative path")
    raw = Path(value)
    if raw.is_absolute() or ".." in raw.parts:
        raise UapiError(f"{field} escapes the repository")
    root = root.resolve()
    candidate = (root / raw).resolve(strict=False)
    try:
        candidate.relative_to(root)
    except ValueError as exc:
        raise UapiError(f"{field} escapes the repository through a symlink") from exc
    return candidate


def require_sha(value: object, field: str) -> str:
    if not isinstance(value, str) or not HEX_64.fullmatch(value):
        raise UapiError(f"{field} must be a lowercase SHA-256")
    return value


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def tree_sha256(root: Path) -> str:
    """Hash regular files by sorted POSIX path and their raw content.

    Each path and payload is prefixed by its unsigned 64-bit big-endian byte
    length, making the stream unambiguous without depending on filesystem
    metadata or on characters that happen not to occur in header paths.
    """
    if not root.is_dir():
        raise UapiError(f"UAPI header tree is missing: {root}")
    files: list[Path] = []
    for path in root.rglob("*"):
        if path.is_symlink() or not (path.is_file() or path.is_dir()):
            raise UapiError(f"UAPI header tree contains unsupported entry: {path}")
        if path.is_file():
            files.append(path)
    if not files:
        raise UapiError("UAPI header tree is empty")
    digest = hashlib.sha256()
    for path in sorted(files, key=lambda item: item.relative_to(root).as_posix()):
        relative = path.relative_to(root).as_posix().encode("utf-8")
        payload = path.read_bytes()
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)
    return digest.hexdigest()


def load_manifest(path: Path, root: Path) -> dict[str, Any]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise UapiError(f"cannot read UAPI manifest: {exc}") from exc
    if not isinstance(manifest, dict) or manifest.get("schema") != "thekernel-linux-uapi-headers-v1":
        raise UapiError("unsupported UAPI manifest schema")
    linux = manifest.get("linux")
    if not isinstance(linux, dict):
        raise UapiError("linux must be an object")
    if linux.get("version") != VERSION or linux.get("ref") != REF or linux.get("commit") != COMMIT:
        raise UapiError("UAPI manifest must bind Linux v6.12.103 ref and commit")
    if linux.get("architecture") != ARCH:
        raise UapiError("only Linux v6.12.103 x86_64 UAPI headers are supported")
    source = linux.get("source")
    if not isinstance(source, dict):
        raise UapiError("linux.source must be an object")
    repository_path(root, source.get("tarball_path"), "linux.source.tarball_path")
    require_sha(source.get("tarball_sha256"), "linux.source.tarball_sha256")
    headers = manifest.get("headers")
    if not isinstance(headers, dict):
        raise UapiError("headers must be an object")
    if headers.get("materialized_path") != MATERIALIZED_PATH:
        raise UapiError(f"headers.materialized_path must be {MATERIALIZED_PATH}")
    if headers.get("tree_sha256") != TREE_SHA256:
        raise UapiError("headers.tree_sha256 must match the pinned Linux UAPI tree")
    return manifest


def verify(manifest_path: Path = DEFAULT_MANIFEST, root: Path = ROOT, *, require_materialized: bool = False) -> dict[str, str]:
    root = root.resolve()
    manifest = load_manifest(manifest_path, root)
    source = manifest["linux"]["source"]
    tarball = repository_path(root, source["tarball_path"], "linux.source.tarball_path")
    report = {"manifest": "ok"}
    if tarball.is_file():
        actual = sha256(tarball)
        if actual != source["tarball_sha256"]:
            raise UapiError(f"Linux tarball SHA-256 mismatch: expected {source['tarball_sha256']}, got {actual}")
        report["source"] = "ok"
    else:
        report["source"] = "unmaterialized"
    headers_path = repository_path(root, manifest["headers"]["materialized_path"], "headers.materialized_path")
    if headers_path.exists():
        actual = tree_sha256(headers_path)
        if actual != TREE_SHA256:
            raise UapiError(f"UAPI header tree SHA-256 mismatch: expected {TREE_SHA256}, got {actual}")
        report["headers"] = "ok"
    else:
        report["headers"] = "unmaterialized"
    if require_materialized and any(status != "ok" for status in report.values()):
        raise UapiError("required UAPI material is not fully materialized")
    return report


def main(argv: list[str] | None = None) -> int:
    import argparse
    import sys

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--require-materialized", action="store_true")
    args = parser.parse_args(argv)
    root = args.root.resolve()
    manifest = (args.manifest if args.manifest is not None else root / "docs/linux-abi/uapi-headers.json").resolve()
    try:
        for item, status in verify(manifest, root, require_materialized=args.require_materialized).items():
            print(f"linux-uapi: {item}={status}")
    except UapiError as exc:
        print(f"linux-uapi: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
