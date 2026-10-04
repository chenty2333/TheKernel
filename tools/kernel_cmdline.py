"""Append boot arguments without modifying the repository's GRUB template."""
from __future__ import annotations

import re


def append_kernel_cmdline(template: str, command_line: str) -> str:
    # GRUB is an executable language. Accept literal kernel-argument tokens,
    # not command separators, variables, quoting, comments or multiline input.
    if not re.fullmatch(r"[a-zA-Z0-9_=.,:/+@%\- \t]*", command_line):
        raise ValueError("kernel command line must contain literal, single-line argument tokens")
    extra = " ".join(command_line.split())
    lines = template.splitlines(keepends=True)
    matches = [i for i, line in enumerate(lines) if re.match(r"\s*multiboot2\s+", line)]
    if len(matches) != 1:
        raise ValueError("GRUB template must contain exactly one multiboot2 command")
    if extra:
        index = matches[0]
        line = lines[index]
        ending = "\r\n" if line.endswith("\r\n") else "\n" if line.endswith("\n") else ""
        lines[index] = line.rstrip("\r\n") + " " + extra + ending
    return "".join(lines)
