"""Check the Multiboot2 header the bootloader actually walks.

The header in ``multiboot.S`` is data embedded in the kernel image, and three
things have to agree about it or the image does not boot at all:

* the ``.int {mb2_hdr_length}`` field, which is the length the bootloader
  trusts;
* the tags that follow it, which have to fill exactly that many bytes and, in
  GRUB, each start on the boundary ``ALIGN_UP(size, 8)`` produces -- GRUB does
  not advance by ``size``;
* the checksum, because ``magic + architecture + length + checksum`` must wrap
  to zero or the bootloader never finds the header in the first place.

When they disagree the failure is not a build error.  The image is produced,
GRUB refuses it with a single line on the serial console --
``unsupported tag: 0xc`` -- and a machine with no serial port shows nothing at
all.  That is exactly how a framebuffer request tag of 20 bytes, padded by
hand to the 4-byte alignment the specification states instead of the 8-byte
alignment GRUB uses, cost an afternoon.

These tests parse both files as text.  They are deliberately independent of
the build: they need no compiler, no target, and no artifact, so they run in
the static part of the daily tier and fail in seconds rather than after a boot
that produces no output.
"""

from __future__ import annotations

import ast
import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
BOOT_RS = ROOT / "crates/ax/thekernel-axplat-x86-pc/src/boot.rs"
MULTIBOOT_S = ROOT / "crates/ax/thekernel-axplat-x86-pc/src/multiboot.S"
BUILD_RS = ROOT / "crates/ax/thekernel-axplat-x86-pc/build.rs"

# The Multiboot2 header tag alignment.  GRUB's `multiboot_mbi2.c` advances with
# `ALIGN_UP(tag->size, MULTIBOOT_TAG_ALIGN)`, and that constant is 8.
TAG_ALIGN = 8


MASK = 0xFFFF_FFFF


def _rust_const(source: str, name: str, depth: int = 0) -> int:
    """Evaluate a `const NAME: u32 = ...;` item written in literal/const form.

    Handles the additive and `wrapping_*` forms the boot crate uses, resolving
    references to other constants in the same file.  Anything else is a test
    failure rather than a silently wrong number.
    """
    if depth > 8:
        raise AssertionError(f"{name} is defined in terms of itself")
    match = re.search(rf"const\s+{name}\s*:\s*u32\s*=\s*(.+?);", source, re.S)
    if match is None:
        raise AssertionError(f"{name} not found in {BOOT_RS.name}")
    # A Rust integer literal is a Python one once the `u32` suffix is gone.
    expression = re.sub(r"\b(0[xX][0-9A-Fa-f_]+|\d[\d_]*)u32\b", r"\1", match.group(1))
    # `0.wrapping_sub(..)` is a Rust integer method call and a Python float
    # literal, so a bare integer that is immediately dotted gets parenthesised.
    expression = re.sub(r"(?<![\w.])(\d[\d_]*)(?=\s*\.)", r"(\1)", expression)
    try:
        tree = ast.parse(expression, mode="eval")
    except SyntaxError as error:
        raise AssertionError(
            f"{name} is not an expression this test can evaluate: {expression!r}"
        ) from error

    def evaluate(node: ast.AST) -> int:
        if isinstance(node, ast.Expression):
            return evaluate(node.body)
        if isinstance(node, ast.Constant) and isinstance(node.value, int):
            return node.value & MASK
        if isinstance(node, ast.BinOp) and isinstance(node.op, (ast.Add, ast.Sub)):
            left, right = evaluate(node.left), evaluate(node.right)
            return (left + right) & MASK if isinstance(node.op, ast.Add) else (left - right) & MASK
        if isinstance(node, ast.Name):
            return _rust_const(source, node.id, depth + 1)
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr in ("wrapping_add", "wrapping_sub", "wrapping_mul")
        ):
            left = evaluate(node.func.value)
            right = evaluate(node.args[0])
            if node.func.attr == "wrapping_add":
                return (left + right) & MASK
            if node.func.attr == "wrapping_sub":
                return (left - right) & MASK
            return (left * right) & MASK
        raise AssertionError(f"{name} uses {type(node).__name__}, which this test cannot evaluate")

    return evaluate(tree)


def _header_source() -> str:
    """Return the text of `multiboot2_header:` up to the 64-bit entry code."""
    source = MULTIBOOT_S.read_text()
    start = source.index("multiboot2_header:")
    end = source.index(".macro ENTRY32_COMMON")
    return source[start:end]


def _directives(header: str) -> list[tuple[int, str, str]]:
    """Return `(offset, directive, text)` for every data directive.

    Offsets are relative to the start of the header: the four words of the
    header itself come first, so the first tag opens at 16.  The text is kept
    unevaluated because `global_asm!` substitutes expressions into the header
    words, which are positions rather than values the tag structure needs.
    """
    fields: list[tuple[int, str, str]] = []
    cursor = 16
    # `_header_source` starts at the label, so the first four words are the
    # header itself -- magic, architecture, length, checksum -- not tag data.
    for line in header.splitlines()[5:]:
        stripped = line.strip()
        if not stripped.startswith("."):
            continue
        directive = stripped.split()[0]
        width = {"short": 2, "int": 4}.get(directive.lstrip("."))
        if width is None:
            raise AssertionError(f"unexpected directive in header: {line!r}")
        fields.append((cursor, directive, stripped.split()[1]))
        cursor += width
    return fields


