#!/usr/bin/env python3
"""Validate and safely extract one exact Cargo release archive.

This helper is intentionally independent of a Cargo workspace.  The release
consumer gate uses it after packaging so a normalized manifest cannot smuggle
workspace, path, or Git dependencies into the temporary consumer graph.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import tarfile
import tomllib
from pathlib import Path, PurePosixPath
from typing import Any, NoReturn


DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")
FORBIDDEN_DEPENDENCY_KEYS = (
    "path",
    "git",
    "workspace",
    "registry",
    "registry-index",
)
DEFAULT_MAX_ARCHIVE_BYTES = 128 * 1024 * 1024
DEFAULT_MAX_UNPACKED_BYTES = 512 * 1024 * 1024
DEFAULT_MAX_MEMBERS = 100_000


def fail(message: str) -> NoReturn:
    print(f"release artifact audit failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def audit_dependencies(scope: str, table: Any) -> None:
    if table is None:
        return
    if not isinstance(table, dict):
        fail(f"{scope} is not a table")
    for name, specification in table.items():
        if isinstance(specification, str):
            continue
        if not isinstance(specification, dict):
            fail(f"{scope}.{name} has an invalid specification")
        leaked = [key for key in FORBIDDEN_DEPENDENCY_KEYS if key in specification]
        if leaked:
            fail(f"{scope}.{name} leaks {', '.join(leaked)}")
        if "version" not in specification:
            fail(f"{scope}.{name} has no registry version")


def audit_manifest(
    path: Path,
    *,
    package_name: str,
    version: str,
    repository: str | None,
) -> None:
    with path.open("rb") as manifest_file:
        manifest = tomllib.load(manifest_file)

    package = manifest.get("package")
    if not isinstance(package, dict):
        fail(f"{path}: missing [package]")
    if package.get("name") != package_name:
        fail(
            f"{path}: package name {package.get('name')!r} does not match "
            f"{package_name!r}"
        )
    if package.get("version") != version:
        fail(
            f"{path}: version {package.get('version')!r} does not match "
            f"{version!r}"
        )
    if repository is not None and package.get("repository") != repository:
        fail(
            f"{path}: repository {package.get('repository')!r} does not match "
            f"{repository!r}"
        )

    for forbidden in ("patch", "replace", "workspace"):
        if forbidden in manifest:
            fail(f"{path}: normalized archive contains [{forbidden}]")

    for table_name in DEPENDENCY_TABLES:
        audit_dependencies(table_name, manifest.get(table_name))
    targets = manifest.get("target", {})
    if not isinstance(targets, dict):
        fail(f"{path}: target is not a table")
    for target_name, target in targets.items():
        if not isinstance(target, dict):
            fail(f"{path}: target.{target_name} is not a table")
        for table_name in DEPENDENCY_TABLES:
            audit_dependencies(
                f"target.{target_name}.{table_name}", target.get(table_name)
            )


def audit_vcs_info(path: Path, expected_head: str) -> None:
    if not path.is_file():
        fail(f"{path}: Cargo archive has no .cargo_vcs_info.json")
    try:
        record = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        fail(f"{path}: invalid VCS record: {error}")
    git = record.get("git")
    if not isinstance(git, dict):
        fail(f"{path}: VCS record has no git object")
    if git.get("sha1") != expected_head:
        fail(
            f"{path}: archive HEAD {git.get('sha1')!r} does not match "
            f"{expected_head!r}"
        )
    if git.get("dirty", False) is not False:
        fail(f"{path}: archive was produced from a dirty worktree")


def validate_members(
    archive: tarfile.TarFile,
    *,
    expected_root: str,
    max_members: int,
    max_unpacked_bytes: int,
) -> list[tarfile.TarInfo]:
    members = archive.getmembers()
    if len(members) > max_members:
        fail(f"archive has {len(members)} members; limit is {max_members}")

    total_size = 0
    seen: set[str] = set()
    for member in members:
        name = member.name
        if name in seen:
            fail(f"archive contains duplicate member {name!r}")
        seen.add(name)
        if "\\" in name:
            fail(f"archive member uses a backslash: {name!r}")
        pure = PurePosixPath(name)
        if pure.is_absolute() or ".." in pure.parts:
            fail(f"archive member escapes extraction root: {name!r}")
        if not pure.parts or pure.parts[0] != expected_root:
            fail(f"archive member is outside {expected_root!r}: {name!r}")
        if not (member.isfile() or member.isdir()):
            fail(f"archive member is not a regular file or directory: {name!r}")
        if member.size < 0:
            fail(f"archive member has a negative size: {name!r}")
        total_size += member.size
        if total_size > max_unpacked_bytes:
            fail(
                f"archive expands to more than {max_unpacked_bytes} bytes "
                f"(at {name!r})"
            )
    return members


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--extract-root", required=True, type=Path)
    parser.add_argument("--package", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--repo-head", required=True)
    parser.add_argument("--repository")
    parser.add_argument("--expected-sha256")
    parser.add_argument(
        "--max-archive-bytes", type=int, default=DEFAULT_MAX_ARCHIVE_BYTES
    )
    parser.add_argument(
        "--max-unpacked-bytes", type=int, default=DEFAULT_MAX_UNPACKED_BYTES
    )
    parser.add_argument("--max-members", type=int, default=DEFAULT_MAX_MEMBERS)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    archive_path = args.archive.resolve()
    extract_root = args.extract_root.resolve()
    if not archive_path.is_file():
        fail(f"archive does not exist: {archive_path}")
    if args.max_archive_bytes <= 0 or args.max_unpacked_bytes <= 0:
        fail("archive size limits must be positive")
    if args.max_members <= 0:
        fail("member limit must be positive")
    if archive_path.stat().st_size > args.max_archive_bytes:
        fail(
            f"archive is {archive_path.stat().st_size} bytes; limit is "
            f"{args.max_archive_bytes}"
        )
    if not args.repo_head or len(args.repo_head) != 40 or any(
        character not in "0123456789abcdef" for character in args.repo_head
    ):
        fail(f"invalid repository HEAD: {args.repo_head!r}")

    archive_sha256 = sha256_file(archive_path)
    if args.expected_sha256 is not None and archive_sha256 != args.expected_sha256:
        fail(
            f"{args.package}: archive checksum {archive_sha256} does not match "
            f"expected {args.expected_sha256}"
        )

    expected_root = f"{args.package}-{args.version}"
    package_dir = extract_root / expected_root
    if package_dir.exists():
        fail(f"extraction destination already exists: {package_dir}")
    extract_root.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive_path, mode="r:gz") as archive:
        members = validate_members(
            archive,
            expected_root=expected_root,
            max_members=args.max_members,
            max_unpacked_bytes=args.max_unpacked_bytes,
        )
        archive.extractall(extract_root, members=members)

    if not package_dir.is_dir():
        fail(f"archive did not create package root: {package_dir}")
    audit_manifest(
        package_dir / "Cargo.toml",
        package_name=args.package,
        version=args.version,
        repository=args.repository,
    )
    audit_vcs_info(package_dir / ".cargo_vcs_info.json", args.repo_head)

    # Detect archive replacement between validation and extraction.
    final_sha256 = sha256_file(archive_path)
    if final_sha256 != archive_sha256:
        fail(f"archive changed while it was being audited: {archive_path}")

    print(f"{args.package}\t{args.version}\t{archive_sha256}\t{package_dir}")


if __name__ == "__main__":
    main()
