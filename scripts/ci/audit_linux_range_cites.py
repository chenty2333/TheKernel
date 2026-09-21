#!/usr/bin/env python3
"""Report citations whose quoted text does not sit in the range the cite states.

`docs/upstream-provenance.md` claims that every Linux range in a `crates/**`
comment points at the code it quotes.  This checks that claim mechanically, so a
range is never justified by prose about how it was "derived".

Run it as::

    python3 scripts/ci/audit_linux_range_cites.py \\
        --linux /home/ava/Desktop/linux-7.2.3 --scope crates/linux

It exits 1 when a cite misses, and is **not** a CI stage: it needs a materialized
Linux release, and a miss is not automatically a defect (see the rule below).  A
human reads each printed row and either corrects the range or leaves it, and the
documented count of rows is the output of this command.

The rule, and what it cannot tell:

* A cite is checkable only when the comment text around it actually quotes the
  file the cite names.  The hunk is the run of comment lines touching the cite
  (up to two unmatched lines of separator or prose allowed inside it); the walk
  stops at a code line, at a line carrying its own citation, and at the
  `--window` edge, so one cite is never judged against a neighbouring cite's
  text.  A ` ``` ` delimiter is quote furniture rather than a boundary, so it
  neither stops the walk nor counts as a quoted line -- without that, the common
  layout here (a prose cite on the line above the fence it introduces) would
  leave most of the workspace's ranges unchecked.  Each kept line is looked up in
  the cited Linux file, and a cite is printed when some kept line has **no**
  occurrence inside `[lo, hi]`.
* Positions are resolved per quoted line, not pooled, because short C statements
  recur verbatim: `if (!arg || nr_args != 1)` appears about fifteen times in
  `io_uring/register.c` alone.  A line the range can explain by any of its
  duplicate positions passes.  The cost is the other direction -- a range that
  covers one of the duplicates but not the one the author meant is not printed.
* A printed row is a candidate, not a verdict.  Innocent causes exist: the cite
  may name an enclosing statement while quoting one branch of it, or the same
  text may live in several files and only the named one is consulted.  The guilty
  cause is the point: the range is simply wrong.
* The tool cannot see intent.  It never asks whether the author meant a different
  Linux file, so a cite pointing at the right text in the wrong file is invisible
  here -- that is what `scan_linux_excerpts.py` is for, since it matches text
  without consulting the citation at all.  And a comment that paraphrases rather
  than quotes has nothing to check.
"""
from __future__ import annotations

import argparse
import re
import sys
from collections import defaultdict
from pathlib import Path

# A citation is `<file>:<lo>[-<hi>]` inside one pair of backticks, and a second
# range for the same file may be written as bare `` `:<lo>-<hi>` `` -- both forms
# appear in `crates/linux`, so both are read, and a bare one inherits the file
# named earlier on the same line.
CITE = re.compile(r"`([A-Za-z0-9_./-]+\.[chH])?:(\d+)(?:-(\d+))?`")
GUTTER = "├└┌┐┘┤│─ \t"
MARKERS = ("///", "//!", "//", "*", "/*")
# A doc-comment code-fence delimiter, once its `///` and gutter are stripped.
FENCE = re.compile(r"^```")


def norm(text: str) -> str:
    return "".join(text.split())


def strip_comment(text: str) -> str:
    body = text.strip().lstrip(GUTTER)
    for marker in MARKERS:
        if body.startswith(marker):
            body = body[len(marker):]
            break
    return body.strip().lstrip(GUTTER).strip()


def line_index(path: Path) -> dict[str, list[int]]:
    """Where each whitespace-free line of a Linux file occurs, ≥ 20 chars only."""
    table: dict[str, list[int]] = defaultdict(list)
    for number, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
        key = norm(line)
        if len(key) >= 20:
            table[key].append(number)
    return table


