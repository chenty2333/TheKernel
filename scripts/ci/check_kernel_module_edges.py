#!/usr/bin/env python3
"""Keep module-level coupling inside the kernel crate from spreading sideways.

`scripts/ci/check_cargo_dependency_layers.py` can only see edges between cargo
packages.  Within the `thekernel` crate the same freedom is invisible to it: any
module may `use crate::<other>` and reach into another subsystem without a
manifest line changing, which is exactly why `kernel/src/drm` is 61 274 lines
of kernel that cannot be lifted into a crate of its own -- the measurement, and
the five dependencies that make it so, are in
`docs/design/kernel-module-coupling.md`.  This gate pins the set of
module-level `use crate::…` edges to the reviewed baseline in
`config/kernel-module-edges.toml`: a new edge fails the run, and an edge that has
been retired is reported so the baseline can shrink.

Self-edges (`drm` using `crate::drm`) are not coupling and are not recorded.
The unit counted is a `use crate::…` statement, not every path expression: a
module that spells a foreign item out inline is still coupled, so retiring a
baseline edge proves the import is gone, not the dependency.
"""
from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT / "scripts/ci") not in sys.path:
    sys.path.insert(0, str(ROOT / "scripts/ci"))

from linux_abi_gate import mask_rust_noncode  # noqa: E402

KERNEL_SRC = ROOT / "kernel/src"
BASELINE = ROOT / "config/kernel-module-edges.toml"
USE_CRATE = re.compile(r"\buse\s+(?:pub(?:\([^)]*\))?\s+)?crate::")
IDENTIFIER = re.compile(r"[a-z_][a-z0-9_]*")


def module_of(path: Path) -> str:
    relative = path.relative_to(KERNEL_SRC)
    if len(relative.parts) == 1:
        return relative.stem
    return relative.parts[0]


def statement(text: str, start: int) -> str:
    """Return the `use …;` statement beginning at `start`, or up to the line end."""
    end = text.find(";", start)
    if end < 0:
        end = text.find("\n", start)
    return text[start:min(end, len(text))]


def split_top_level(group: str) -> list[str]:
    """Split a use tree on the commas that are not inside braces."""
    entries, depth, current = [], 0, []
    for char in group:
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
        if char == "," and depth == 0:
            entries.append("".join(current))
            current = []
        else:
            current.append(char)
    if "".join(current).strip():
        entries.append("".join(current))
    return entries


def head(text: str) -> set[str]:
    """The module names a use tree reaches at the crate root."""
    text = text.strip()
    if text.startswith("{"):
        end = text.rindex("}")
        return {name for entry in split_top_level(text[1:end]) for name in head(entry)}
    match = IDENTIFIER.match(text)
    if not match or match.group(0) == "self":
        return set()
    # `crate::file::Table` names the module `file`; anything past the first `::`
    # is an item inside it, not a module of its own.
    return {match.group(0)}


def targets(statement_text: str) -> set[str]:
    index = statement_text.find("crate::")
    return set() if index < 0 else head(statement_text[index + len("crate::"):])


def edges() -> dict[str, set[str]]:
    """Collect `module -> {crate targets}` from every Rust file in the crate."""
    result: dict[str, set[str]] = {}
    for path in sorted(KERNEL_SRC.rglob("*.rs")):
        source = mask_rust_noncode(path.read_text(encoding="utf-8"))
        source_module = module_of(path)
        for match in USE_CRATE.finditer(source):
            for target in targets(statement(source, match.start())):
                if target != source_module:
                    result.setdefault(source_module, set()).add(target)
    return result


INLINE_CRATE = re.compile(r"\bcrate::([a-z_][a-z0-9_]*)")


def inline_edges() -> dict[str, set[str]]:
    """Collect inline `crate::<target>::...` edges that occur outside `use` statements."""
    result: dict[str, set[str]] = {}
    for path in sorted(KERNEL_SRC.rglob("*.rs")):
        source = mask_rust_noncode(path.read_text(encoding="utf-8"))
        source_module = module_of(path)
        lines = source.splitlines()
        in_use = False
        for line in lines:
            stripped = line.strip()
            if stripped.startswith("use ") or stripped.startswith("pub use ") or stripped.startswith("pub(crate) use "):
                in_use = True
            if in_use:
                if ";" in stripped:
                    in_use = False
                continue
            for match in INLINE_CRATE.finditer(line):
                target = match.group(1)
                if target != source_module:
                    result.setdefault(source_module, set()).add(target)
    return result


