#!/usr/bin/env python3
"""Mechanically retarget a temporary TheKernel manifest to release artifacts."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
import tomllib
from pathlib import Path
from typing import NoReturn


def fail(message: str) -> NoReturn:
    print(f"release consumer rewrite failed: {message}", file=sys.stderr)
    raise SystemExit(1)


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def parse_replacement(value: str) -> tuple[str, str]:
    if "=" not in value:
        raise argparse.ArgumentTypeError("replacement must be OLD=NEW")
    old, new = value.split("=", 1)
    if not old or not new:
        raise argparse.ArgumentTypeError("replacement paths must be non-empty")
    return old, new


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument(
        "--replace", action="append", default=[], type=parse_replacement
    )
    parser.add_argument("--forbid-text", action="append", default=[])
    parser.add_argument("--record", type=Path)
    args = parser.parse_args()

    if not args.replace:
        fail("at least one --replace is required")
    manifest_path = args.manifest.resolve()
    if not manifest_path.is_file():
        fail(f"manifest does not exist: {manifest_path}")

    original = manifest_path.read_bytes()
    try:
        text = original.decode("utf-8")
    except UnicodeDecodeError as error:
        fail(f"manifest is not UTF-8: {error}")

    seen_old: set[str] = set()
    seen_new: set[str] = set()
    records: list[tuple[str, str]] = []
    for old, new in args.replace:
        if old in seen_old:
            fail(f"duplicate old path: {old!r}")
        if new in seen_new:
            fail(f"duplicate new path: {new!r}")
        seen_old.add(old)
        seen_new.add(new)

        # JSON strings and TOML basic strings share the escaping needed for
        # these path values.  Replacing the complete literal avoids changing a
        # comment or a package name that merely contains the same substring.
        old_literal = json.dumps(old)
        new_literal = json.dumps(new)
        count = text.count(old_literal)
        if count != 1:
            fail(
                f"expected one exact {old_literal} path anchor, found {count}; "
                "the source manifest changed"
            )
        text = text.replace(old_literal, new_literal, 1)
        records.append((old, new))

    for forbidden in args.forbid_text:
        if forbidden and forbidden in text:
            fail(f"rewritten manifest still contains forbidden text {forbidden!r}")

    try:
        tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        fail(f"rewritten manifest is invalid TOML: {error}")

    rewritten = text.encode("utf-8")
    manifest_path.write_bytes(rewritten)
    if args.record is not None:
        args.record.parent.mkdir(parents=True, exist_ok=True)
        with args.record.open("w", encoding="utf-8") as record:
            record.write(f"before_sha256\t{digest(original)}\n")
            record.write(f"after_sha256\t{digest(rewritten)}\n")
            for old, new in records:
                record.write(f"replace\t{old}\t{new}\n")

    print(
        f"release consumer rewrite: PASS ({len(records)} exact path replacements)"
    )


if __name__ == "__main__":
    main()
