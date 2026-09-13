"""The framebuffer console's own glyph table, read from the kernel's source.

The console renders each character cell from a generated 8x16 table
(``kernel/src/pseudofs/dev/console_font/glyphs.rs``).  Reading that table here
-- rather than keeping a second copy of it in the test tree -- is what lets an
oracle assert that *specific text* is on the screen: the expectation is
rendered with the same font the console draws with, so a font change moves both
sides together, and there is nothing to keep in sync by hand.

The table is data; the kernel's own host tests hold its invariants
(``no_glyph_touches_the_cell_border``, full coverage of 0x20..=0x7e).
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_GLYPHS_PATH = REPO_ROOT / "kernel/src/pseudofs/dev/console_font/glyphs.rs"

_TABLE_RE = re.compile(r"const GLYPHS: \[\[u8; 16\]; (\d+)\] = \[(.*?)\n\];", re.DOTALL)
_FIRST_RE = re.compile(r"FIRST_BYTE: u8 = (0x[0-9a-fA-F]+)")
_ROW_RE = re.compile(r"\[([^\]]*)\]")
_BYTE_RE = re.compile(r"0x([0-9a-fA-F]{2})")


class ConsoleFontError(ValueError):
    """The generated table could not be read as the console's font."""


@dataclass(frozen=True)
class ConsoleFont:
    """One bitmap per byte index, each ``height`` row bytes, MSB leftmost."""

    width: int
    height: int
    first_byte: int
    glyphs: tuple[tuple[int, ...], ...]

    @classmethod
    def load(cls, path: Path = DEFAULT_GLYPHS_PATH) -> "ConsoleFont":
        try:
            source = path.read_text(encoding="utf-8")
        except OSError as error:
            raise ConsoleFontError(f"cannot read the console font table {path}: {error}") from error
        table = _TABLE_RE.search(source)
        first = _FIRST_RE.search(source)
        if table is None or first is None:
            raise ConsoleFontError(f"{path} is not the generated console font table")
        rows = [tuple(int(value, 16) for value in _BYTE_RE.findall(row))
                for row in _ROW_RE.findall(table.group(2))]
        expected = int(table.group(1))
        if len(rows) != expected or any(len(row) != 16 for row in rows):
            raise ConsoleFontError(f"{path} declares {expected} glyphs but holds {len(rows)}")
        return cls(width=8, height=16, first_byte=int(first.group(1), 16), glyphs=tuple(rows))

    def cell(self, character: str) -> tuple[int, ...]:
        """The 16 row bytes the console paints for one character."""

        index = ord(character) - self.first_byte
        if not 0 <= index < len(self.glyphs):
            raise ConsoleFontError(
                f"{character!r} is outside the console font's range "
                f"0x{self.first_byte:02x}..0x{self.first_byte + len(self.glyphs) - 1:02x}"
            )
        return self.glyphs[index]

    def cells(self, text: str) -> tuple[tuple[int, ...], ...]:
        """The cell bitmaps one console line of ``text`` occupies, in order."""

        return tuple(self.cell(character) for character in text)


def rendered_rows(font: ConsoleFont, character: str) -> list[int]:
    """Convenience for tests and evidence tools: one glyph as 16 row bytes."""

    return list(font.cell(character))
