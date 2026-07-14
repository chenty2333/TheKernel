#!/usr/bin/env python3
"""Verify unpublished sibling checksums in a packaged Cargo.lock."""

from __future__ import annotations

import argparse
import hashlib
import sys
import tomllib
from pathlib import Path
from typing import NoReturn


CRATES_IO_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"


def fail(message: str) -> NoReturn:
    print(f"release lock audit failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def parse_artifact(value: str) -> tuple[str, Path]:
    if "=" not in value:
        raise argparse.ArgumentTypeError("artifact must be PACKAGE=ARCHIVE")
    package, archive = value.split("=", 1)
    if not package or not archive:
        raise argparse.ArgumentTypeError("artifact values must be non-empty")
    return package, Path(archive).resolve()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--lock", required=True, type=Path)
    parser.add_argument("--version", default="0.1.0")
    parser.add_argument("--artifact", action="append", default=[], type=parse_artifact)
    args = parser.parse_args()
    if not args.artifact:
        fail("at least one sibling artifact is required")
    if not args.lock.is_file():
        fail(f"packaged lockfile does not exist: {args.lock}")
    try:
        with args.lock.open("rb") as lock_file:
            lock = tomllib.load(lock_file)
    except (OSError, tomllib.TOMLDecodeError) as error:
        fail(f"cannot load packaged lockfile: {error}")

    packages = lock.get("package")
    if not isinstance(packages, list):
        fail("packaged lockfile has no package list")
    seen: set[str] = set()
    for name, archive in args.artifact:
        if name in seen:
            fail(f"duplicate sibling artifact {name!r}")
        seen.add(name)
        if not archive.is_file():
            fail(f"sibling archive does not exist for {name}")
        matches = [
            package
            for package in packages
            if isinstance(package, dict)
            and package.get("name") == name
            and package.get("version") == args.version
        ]
        if len(matches) != 1:
            fail(
                f"expected one locked {name} {args.version}, found {len(matches)}"
            )
        package = matches[0]
        if package.get("source") != CRATES_IO_SOURCE:
            fail(f"{name} does not have the canonical crates.io lock source")
        archive_checksum = sha256_file(archive)
        if package.get("checksum") != archive_checksum:
            fail(f"{name} lock checksum does not match the exact sibling archive")

    print(f"release lock audit: PASS ({len(seen)} exact sibling archives)")


if __name__ == "__main__":
    main()
