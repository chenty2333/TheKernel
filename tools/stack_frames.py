#!/usr/bin/env python3
"""Fail if a kernel function's stack frame could run off a task stack.

This kernel's task stacks come straight from `alloc::alloc`
(`crates/ax/thekernel-axtask/src/task.rs`, `TaskStack::try_alloc`) with no guard
page, and they are `task-stack-size` from `config/kernel.toml`.  A function
whose frame is larger than that does not fault cleanly: Rust emits a stack probe
that writes one zero per page as it walks down, so the frame lands *below* the
stack, in whatever the allocator put there.

That is not hypothetical.  `kernel/src/task/ops.rs`'s
`exit_status_trace_dump` reserved 364 KiB -- it copied a 182 KiB ring array by
value twice -- against a 256 KiB stack, on a path any process reaches by reading
`/proc/sys/kernel/exit-status`.  Nothing in the gates looked at frame sizes, so
this script does.

Usage:
    python3 tools/stack_frames.py <release kernel ELF>
    python3 tools/stack_frames.py --state <state directory>

Pass the release *bare-metal* kernel with its symbols -- the ELF under the
state's `target/` tree, not the stripped copy the build leaves in `out/`.  A
release image carries one adjustment per frame, so the size of a frame is the
size of its largest `sub`; without symbols a frame split across several
adjustments cannot be added up, and the check falls back to the largest single
one (and says so).  `--state` takes the newest release kernel under
`<state>/target/thekernel/`, which is what a gate wants after its build stage,
and prints the path it chose.

It reads the bound from `config/kernel.toml` rather than taking it as an
argument, so the check follows the configuration the image was built from.
Exits 0 when every frame fits, 1 when one does not, and 2 when the image could
not be read or the arguments were wrong -- a gate must be able to tell "this
tree has a frame that does not fit" from "this check did not run".
"""
from __future__ import annotations

import re
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import NoReturn

REPO_ROOT = Path(__file__).resolve().parent.parent

# Exit codes: 1 is "this tree has a frame that does not fit", so anything that
# stops the check from running has to be something else.  A bare
# `SystemExit("text")` exits 1, which is how a gate would read an unreadable
# image as a real finding.
USAGE_ERROR = 2


def refuse(message: str) -> NoReturn:
    print(f"stack_frames: {message}", file=sys.stderr)
    raise SystemExit(USAGE_ERROR)

FUNCTION = re.compile(r"^[0-9a-f]+ <(.+)>:$")
# Rust's stack probe: `sub $0x5b000,%r11` sets the target, then a 0x1000 loop
# walks to it.  A plain `sub $0x...,%rsp` is the same thing for smaller frames.
FRAME = re.compile(r"\bsub\s+\$0x([0-9a-f]+),%(r11|rsp)\b")

# The size above which a frame is worth printing even when it fits.
INTERESTING = 32 * 1024
LISTED = 10


def task_stack_size() -> int:
    config = tomllib.loads((REPO_ROOT / "config/kernel.toml").read_text())
    size = config.get("task-stack-size")
    if not isinstance(size, int) or size <= 0:
        refuse("config/kernel.toml has no usable task-stack-size")
    return size


def demangle(names: list[str]) -> dict[str, str]:
    """Rust v0 demangling, when the host's `c++filt` understands it.

    A missing or older `c++filt` is not an error: the mangled name identifies
    the function too, it is just harder to read.
    """
    unique = sorted(set(names))
    if not unique:
        return {}
    try:
        done = subprocess.run(
            ["c++filt", "-s", "rust"],
            input="\n".join(unique),
            capture_output=True,
            text=True,
            check=True,
        ).stdout.splitlines()
    except (OSError, subprocess.CalledProcessError):
        return {}
    if len(done) != len(unique):
        return {}
    return dict(zip(unique, done))


def disassembly(elf: Path) -> str:
    """`objdump -d` of `elf`, or a `SystemExit(2)` naming what was wrong."""
    try:
        done = subprocess.run(
            ["objdump", "-d", "--no-show-raw-insn", str(elf)],
            capture_output=True,
            text=True,
            check=True,
        )
    except FileNotFoundError as error:
        refuse("objdump is not installed")
    except subprocess.CalledProcessError as error:
        detail = str(next(
            (
                line.strip()
                for line in (error.stderr or "").splitlines()
                if line.strip()
            ),
            f"objdump exited {error.returncode}",
        ))
        refuse(f"not a readable object file: {elf}: {detail}")
    return done.stdout


