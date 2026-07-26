#!/usr/bin/env python3
"""Partition cargo/clippy JSON diagnostics into owned and vendored code.

The clippy gate lints TheKernel-owned packages. Vendored sources under
`third_party/rust-patches/` and the maintained sibling workspaces keep their
upstream lint posture, but cargo still emits rustc lints for them because they
are path dependencies rather than registry dependencies.

Dropping those messages silently would make the gate look cleaner than the tree
is, so this filter reports both populations and fails only on the owned one.
A compile error fails the gate wherever it occurs.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path, PurePosixPath
from urllib.parse import unquote, urlsplit

# Path prefixes whose diagnostics are reported but never gate-failing.
VENDORED_PREFIXES = ("third_party/",)
REPO_ROOT = Path(__file__).resolve().parents[2]


def _primary_file(message: dict) -> str | None:
    for span in message.get("spans", ()):
        if span.get("is_primary"):
            return span.get("file_name")
    # A message without a primary span (crate-level summaries, notes emitted
    # against the crate root) inherits the first span it does have.
    for span in message.get("spans", ()):
        return span.get("file_name")
    return None


def _package_is_vendored(record: dict) -> bool | None:
    """Classify a Cargo package ID, or defer when no usable ID is present."""
    package_id = record.get("package_id")
    if not isinstance(package_id, str) or not package_id:
        return None
    if package_id.startswith(("registry+", "git+")):
        return True
    if not package_id.startswith("path+file://"):
        # Unknown Cargo source kinds must not silently weaken the owned gate.
        return False

    package_url = package_id[len("path+") :]
    package_path = Path(unquote(urlsplit(package_url).path)).resolve()
    try:
        relative = package_path.relative_to(REPO_ROOT)
    except ValueError:
        # Maintained sibling workspaces live beside this repository and have
        # independent lint gates.
        return True
    return bool(relative.parts and relative.parts[0] == "third_party")


def _is_vendored(record: dict, file_name: str | None) -> bool:
    package_classification = _package_is_vendored(record)
    if package_classification is not None:
        return package_classification
    if file_name is None:
        # Crate-level summaries carry no location. They restate counts that the
        # per-diagnostic messages already provide, so they never gate.
        return True
    path = PurePosixPath(file_name)
    if path.is_absolute():
        # Registry checkouts and toolchain sources live outside the worktree.
        return True
    return file_name.startswith(VENDORED_PREFIXES)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--profile",
        required=True,
        help="clippy profile label used in the summary line",
    )
    args = parser.parse_args()

    owned: list[dict] = []
    vendored_counts: Counter[str] = Counter()
    owned_counts: Counter[str] = Counter()
    hard_errors = 0

    for line in sys.stdin:
        line = line.strip()
        if not line or not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if record.get("reason") != "compiler-message":
            continue
        message = record.get("message") or {}
        level = message.get("level")
        if level not in ("warning", "error"):
            continue
        code = ((message.get("code") or {}).get("code")) or level
        file_name = _primary_file(message)

        if level == "error":
            # A compile error is never someone else's problem: it means this
            # gate cannot make any claim about the tree.
            hard_errors += 1
            owned.append(message)
            owned_counts[code] += 1
            continue

        if _is_vendored(record, file_name):
            vendored_counts[code] += 1
        else:
            owned.append(message)
            owned_counts[code] += 1

    for message in owned:
        rendered = message.get("rendered")
        if rendered:
            sys.stdout.write(rendered)

    owned_total = sum(owned_counts.values())
    vendored_total = sum(vendored_counts.values())

    print()
    print(f"clippy[{args.profile}]: owned diagnostics: {owned_total}")
    for code, count in owned_counts.most_common():
        print(f"clippy[{args.profile}]:   {count:5d}  {code}")
    print(
        f"clippy[{args.profile}]: vendored diagnostics (reported, not gated): "
        f"{vendored_total}"
    )
    for code, count in vendored_counts.most_common():
        print(f"clippy[{args.profile}]:   {count:5d}  {code}")

    if hard_errors:
        print(
            f"clippy[{args.profile}]: FAILED with {hard_errors} compile error(s)",
            file=sys.stderr,
        )
        return 1
    if owned_total:
        print(
            f"clippy[{args.profile}]: FAILED with {owned_total} owned diagnostic(s)",
            file=sys.stderr,
        )
        return 1
    print(f"clippy[{args.profile}]: clean")
    return 0


if __name__ == "__main__":
    sys.exit(main())
