#!/usr/bin/env python3
"""Keep kernel diagnostics in the one mechanism that can answer for them.

Every message the kernel emits is decided on three axes -- severity, target, and
destination -- and the author chooses only the first two (see
`docs/debugging.md`). A bare `println!`/`ax_println!` in kernel code breaks that
in three ways at once: it has no severity, so no console level and no filter can
speak about it; it has no target, so `log_filter` cannot cap it; and it chooses a
destination itself, writing a terminal from inside the kernel while the ring that
exists to hold the boot's evidence never sees it. The records that matter -- a
device that refused to come up, a policy that failed closed -- are exactly the
ones a reader wants back, hours later, from `dmesg`.

This gate fails a new raw print in code this kernel builds into the running
system, and ratchets what is already there: `config/log-surface.toml` records a
per-file count, a file that exceeds its count fails the run, and a file under it
is reported so the baseline can shrink. It says nothing about *which* severity a
call site chose -- that is the author's, and `docs/debugging.md` states the rule
that repeating paths belong in the debug band.

The sanctioned surfaces are deliberately not banned, because each answers a
question the ring cannot:

* `log` macros (`info!` … `trace!`) -- the normal path.
* `diagnostic_println!` in the platform crate -- the pre-log boot probe, which
  runs before the ring exists and is read from `kernel.log` by the CPU suite.
* `axhal::console::emergency_diagnostic_print` -- the panic and fail-stop path,
  which must not acquire the normal logger's locks.
* `klog::record` -- the compatibility entry point legacy `ax_print` fragments
  arrive through; it *does* retain, which is why `ax_println!` is a ratchet
  rather than a hard error.
* `eprintln!`/`println!` in `build.rs`, `examples/`, `benches/` and anything
  under a `tests/` directory -- host programs, not the kernel.
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

BASELINE = ROOT / "config/log-surface.toml"

#: The macros that write a terminal directly instead of producing a record.
RAW_PRINTS = (
    "println",
    "print",
    "eprintln",
    "eprint",
    "dbg",
    "ax_println",
    "ax_print",
    "debug_println",
    "debug_print",
)

#: Directories that are never part of the running kernel.
EXCLUDED_PARTS = ("examples", "benches", "tests", "target")

#: Imported upstream trees, named in `config/log-surface.toml` rather than
#: hardcoded, whose own debug aids are upstream's to decide about.
VENDORED_KEY = "vendored"

RAW_RE = re.compile(
    r"(?<![A-Za-z0-9_:.])(" + "|".join(RAW_PRINTS) + r")!\("
)

#: `#[cfg(test)]` (in any `all(test, …)` form) and a bare `mod tests {`
#: declaration. Both introduce host test code, which may print freely.
CFG_TEST = re.compile(r"#\[[^\]]*\btest\b[^\]]*\]|\bmod\s+tests?\s*\{")


def mask_host_code(masked: str) -> str:
    """Blank the bodies of `#[cfg(test)]` items and of `mod tests` blocks.

    A test module is host code even when it sits in the middle of a file that is
    otherwise the kernel, so the scan is brace-matched from the attribute to the
    end of the block it introduces rather than to the end of the file: code that
    follows a test module is still policed.

    The input is already masked, so braces inside strings and comments cannot
    mislead the count.
    """
    out = list(masked)
    position = 0
    while True:
        found = CFG_TEST.search(masked, position)
        if found is None:
            break
        start = masked.find("{", found.start())
        if start < 0:
            break
        depth = 0
        end = start
        for offset in range(start, len(masked)):
            if masked[offset] == "{":
                depth += 1
            elif masked[offset] == "}":
                depth -= 1
                if depth == 0:
                    end = offset
                    break
        for offset in range(found.start(), end + 1):
            if out[offset] != "\n":
                out[offset] = " "
        position = end + 1
    return "".join(out)


def kernel_sources(vendored: tuple[str, ...]) -> list[Path]:
    """Every Rust file this kernel builds in, minus host-only programs."""
    found: list[Path] = []
    for root in (ROOT / "kernel/src", ROOT / "crates"):
        for path in sorted(root.rglob("*.rs")):
            relative = path.relative_to(ROOT)
            if any(part in EXCLUDED_PARTS for part in relative.parts):
                continue
            if path.name == "build.rs":
                continue
            if path.name in ("tests.rs", "dummy.rs"):
                continue
            if any(part.endswith("/tests") or part == "tests"
                   for part in relative.parent.parts):
                continue
            if relative.parent.name.endswith("tests"):
                continue
            text = relative.as_posix()
            if any(text.startswith(prefix) for prefix in vendored):
                continue
            found.append(path)
    return found


def occurrences(vendored: tuple[str, ...]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for path in kernel_sources(vendored):
        source = path.read_text(encoding="utf-8")
        # Strings and comments are masked to equal-length spaces first, so a
        # `println!` inside a documentation example or a quoted message does not
        # count; only a call the compiler would build does.
        masked = mask_host_code(mask_rust_noncode(source))
        hits = len(RAW_RE.findall(masked))
        if hits:
            counts[str(path.relative_to(ROOT))] = hits
    return counts


def main() -> int:
    baseline: dict[str, int] = {}
    vendored: tuple[str, ...] = ()
    if BASELINE.is_file():
        data = tomllib.loads(BASELINE.read_text(encoding="utf-8"))
        baseline = dict(data.get("allow", {}))
        vendored = tuple(data.get(VENDORED_KEY, ()))
    found = occurrences(vendored)

    failures: list[str] = []
    for path, count in sorted(found.items()):
        allowed = baseline.get(path)
        if allowed is None:
            failures.append(
                f"{path}: {count} raw print(s) with no baseline entry -- a kernel "
                "diagnostic is a `log` record; a pre-log boot statement is "
                "`diagnostic_println!`; a panic-path line is "
                "`axhal::console::emergency_diagnostic_print`"
            )
        elif count > allowed:
            failures.append(f"{path}: {count} raw print(s), baseline allows {allowed}")

    shrunk = {path: allowed for path, allowed in sorted(baseline.items())
              if found.get(path, 0) < allowed}

    for message in failures:
        print(f"log-surface: FAIL {message}")
    for path, allowed in sorted(shrunk.items()):
        print(f"log-surface: SHRUNK {path} now uses {found.get(path, 0)} of "
              f"{allowed}; lower config/log-surface.toml")
    total = sum(found.values())
    print(f"log-surface: {len(found)} file(s) hold {total} raw print(s); "
          f"baseline holds {sum(baseline.values())}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