def all_edges() -> dict[str, set[str]]:
    """Union of `use crate::` edges and inline `crate::` edges."""
    res = {k: set(v) for k, v in edges().items()}
    for mod, targets_ in inline_edges().items():
        res.setdefault(mod, set()).update(targets_)
    return res


def pair_set(pairs: list[str]) -> dict[str, set[str]]:
    graph: dict[str, set[str]] = {}
    for pair in pairs:
        source, separator, target = (part.strip() for part in pair.partition("->"))
        if not separator or not source or not target:
            raise ValueError(f'edge {pair!r} must read "source -> target"')
        if target.startswith("crate::"):
            raise ValueError(f'edge {pair!r} repeats the crate root: both sides name a module under kernel/src')
        if not (IDENTIFIER.fullmatch(source) and IDENTIFIER.fullmatch(target)):
            raise ValueError(f'edge {pair!r} must name exactly one module on each side')
        graph.setdefault(source, set()).add(target)
    if len(pairs) != len(set(pairs)):
        raise ValueError("baseline declares a duplicate edge")
    return graph


def label(path: Path) -> str:
    """How the baseline is named in a message: repo-relative when it is in the repo."""
    return str(path.relative_to(ROOT)) if path.is_relative_to(ROOT) else str(path)


def main(argv: list[str] | None = None) -> int:
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--audit-inline", action="store_true", help="audit inline crate::<target>:: cross-module paths")
    args = parser.parse_args([] if argv is None else argv)

    if args.audit_inline:
        current_use = edges()
        current_inline = inline_edges()
        all_inline_pairs = sorted(f"{s} -> {t}" for s, ts in current_inline.items() for t in ts)
        hidden_pairs = sorted(
            f"{s} -> {t}"
            for s, ts in current_inline.items()
            for t in ts
            if t not in current_use.get(s, set())
        )
        print(f"kernel module inline audit: {len(all_inline_pairs)} inline cross-module edges across {len(current_inline)} modules")
        print(f"kernel module inline audit: {len(hidden_pairs)} edges bypass use declarations (hidden coupling):")
        for edge in hidden_pairs:
            print(f"  {edge}")
        return 0

    try:
        declared = tomllib.loads(BASELINE.read_text(encoding="utf-8"))
        baseline = pair_set(declared["edges"])
    except (OSError, KeyError, TypeError, ValueError, tomllib.TOMLDecodeError) as error:
        print(f"kernel module edges: {label(BASELINE)} is unusable: {error}", file=sys.stderr)
        return 1
    if not isinstance(declared.get("schema"), int) or declared["schema"] != 1:
        print("kernel module edges: baseline schema must be 1", file=sys.stderr)
        return 1
    current = edges()
    fresh = sorted(f"{source} -> {target}"
                   for source, targets_ in current.items()
                   for target in targets_ - baseline.get(source, set()))
    retired = sorted(f"{source} -> {target}"
                     for source, targets_ in baseline.items()
                     for target in targets_ - current.get(source, set()))
    total = sum(len(items) for items in current.values())
    if fresh:
        print(f"kernel module edges: {len(fresh)} new module-level `use crate::` edges "
              f"({total} total, baseline {sum(len(items) for items in baseline.values())}):",
              file=sys.stderr)
        print(*fresh, sep="\n", file=sys.stderr)
        print(f"Lift the boundary instead of widening it, or record the edge in "
              f"{label(BASELINE)} with the reason it is acceptable.", file=sys.stderr)
        return 1
    if retired:
        print(f"kernel module edges: {total} edges ({len(current)} modules); "
              f"{len(retired)} baseline edges are gone and may leave the list: {retired}")
    else:
        print(f"kernel module edges: {total} edges across {len(current)} modules, exactly the committed baseline")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
