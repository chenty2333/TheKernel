#!/usr/bin/env python3
"""Count Rust lines that reproduce a Linux 7.2.3 source line verbatim.

`docs/upstream-provenance.md` states a policy for the `crates/` tree: excerpts of
Linux C are allowed as cited documentation, and every claim about how many there
are is a measurement.  This is the instrument that makes those claims checkable
instead of narrative.  It is not a CI stage -- it needs a materialized Linux
release, which is a 2 GB tree and a network fetch -- so it is run by whoever
edits a `NOTICE` or a provenance row, and its output is what those files must
agree with.

Run it as::

    python3 scripts/ci/scan_linux_excerpts.py \\
        --linux /home/ava/Desktop/linux-7.2.3 --scope crates/linux --threshold 40

with `--show-lines` for the per-site line inventory a table needs.  `--scope`
takes any directory of Rust sources, and `--group-by 1` rolls the counts up per
package, which is what a per-crate `NOTICE` must quote.

The matching rule, stated so a disagreement can be localized:

* The reference set is every line of every `*.c` and `*.h` under the release
  root with **all** whitespace removed, kept at the requested length and hashed
  with blake2b to 8 bytes.  A match is therefore byte-identity modulo a 64-bit
  digest, whose collision chance over a kernel tree is on the order of 1e-12.
* A Rust line is a candidate when, after removing its leading whitespace, its
  comment marker, and any remaining whitespace, it matches a reference line.
* A candidate is classified by where it sits: `fenced` (inside a fenced block in
  a doc comment), `unfenced-doc` (doc-comment prose), `line-comment` (`//`), or
  `code` (a real Rust statement).
* A candidate built only from `- = # * _ + |` and spaces is a drawing, not a
  sentence, and is excluded from every count unless `--include-rules` is given.
  `bpf/src/uapi.rs` and `futex/src/lib.rs` are otherwise 28 matches of nothing
  but `// ----------` rule lines.
* A fenced block counts as *marked* when the line that opens it is preceded,
  outside the fence, by a doc line carrying `Excerpt:`.  The header reports how
  many matched blocks are marked and how many marks exist in the scope, so a
  citation-precision claim is a scan output rather than a hand count.
"""
from __future__ import annotations

import argparse
import hashlib
import sys
from collections import Counter, defaultdict
from pathlib import Path

RULE_CHARS = "-=#*_+| "
# A quoted hunk is often drawn with a nesting gutter (`├──┤return -EINVAL;`) to
# stand in for the C indentation it reproduces.  The gutter is a rendering
# artifact, not text, so it is stripped before comparing; box-drawing characters
# are excluded, so an ASCII `-----` rule line is still caught by RULE_CHARS and a
# real C `-EINVAL` still starts with its minus.
GUTTER = "├└┌┐┘┤│─ "


def norm(text: str) -> bytes:
    """The bytes a line is compared on: everything but its whitespace."""
    return "".join(text.split()).encode("utf-8", errors="replace")


def candidate(text: str) -> bytes:
    """A Rust line as compared: gutter and whitespace removed."""
    return norm(text.lstrip(GUTTER))


def reference(root: Path, minimum: int, quiet: bool) -> tuple[set[bytes], int]:
    """Hash every reference line of at least `minimum` characters in `root`."""
    hashes: set[bytes] = set()
    seen = 0
    files = sorted(root.rglob("*.c")) + sorted(root.rglob("*.h"))
    for index, path in enumerate(files):
        try:
            data = path.read_bytes()
        except OSError:
            continue
        for line in data.split(b"\n"):
            key = norm(line.decode("utf-8", errors="replace"))
            if len(key) >= minimum:
                seen += 1
                hashes.add(hashlib.blake2b(key, digest_size=8).digest())
        if not quiet and index and index % 20000 == 0:
            print(f"  reference: {index}/{len(files)} files", file=sys.stderr)
    return hashes, seen


def classify(path: Path) -> tuple[list[tuple[str, str, int, int]], set[int]]:
    """`(category, marker-stripped text, fence region, 1-based line number)` per line.

    The fence region number lets a caller count distinct fenced blocks rather
    than distinct runs of matching lines: a quoted hunk whose blank and `*`
    lines match nothing is still one block.  The second value is the set of
    regions opened under an `Excerpt:` doc line, which is how a citation-precision
    claim gets measured instead of counted by hand.
    """
    out: list[tuple[str, str, int, int]] = []
    marked: set[int] = set()
    fence = 0
    region = 0
    block = False
    pending = False
    for number, raw in enumerate(
            path.read_text(encoding="utf-8", errors="replace").splitlines(), start=1):
        text = raw.strip()
        if not text:
            continue
        if block:
            category, body = "block-comment", text.lstrip("/*").strip()
            if "*/" in text:
                block = False
        elif text.startswith("/*"):
            category, body, block = "block-comment", text.lstrip("/*").strip(), "*/" not in text
        elif text.startswith("///") or text.startswith("//!"):
            category, body = "doc", text[3:].strip()
        elif text.startswith("//"):
            category, body = "line-comment", text[2:].strip()
        else:
            category, body = "code", text
        if category == "doc":
            if body.startswith("```"):
                # The delimiter opens or closes a region and is never a candidate.
                fence = not fence
                if fence:
                    region += 1
                    if pending:
                        marked.add(region)
                        pending = False
                continue
            if not fence and "Excerpt:" in body:
                pending = True
            out.append(("fenced" if fence else "unfenced-doc", body, region if fence else 0, number))
        elif category == "block-comment":
            out.append(("unfenced-doc", body, 0, number))
        else:
            out.append((category, body, 0, number))
    return out, marked