def _tags(header: str) -> list[tuple[int, int, int]]:
    """Parse `(offset, type, size)` for every tag in the header source.

    A Multiboot2 tag always opens with `u16 type`, `u16 flags`, `u32 size`, so
    tags are recognised by that three-directive shape.  This is the important
    property: the padding some tags need is indistinguishable from data, so
    the only way to know where a tag ends is the size it declares, and the only
    way to know a tag starts is its shape.
    """
    fields = _directives(header)
    tags: list[tuple[int, int, int]] = []
    index = 0
    while index < len(fields):
        offset, directive, text = fields[index]
        shape = [field[1] for field in fields[index : index + 3]]
        if directive != ".short" or shape != [".short", ".short", ".int"]:
            raise AssertionError(
                f"offset {offset} does not open a tag "
                f"(expected .short/.short/.int, found {'/'.join(shape) or 'nothing'})"
            )
        size_text = fields[index + 2][2]
        try:
            size = int(size_text, 0)
        except ValueError as error:
            raise AssertionError(
                f"tag at {offset} has a computed size ({size_text}); GRUB needs "
                "a literal one to walk the header"
            ) from error
        tags.append((offset, int(text, 0), size))
        # The first field at or past the end the size declares.  The search
        # starts after the three words of the tag header itself, so a tag
        # shorter than its own header cannot point at its own size word.  A tag
        # whose size is not a multiple of eight ends before the next tag may
        # start; GRUB rounds up, which is what the alignment check is about.
        # Round up to the next tag boundary before looking.  The four bytes of
        # padding a 12-byte tag needs are *not* described by its size -- that
        # is the whole trap: the size says 12, so a reader that trusts the
        # specification lands on the padding and reads it as a tag header.
        end = (offset + size + TAG_ALIGN - 1) // TAG_ALIGN * TAG_ALIGN
        index = next(
            (
                position
                for position, (field_offset, _, _) in enumerate(fields)
                if field_offset >= end
            ),
            len(fields),
        )
    return tags


class Multiboot2HeaderTests(unittest.TestCase):
    def test_declared_length_matches_the_tags_that_follow_it(self) -> None:
        declared = _rust_const(BOOT_RS.read_text(), "MULTIBOOT2_HEADER_LENGTH")
        tags = _tags(_header_source())
        assembled = max(
            offset + (size + TAG_ALIGN - 1) // TAG_ALIGN * TAG_ALIGN
            for offset, _, size in tags
        )
        self.assertEqual(
            assembled,
            declared,
            "multiboot.S assembles a header of a different size than "
            f"MULTIBOOT2_HEADER_LENGTH ({declared}); the bootloader reads the "
            "length field and walks off the end of the header",
        )

    def test_every_tag_starts_on_grubs_boundary(self) -> None:
        tags = _tags(_header_source())
        kinds = [tag_type for _, tag_type, _ in tags]
        # The framebuffer request is optional and the end tag is not; both are
        # allowed in any position, so this checks membership rather than order.
        self.assertEqual(kinds[0], 2, f"expected the address tag first, got {kinds}")
        self.assertEqual(kinds[-1], 0, f"expected the end tag last, got {kinds}")
        self.assertIn(3, kinds, f"expected an entry-address tag in {kinds}")
        for kind in kinds[1:-1]:
            self.assertIn(kind, (3, 5), f"unexpected tag type {kind} in {kinds}")
        cursor = 16
        for offset, tag_type, size in tags:
            self.assertEqual(
                offset,
                cursor,
                f"tag type {tag_type} is declared at {offset} but GRUB reaches "
                f"it at {cursor}: it advances by ALIGN_UP(size, {TAG_ALIGN})",
            )
            self.assertGreaterEqual(size, 8, f"tag type {tag_type} is too short")
            self.assertEqual(size % 4, 0, f"tag type {tag_type} size is not word-aligned")
            cursor += (size + TAG_ALIGN - 1) // TAG_ALIGN * TAG_ALIGN

    def test_end_tag_terminates_the_header_exactly(self) -> None:
        header = _header_source()
        declared = _rust_const(BOOT_RS.read_text(), "MULTIBOOT2_HEADER_LENGTH")
        tags = _tags(header)
        end_offset, end_type, end_size = tags[-1]
        self.assertEqual(end_type, 0)
        self.assertEqual(
            end_offset + end_size,
            declared,
            "the end tag must be the last thing in the header: a bootloader "
            "that finds trailing bytes looks for another tag in them",
        )

    def test_checksum_makes_the_first_four_words_sum_to_zero(self) -> None:
        source = BOOT_RS.read_text()
        magic = _rust_const(source, "MULTIBOOT2_HEADER_MAGIC")
        architecture = _rust_const(source, "MULTIBOOT2_HEADER_ARCH")
        length = _rust_const(source, "MULTIBOOT2_HEADER_LENGTH")
        checksum = _rust_const(source, "MULTIBOOT2_HEADER_CHECKSUM")
        self.assertEqual(
            (magic + architecture + length + checksum) & 0xFFFF_FFFF,
            0,
            "the header checksum is wrong, so `find_header` never recognises "
            "this image as Multiboot2 and the machine boots nothing at all",
        )
        self.assertEqual(magic, 0xE852_50D6)
        self.assertEqual(architecture, 0, "i386 is the only architecture a 32-bit entry can use")

    def test_the_assembly_files_are_build_inputs(self) -> None:
        # `global_asm!(include_str!(...))` is invisible to Cargo's dependency
        # scanner, so an edit to multiboot.S changes no build input and the
        # kernel is linked from a stale copy of its own header.  The build
        # script has to say so explicitly.
        build = BUILD_RS.read_text()
        for source in ("src/multiboot.S", "src/ap_start.S", "src/kexec_transition.S"):
            self.assertIn(
                f'"{source}"',
                build,
                f"{source} is assembled but not declared with rerun-if-changed, "
                "so editing it does not rebuild the kernel",
            )


if __name__ == "__main__":
    unittest.main()
