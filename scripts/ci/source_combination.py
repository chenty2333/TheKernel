#!/usr/bin/env python3
"""Load the exact sibling sources used by a TheKernel CI integration run."""

from __future__ import annotations

import argparse
import hashlib
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

DEFAULT_CONFIG = Path("config/source-combination.toml")
HEX_40 = re.compile(r"^[0-9a-f]{40}$")
REPOSITORY = re.compile(
    r"^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$"
)
CHECKOUT_PATH = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]*$")
REQUIRED_SOURCES = frozenset(("ax", "linux_abi"))


class SourceCombinationError(ValueError):
    """The source-combination record cannot safely drive a checkout."""


@dataclass(frozen=True)
class Source:
    repository: str
    ref: str
    path: str


def load(path: Path) -> dict[str, Source]:
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        raise SourceCombinationError(f"cannot read {path}: {exc}") from exc

    if set(data) != {"schema", "source"} or data["schema"] != 1:
        raise SourceCombinationError("record must contain only schema = 1 and source")
    raw_sources = data["source"]
    if not isinstance(raw_sources, dict):
        raise SourceCombinationError("source must be a table")
    source_names = set(raw_sources)
    if source_names != REQUIRED_SOURCES:
        raise SourceCombinationError(
            "record must define exactly: " + ", ".join(sorted(REQUIRED_SOURCES))
        )

    sources: dict[str, Source] = {}
    paths: set[str] = set()
    for name, raw in raw_sources.items():
        if not isinstance(raw, dict) or set(raw) != {"repository", "ref", "path"}:
            raise SourceCombinationError(
                f"source.{name} must contain only repository, ref, and path"
            )
        repository, ref, checkout_path = (
            raw["repository"],
            raw["ref"],
            raw["path"],
        )
        if not isinstance(repository, str) or not REPOSITORY.fullmatch(repository):
            raise SourceCombinationError(
                f"source.{name}.repository is not an owner/repository name"
            )
        if not isinstance(ref, str) or not HEX_40.fullmatch(ref):
            raise SourceCombinationError(
                f"source.{name}.ref is not a lowercase 40-hex commit"
            )
        if not isinstance(checkout_path, str) or not CHECKOUT_PATH.fullmatch(checkout_path):
            raise SourceCombinationError(
                f"source.{name}.path is not a checkout directory name"
            )
        if checkout_path in paths:
            raise SourceCombinationError(f"source.{name}.path duplicates {checkout_path}")
        paths.add(checkout_path)
        sources[name] = Source(repository, ref, checkout_path)
    return sources


def combination_id(sources: Mapping[str, Source], thekernel_commit: str) -> str:
    if not HEX_40.fullmatch(thekernel_commit):
        raise SourceCombinationError(
            "TheKernel commit is not a lowercase 40-hex commit"
        )
    lines = [f"schema=1", f"thekernel={thekernel_commit}"]
    lines.extend(
        f"{name}={source.repository}@{source.ref}:{source.path}"
        for name, source in sorted(sources.items())
    )
    digest = hashlib.sha256(("\n".join(lines) + "\n").encode()).hexdigest()
    return f"source-combination-v1-{digest}"


def current_commit() -> str:
    try:
        return subprocess.check_output(
            ["git", "rev-parse", "HEAD^{commit}"], text=True
        ).strip()
    except (OSError, subprocess.CalledProcessError) as exc:
        raise SourceCombinationError(
            "cannot resolve the current TheKernel commit"
        ) from exc


def outputs(sources: Mapping[str, Source], thekernel_commit: str) -> dict[str, str]:
    result = {
        "thekernel_commit": thekernel_commit,
        "combination_id": combination_id(sources, thekernel_commit),
    }
    for name, source in sorted(sources.items()):
        result[f"{name}_repository"] = source.repository
        result[f"{name}_ref"] = source.ref
        result[f"{name}_path"] = source.path
    return result


def write_github_outputs(path: Path, values: Mapping[str, str]) -> None:
    with path.open("a", encoding="utf-8") as output:
        for key, value in values.items():
            output.write(f"{key}={value}\n")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("--thekernel-commit")
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args(argv)

    try:
        sources = load(args.config)
        commit = args.thekernel_commit or current_commit()
        values = outputs(sources, commit)
    except SourceCombinationError as exc:
        print(f"source-combination: {exc}", file=sys.stderr)
        return 1

    if args.github_output:
        write_github_outputs(args.github_output, values)
    print(values["combination_id"])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
