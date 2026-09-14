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
    python3 tools/stack_frames.py OUT/x86_64/<plat>/<profile>/<mem>/kernel-x86_64

It reads the bound from `config/kernel.toml` rather than taking it as an
argument, so the check follows the configuration the image was built from.
Exits 0 when every frame fits, 1 when one does not, and 2 on a usage error.
"""
from __future__ import annotations

import re
import subprocess
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

FUNCTION = re.compile(r"^[0-9a-f]+ <(.+)>:$")
# Rust's stack probe: `sub $0x5b000,%r11` sets the target, then a 0x1000 loop
# walks to it.  A plain `sub $0x...,%rsp` is the same thing for smaller frames.
FRAME = re.compile(r"\bsub\s+\$0x([0-9a-f]+),%(r11|rsp)\b")


def task_stack_size() -> int:
    config = tomllib.loads((REPO_ROOT / "config/kernel.toml").read_text())
    size = config.get("task-stack-size")
    if not isinstance(size, int) or size <= 0:
        raise SystemExit("config/kernel.toml has no usable task-stack-size")
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


def frames(elf: Path) -> list[tuple[int, str]]:
    disassembly = subprocess.run(
        ["objdump", "-d", str(elf)], capture_output=True, text=True, check=True
    ).stdout
    function = "?"
    found: list[tuple[int, str]] = []
    for line in disassembly.splitlines():
        header = FUNCTION.match(line.strip())
        if header:
            function = header.group(1)
            continue
        match = FRAME.search(line)
        if not match:
            continue
        value = int(match.group(1), 16)
        # An 8-bit sign-extended displacement is a small negative move, not a
        # frame size.
        if value >= 1 << 63:
            continue
        found.append((value, function))
    return sorted(found, reverse=True)


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    elf = Path(sys.argv[1])
    if not elf.is_file():
        print(f"stack_frames: no such file: {elf}", file=sys.stderr)
        return 2

    bound = task_stack_size()
    found = frames(elf)
    names = demangle([function for _, function in found])
    print(f"task stack: {bound} bytes ({bound // 1024} KiB); largest frames:")
    for value, function in found[:10]:
        print(f"  {value / 1024:9.1f} KiB  {names.get(function, function)}")
    over = [(value, function) for value, function in found if value >= bound]
    for value, function in over:
        print(
            f"stack_frames: {names.get(function, function)} reserves "
            f"{value / 1024:.1f} KiB, which does not fit a {bound // 1024} KiB task stack",
            file=sys.stderr,
        )
    if over:
        return 1
    largest = found[0][0] if found else 0
    print(
        f"stack_frames: OK -- largest frame {largest / 1024:.1f} KiB, "
        f"{100 * largest / bound:.0f}% of the stack"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