def hunk_lines(source: list[str], row: int, table: dict[str, list[int]],
               window: int) -> list[tuple[int, list[int]]]:
    """The quoted lines near `row`, each with the Linux positions its text has.

    The hunk is the run of comment lines touching the cite, allowing up to two
    unmatched comment lines (a blank separator, a sentence of prose) inside it.
    The walk stops at a code line, at a line carrying its own citation, and at the
    window edge, so one cite is never judged against a neighbouring cite's text.
    A ` ``` ` fence delimiter stops neither side of the walk and is never itself a
    quoted line: the commonest layout in this workspace is a prose cite one line
    above the fence that opens its own quotation, and treating the delimiter as a
    boundary would leave every one of those cites unchecked.  Positions stay
    grouped per quoted Rust line, because a short C statement such as
    `if (!arg || nr_args != 1)` recurs verbatim dozens of times in a Linux file: a
    cite is satisfied by *any* of a line's candidate positions, and flattening the
    two together would flag a correct range.
    """
    lines: list[tuple[int, list[int]]] = []
    for direction in (-1, 1):
        gap = 0
        offset = direction
        while abs(offset) <= window:
            index = row - 1 + offset
            if not 0 <= index < len(source):
                break
            text = source[index].strip()
            body = strip_comment(text)
            if not text.lstrip(GUTTER).startswith(("/", "*", "!")):
                break
            if FENCE.match(body):
                offset += direction
                continue
            if CITE.search(body) or "`" in body:
                break
            key = norm(body)
            positions = table.get(key) if len(key) >= 20 else None
            if positions:
                lines.append((index + 1, list(positions)))
                gap = 0
            else:
                gap += 1
                if gap > 2:
                    break
            offset += direction
    return lines


def audit(linux: Path, scope: Path, window: int, quiet: bool) -> tuple[int, int]:
    index: dict[str, dict[str, list[int]]] = {}

    def positions(rel: str) -> dict[str, list[int]]:
        if rel not in index:
            path = linux / rel
            index[rel] = line_index(path) if path.is_file() else {}
        return index[rel]

    checked = misses = 0
    for path in sorted(scope.rglob("*.rs")):
        source = path.read_text(errors="replace").splitlines()
        for number, raw in enumerate(source, 1):
            named: str | None = None
            for cite in CITE.finditer(raw):
                rel = cite.group(1) or named
                if cite.group(1):
                    named = cite.group(1)
                if rel is None:
                    continue
                lo, hi = int(cite.group(2)), int(cite.group(3) or cite.group(2))
                table = positions(rel)
                if not table:
                    continue
                lines = hunk_lines(source, number, table, window)
                if not lines:
                    continue
                checked += 1
                unexplained = [(row, candidates) for row, candidates in lines
                               if not any(lo <= at <= hi for at in candidates)]
                if not unexplained:
                    continue
                misses += 1
                if not quiet:
                    detail = "; ".join(
                        f"quoted {path}:{row} matches "
                        + ", ".join(str(at) for at in candidates[:6])
                        + ("…" if len(candidates) > 6 else "")
                        for row, candidates in unexplained[:4])
                    print(f"MISS {path}:{number} cites {rel}:{lo}-{hi}; {detail}")
    return checked, misses


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--linux", required=True, type=Path,
                        help="materialized Linux release root")
    parser.add_argument("--scope", required=True, type=Path,
                        help="directory of Rust sources to audit")
    parser.add_argument("--window", type=int, default=14,
                        help="comment lines around a cite to consider (default 14)")
    parser.add_argument("--quiet", action="store_true", help="print the summary only")
    args = parser.parse_args()

    for name, path in (("--linux", args.linux), ("--scope", args.scope)):
        if not path.is_dir():
            print(f"audit: {name} path is not a directory: {path}", file=sys.stderr)
            return 2
    checked, misses = audit(args.linux, args.scope, args.window, args.quiet)
    print(f"audited {checked} cites next to quoted text from the file they name, "
          f"{misses} do not contain that text within the range they state",
          file=sys.stderr)
    return 1 if misses else 0


if __name__ == "__main__":
    raise SystemExit(main())
