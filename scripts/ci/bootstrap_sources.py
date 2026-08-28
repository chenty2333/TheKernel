#!/usr/bin/env python3
"""Create and verify the exact sibling checkouts in source-combination.toml."""

from __future__ import annotations

import argparse
import ctypes
import errno
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Sequence

import source_combination


class BootstrapError(RuntimeError):
    """A sibling checkout could not be safely created or verified."""


def git(directory: Path, *arguments: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(directory), *arguments],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise BootstrapError(f"git -C {directory} {' '.join(arguments)} failed: {detail}")
    return result.stdout.strip()


def remote_url(remote_base: str, repository: str) -> str:
    return f"{remote_base.rstrip('/')}/{repository}.git"


def allowed_origins(remote_base: str, repository: str) -> frozenset[str]:
    https_origin = remote_url(remote_base, repository)
    if https_origin == remote_url("https://github.com", repository):
        return frozenset((https_origin, f"git@github.com:{repository}.git"))
    return frozenset((https_origin,))


def checkout_path(parent: Path, source: source_combination.Source) -> Path:
    destination = parent / source.path
    try:
        destination.relative_to(parent)
    except ValueError as exc:
        raise BootstrapError(f"checkout path escapes parent: {source.path}") from exc
    return destination


def install_no_replace(temporary: Path, destination: Path) -> None:
    """Atomically install a completed checkout without replacing any entry."""
    if os.path.lexists(destination):
        raise BootstrapError(
            f"refusing to replace checkout created concurrently: {destination}"
        )
    try:
        renameat2 = ctypes.CDLL(None, use_errno=True).renameat2
    except AttributeError as exc:
        raise BootstrapError("atomic no-replace directory install is unavailable") from exc
    renameat2.argtypes = (
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_uint,
    )
    renameat2.restype = ctypes.c_int
    if renameat2(
        -100,
        os.fsencode(temporary),
        -100,
        os.fsencode(destination),
        1,
    ) == 0:
        return
    error = ctypes.get_errno()
    if error in (errno.EEXIST, errno.ENOTEMPTY):
        raise BootstrapError(
            f"refusing to replace checkout created concurrently: {destination}"
        )
    raise BootstrapError(
        f"atomic no-replace directory install failed for {destination}: "
        f"{os.strerror(error)}"
    )


def verify_existing(
    destination: Path, source: source_combination.Source, remote_base: str
) -> None:
    if not destination.is_dir():
        raise BootstrapError(f"refusing to replace existing non-directory: {destination}")
    if git(destination, "status", "--porcelain", "--untracked-files=all"):
        raise BootstrapError(f"refusing to change dirty checkout: {destination}")
    origin = git(destination, "remote", "get-url", "origin")
    origins = allowed_origins(remote_base, source.repository)
    if origin not in origins:
        raise BootstrapError(
            f"existing checkout has origin {origin}, expected one of "
            f"{', '.join(sorted(origins))}: "
            f"{destination}"
        )
    head = git(destination, "rev-parse", "HEAD^{commit}")
    if head != source.ref:
        raise BootstrapError(
            f"existing checkout is {head}, expected {source.ref}: {destination}; "
            "remove or update it yourself, then rerun"
        )


def create_checkout(
    parent: Path, destination: Path, source: source_combination.Source, remote_base: str
) -> None:
    temporary = Path(
        tempfile.mkdtemp(prefix=f".{source.path}.bootstrap-", dir=parent)
    )
    try:
        git(temporary, "init", "--quiet")
        git(temporary, "remote", "add", "origin", remote_url(remote_base, source.repository))
        git(temporary, "fetch", "--no-tags", "origin", source.ref)
        git(temporary, "cat-file", "-e", f"{source.ref}^{{commit}}")
        git(temporary, "checkout", "--quiet", "--detach", source.ref)
        if git(temporary, "rev-parse", "HEAD^{commit}") != source.ref:
            raise BootstrapError(f"fetched commit differs from requested ref: {source.ref}")
        install_no_replace(temporary, destination)
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def bootstrap(
    parent: Path,
    sources: dict[str, source_combination.Source],
    remote_base: str = "https://github.com",
) -> None:
    if not parent.is_dir():
        raise BootstrapError(f"parent directory does not exist: {parent}")
    parent = parent.resolve()
    for name in sorted(sources):
        source = sources[name]
        destination = checkout_path(parent, source)
        if destination.is_symlink():
            raise BootstrapError(f"refusing to use sibling symlink: {destination}")
        if os.path.lexists(destination):
            verify_existing(destination, source, remote_base)
            print(f"verified {name}: {destination} @ {source.ref}")
        else:
            create_checkout(parent, destination, source, remote_base)
            print(f"created {name}: {destination} @ {source.ref}")


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=source_combination.DEFAULT_CONFIG)
    parser.add_argument(
        "--parent",
        type=Path,
        default=Path.cwd().parent,
        help="directory that contains TheKernel and its sibling checkouts (default: ..)",
    )
    parser.add_argument(
        "--remote-base",
        default="https://github.com",
        help="Git remote prefix used to clone owner/repository.git (default: https://github.com)",
    )
    args = parser.parse_args(argv)
    try:
        sources = source_combination.load(args.config)
        bootstrap(args.parent, sources, args.remote_base)
    except (BootstrapError, source_combination.SourceCombinationError) as exc:
        print(f"bootstrap-sources: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
