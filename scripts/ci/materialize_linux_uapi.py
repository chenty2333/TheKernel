#!/usr/bin/env python3
"""Materialize the pinned Linux v6.12.103 x86_64 UAPI headers."""
from __future__ import annotations

import argparse
import os
import posixpath
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
import abi_uapi  # noqa: E402


SOURCE_TOPLEVEL = "linux-6.12.103"


class MaterializeError(RuntimeError):
    pass


def safe_extract(tarball: Path, destination: Path) -> Path:
    """Extract only the expected Linux source tree, as oracle materialization does."""
    with tarfile.open(tarball, "r:xz") as archive:
        members = archive.getmembers()
        if not members:
            raise MaterializeError("Linux tarball is empty")
        for member in members:
            name = Path(member.name)
            if name.is_absolute() or ".." in name.parts or not name.parts or name.parts[0] != SOURCE_TOPLEVEL:
                raise MaterializeError(f"unsafe tarball member: {member.name}")
            if member.isdev() or member.isfifo():
                raise MaterializeError(f"unsafe tarball member type: {member.name}")
            if member.issym() or member.islnk():
                if posixpath.isabs(member.linkname):
                    raise MaterializeError(f"unsafe tarball link: {member.name}")
                base = posixpath.dirname(member.name) if member.issym() else ""
                target = posixpath.normpath(posixpath.join(base, member.linkname))
                if target != SOURCE_TOPLEVEL and not target.startswith(SOURCE_TOPLEVEL + "/"):
                    raise MaterializeError(f"unsafe tarball link: {member.name}")
            elif not (member.isfile() or member.isdir()):
                raise MaterializeError(f"unsupported tarball member type: {member.name}")
        archive.extractall(destination, members=members)
    source = destination / SOURCE_TOPLEVEL
    if not (source / "Makefile").is_file():
        raise MaterializeError("Linux tarball lacks its top-level Makefile")
    return source


def publish(staged: Path, destination: Path) -> None:
    backup = destination.with_name(f".{destination.name}.old-{os.getpid()}")
    if backup.exists():
        raise MaterializeError(f"refusing to replace unexpected backup path {backup}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    try:
        if destination.exists():
            os.replace(destination, backup)
        os.replace(staged, destination)
    except OSError as exc:
        if backup.exists() and not destination.exists():
            os.replace(backup, destination)
        raise MaterializeError(f"could not publish UAPI headers: {exc}") from exc
    if backup.exists():
        shutil.rmtree(backup)


def materialize(root: Path, manifest_path: Path) -> str:
    manifest = abi_uapi.load_manifest(manifest_path, root)
    source_info = manifest["linux"]["source"]
    tarball = abi_uapi.repository_path(root, source_info["tarball_path"], "linux.source.tarball_path")
    if not tarball.is_file():
        raise MaterializeError(f"Linux tarball is missing: {tarball}")
    if abi_uapi.sha256(tarball) != source_info["tarball_sha256"]:
        raise MaterializeError("Linux tarball SHA-256 mismatch")
    destination = abi_uapi.repository_path(root, manifest["headers"]["materialized_path"], "headers.materialized_path")
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="linux-uapi-", dir=destination.parent) as temporary:
        temporary_path = Path(temporary)
        source = safe_extract(tarball, temporary_path)
        install_root = temporary_path / "uapi"
        install_root.mkdir()
        subprocess.run([
            "make", "-C", str(source), "ARCH=x86_64",
            f"INSTALL_HDR_PATH={install_root}", "headers_install",
        ], check=True)
        staged = install_root / "include"
        actual = abi_uapi.tree_sha256(staged)
        if actual != abi_uapi.TREE_SHA256:
            raise MaterializeError(f"UAPI header tree SHA-256 mismatch: expected {abi_uapi.TREE_SHA256}, got {actual}")
        publish(staged, destination)
    return actual


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    parser.add_argument("--manifest", type=Path)
    args = parser.parse_args(argv)
    root = args.root.resolve()
    manifest = (args.manifest if args.manifest is not None else root / "docs/linux-abi/uapi-headers.json").resolve()
    try:
        digest = materialize(root, manifest)
        print(f"linux-uapi: headers.tree_sha256={digest}")
    except (abi_uapi.UapiError, MaterializeError, OSError, subprocess.CalledProcessError) as exc:
        print(f"linux-uapi-materialize: {exc}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