def scan(scope: Path, hashes: set[bytes], minimum: int, include_rules: bool,
         show: list[tuple[str, int, str]] | None = None) -> tuple[dict[str, Counter], Counter]:
    """Per-file counts of verbatim Linux lines, plus scope-wide citation counts.

    When `show` is given it also receives `(relative file, line number,
    "category: text")` for every match, which is what an inventory table needs.
    The second return value holds `markers` (fenced blocks opened by an
    `Excerpt:` line) and `marked_blocks` (of those, how many hold a match), which
    is what the "citation precision" claim in `docs/upstream-provenance.md` needs.
    """
    results: dict[str, Counter] = {}
    marks: Counter = Counter()
    for path in sorted(scope.rglob("*.rs")):
        counter: Counter = Counter()
        blocks: set[int] = set()
        rel = str(path.relative_to(scope))
        lines, marked = classify(path)
        marks["markers"] += len(marked)
        for category, text, region, number in lines:
            key = candidate(text)
            if len(key) < minimum:
                continue
            if not include_rules and not key.decode().strip(RULE_CHARS):
                continue
            if hashlib.blake2b(key, digest_size=8).digest() not in hashes:
                continue
            counter[category] += 1
            if show is not None:
                show.append((rel, number, f"{category}: {text[:88]}"))
            if category == "fenced":
                blocks.add(region)
        matched = blocks & marked
        marks["marked_blocks"] += len(matched)
        if counter:
            counter["blocks"] = len(blocks)
            counter["marked_blocks"] = len(matched)
            results[rel] = counter
    return results, marks


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Count verbatim Linux lines in a Rust tree.")
    parser.add_argument("--linux", required=True, type=Path, help="materialized Linux release root")
    parser.add_argument("--scope", required=True, type=Path, help="directory of Rust sources to scan")
    parser.add_argument("--threshold", type=int, default=40, help="shortest line that counts (default 40)")
    parser.add_argument("--include-rules", action="store_true", help="count `-----` drawing lines as matches")
    parser.add_argument("--limit", type=int, default=40, help="how many files to list")
    parser.add_argument("--group-by", type=int, default=1,
                        help="roll up by this many leading path components (0 = no rollup)")
    parser.add_argument("--show-lines", action="store_true",
                        help="print every match as scope/file.rs:line category")
    parser.add_argument("--quiet", action="store_true", help="suppress progress")
    args = parser.parse_args(argv)
    for name, path in (("linux", args.linux), ("scope", args.scope)):
        if not path.is_dir():
            print(f"scan: {name} path is not a directory: {path}", file=sys.stderr)
            return 1
    hashes, lines = reference(args.linux, args.threshold, args.quiet)
    print(f"reference: {lines} whitespace-free Linux lines of at least {args.threshold} "
          f"characters, {len(hashes)} distinct")
    show: list[tuple[str, int, str]] = [] if args.show_lines else None
    results, marks = scan(args.scope, hashes, args.threshold, args.include_rules, show)
    total: Counter = Counter()
    for counter in results.values():
        total.update(counter)
    inside = total["fenced"]
    outside = sum(total[c] for c in ("unfenced-doc", "line-comment", "code", "block-comment"))
    print(f"scope: {args.scope} at >= {args.threshold}: {inside} fenced lines in {total['blocks']} "
          f"blocks across {sum(1 for c in results.values() if c['fenced'])} files; "
          f"{outside} lines outside fences; {total['code']} of them are real code; "
          f"{marks['marked_blocks']} of the matched blocks carry an `Excerpt:` marker, "
          f"{marks['markers']} markers in the scope overall")
    if args.group_by:
        groups: dict[str, Counter] = defaultdict(Counter)
        for name, counter in results.items():
            key = "/".join(name.split("/")[:args.group_by])
            groups[key].update(counter)
            groups[key]["files"] += 1
        for key, counter in sorted(groups.items(), key=lambda item: -sum(item[1].values())):
            print(f"  [{key}] fenced={counter['fenced']} blocks={counter['blocks']} "
                  f"marked={counter['marked_blocks']} files={counter['files']} "
                  f"unfenced-doc={counter['unfenced-doc']} "
                  f"line-comment={counter['line-comment']} code={counter['code']}")
    if args.show_lines:
        for rel, number, category in show:
            print(f"  {args.scope}/{rel}:{number} {category}")
        return 0
    for name, counter in sorted(results.items(), key=lambda item: -sum(item[1].values()))[:args.limit]:
        print(f"  {name}: " + " ".join(f"{k}={v}" for k, v in sorted(counter.items())))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
