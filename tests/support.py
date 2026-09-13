"""Shared helpers for the repository's Python test suite.

Run everything from the repository root with one command:

    python3 -m unittest discover -s tests -t .
"""

from __future__ import annotations

import importlib.util
import os
import struct
import sys
import tempfile
from pathlib import Path
from types import ModuleType


def repo_root() -> Path:
    """Return the repository root that contains this tests package."""
    return Path(__file__).resolve().parents[1]


def load_script_module(name: str, relative_path: str) -> ModuleType:
    """Load a standalone repository script as a module by path.

    Repository scripts are not importable packages; register the loaded
    module in sys.modules so dataclasses and similar helpers resolve it.
    """
    spec = importlib.util.spec_from_file_location(name, repo_root() / relative_path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_tmpdir() -> tempfile.TemporaryDirectory[str]:
    """Return a temporary directory on the host cache, never on tmpfs.

    Honors THEKERNEL_TEST_TMPDIR and defaults to ~/.cache/thekernel-test-tmp.
    """
    root = Path(
        os.environ.get("THEKERNEL_TEST_TMPDIR", Path.home() / ".cache" / "thekernel-test-tmp")
    )
    root.mkdir(parents=True, exist_ok=True)
    return tempfile.TemporaryDirectory(dir=root)


MULTIBOOT2_HEADER_MAGIC = 0xE852_50D6
LOAD_OFFSET = 0x1000
LOAD_SIZE = 0x100
ENTRY_ADDRESS = 0xFFFF_8000_0000_1000


def multiboot2_tags(extra: bytes = b"") -> bytes:
    """Return a valid tag list: address, `extra`, entry, end.

    `extra` is inserted after the address tag so a caller can add the
    framebuffer request tag, or a deliberately malformed one, without having to
    restate the tags that surround it.
    """
    address = struct.pack("<HHI", 2, 0, 24) + struct.pack(
        "<IIII", 0x2000, LOAD_OFFSET, LOAD_OFFSET + LOAD_SIZE, LOAD_OFFSET + LOAD_SIZE
    )
    entry = struct.pack("<HHI", 3, 0, 12) + struct.pack("<II", LOAD_OFFSET, 0)
    end = struct.pack("<HHI", 0, 0, 8)
    return address + extra + entry + end


def multiboot2_header(tags: bytes | None = None, length: int | None = None) -> bytes:
    """Return a Multiboot2 header with a checksum correct for its own length."""
    tags = multiboot2_tags() if tags is None else tags
    length = 16 + len(tags) if length is None else length
    checksum = (-(MULTIBOOT2_HEADER_MAGIC + 0 + length)) & 0xFFFF_FFFF
    return struct.pack("<IIII", MULTIBOOT2_HEADER_MAGIC, 0, length, checksum) + tags


def multiboot2_elf(header: bytes) -> bytes:
    """Return a 64-bit ELF with one PT_LOAD segment holding `header` first."""
    content = header.ljust(LOAD_SIZE, b"\x00")
    ident = b"\x7fELF" + bytes([2, 1, 1, 0]) + bytes(8)
    ehsize, phentsize, phnum = 64, 56, 1
    elf_header = ident + struct.pack(
        "<HHIQQQIHHHHHH",
        2,
        0x3E,
        1,
        ENTRY_ADDRESS,
        ehsize,
        0,
        0,
        ehsize,
        phentsize,
        phnum,
        64,
        0,
        0,
    )
    program = struct.pack(
        "<IIQQQQQQ", 1, 5, LOAD_OFFSET, ENTRY_ADDRESS, ENTRY_ADDRESS, LOAD_SIZE, LOAD_SIZE, 0x1000
    )
    image = elf_header + program
    return image.ljust(LOAD_OFFSET, b"\x00") + content