def newest_kernel(state: Path) -> Path:
    """The newest release kernel ELF a build left under `state`.

    A state directory holds one tree per platform, profile and memory variant,
    so the build that just ran is the newest of them.  The path is printed
    rather than assumed, because a gate's verdict is only as good as the image
    it was reached on.
    """
    candidates = [
        path
        for path in state.glob("target/thekernel/**/release/thekernel")
        if path.is_file()
    ]
    if not candidates:
        refuse(f"no built kernel under {state}/target/thekernel; build first")
    return max(candidates, key=lambda path: path.stat().st_mtime)


def frames(elf: Path) -> tuple[list[tuple[int, int, int, str]], bool]:
    """Per function: its largest stack adjustment, their sum, how many, name.

    The largest single `sub` is the frame the compiler reserved up front, which
    is what the stack probe walks and what a release build emits.  The sum is
    the over-approximation that catches a frame split across several
    adjustments; it is only meaningful when the image has per-function symbols,
    which is what the returned flag reports.
    """
    function = "?"
    worst: dict[str, int] = {}
    total: dict[str, int] = {}
    count: dict[str, int] = {}
    labels: set[str] = set()
    for line in disassembly(elf).splitlines():
        header = FUNCTION.match(line.strip())
        if header:
            function = header.group(1)
            labels.add(function)
            worst.setdefault(function, 0)
            total.setdefault(function, 0)
            count.setdefault(function, 0)
            continue
        match = FRAME.search(line)
        if not match:
            continue
        value = int(match.group(1), 16)
        # An 8-bit sign-extended displacement is a small negative move, not a
        # frame size.
        if value >= 1 << 63:
            continue
        worst[function] = max(worst.get(function, 0), value)
        total[function] = total.get(function, 0) + value
        count[function] = count.get(function, 0) + 1
    found = sorted(
        ((worst[name], total[name], count[name], name) for name in worst),
        reverse=True,
    )
    # A stripped image names every instruction after its section (`.text`), so
    # the sums below would add up the whole binary.  Those labels do not count
    # as symbols.
    named = sum(1 for label in labels if not label.startswith(".")) > 1
    return found, named


def choose(argv: list[str]) -> Path:
    if len(argv) == 1 and not argv[0].startswith("-"):
        elf = Path(argv[0])
        if not elf.is_file():
            refuse(f"no such file: {elf}")
        return elf
    if len(argv) == 2 and argv[0] == "--state":
        state = Path(argv[1])
        if not state.is_dir():
            refuse(f"no such state directory: {state}")
        return newest_kernel(state)
    print(__doc__.strip(), file=sys.stderr)
    raise SystemExit(USAGE_ERROR)


def main() -> int:
    elf = choose(sys.argv[1:])

    bound = task_stack_size()
    print(f"stack_frames: checking {elf}")
    found, named = frames(elf)
    if not found:
        refuse(f"no instructions to check: {elf} is not a kernel image")
    names = demangle([name for _, _, _, name in found])

    def label(function: str) -> str:
        return names.get(function, function)

    print(f"task stack: {bound} bytes ({bound // 1024} KiB); largest frames:")
    for value, _, _, function in found[:LISTED]:
        print(f"  {value / 1024:9.1f} KiB  {label(function)}")
    hidden = [entry for entry in found[LISTED:] if entry[0] >= INTERESTING]
    if hidden:
        print(f"  ... and {len(hidden)} more at or above {INTERESTING // 1024} KiB")
    if not named:
        print(
            "stack_frames: no per-function symbols; only a function's largest "
            "single adjustment was checked",
            file=sys.stderr,
        )

    over = [entry for entry in found if entry[0] >= bound]
    for value, _, _, function in over:
        print(
            f"stack_frames: {label(function)} reserves {value / 1024:.1f} KiB, "
            f"which does not fit a {bound // 1024} KiB task stack",
            file=sys.stderr,
        )
    split = [
        entry
        for entry in found
        if named and entry[0] < bound <= entry[1]
    ]
    for worst_value, sum_value, steps, function in split:
        print(
            f"stack_frames: {label(function)} adjusts the stack by "
            f"{sum_value / 1024:.1f} KiB in {steps} steps, none of them alone "
            f"{bound // 1024} KiB: a frame this size does not fit either",
            file=sys.stderr,
        )
    if over or split:
        return 1
    largest = found[0][0]
    print(
        f"stack_frames: OK -- largest frame {largest / 1024:.1f} KiB, "
        f"{100 * largest / bound:.0f}% of the stack"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
