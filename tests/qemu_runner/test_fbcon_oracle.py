#!/usr/bin/env python3
"""Host tests for the framebuffer-console ink oracle and its acceptance suite.

The oracle exists to reject a frame a blank/not-blank check would accept: a
console which walks the scanout at the wrong stride for the surface's pixel
depth paints a busy screen full of blended colours and no readable glyph.  The
images below are built byte by byte so each failure mode is demonstrated
rather than asserted, including that one.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from tests.support import load_script_module, repo_root, test_tmpdir
from tools.qemu_runner.model import QmpCheckpoint, QmpTextCells
from tools.qemu_runner.process import (
    ProcessError,
    TextGridInk,
    _ScreenshotColorMismatch,
    _validate_ppm,
    _validate_text_cells,
    measure_text_cells,
    validate_qmp_controls,
)

ROOT = repo_root()

INK = (0xD0, 0xD0, 0xD0)
BACKGROUND = (0, 0, 0)

# The 8x16 cell the console paints with.  `PIXELS` is one glyph's worth of ink:
# five columns wide, seven rows tall, doubled vertically, which is the shape of
# the built-in font this oracle deliberately does not re-encode.
GLYPH = (
    "01110",
    "10001",
    "10001",
    "11111",
    "10001",
    "10001",
    "10001",
)


GLYPH_INK = sum(row.count("1") for row in GLYPH) * 2


def ppm(width: int, height: int, pixels: bytes) -> bytes:
    """Assemble a P6 PPM from raw RGB triplets."""

    assert len(pixels) == width * height * 3
    return b"P6\n%d %d\n255\n" % (width, height) + pixels


def blank(width: int, height: int, colour: tuple[int, int, int] = BACKGROUND) -> bytearray:
    return bytearray(bytes(colour) * (width * height))


def put_glyph(image: bytearray, width: int, column: int, row: int,
              cell_width: int = 8, cell_height: int = 16) -> None:
    """Draw one console glyph with the console's own placement rules."""

    for glyph_row, bits in enumerate(GLYPH):
        for dy in (0, 1):
            y = row * cell_height + glyph_row * 2 + dy
            for dx, bit in enumerate(bits):
                if bit == "1":
                    x = column * cell_width + dx
                    offset = (y * width + x) * 3
                    image[offset : offset + 3] = bytes(INK)


def console(width_cells: int, height_cells: int, inked_cells: int = 0) -> tuple[int, int, bytearray]:
    """A console-sized image with `inked_cells` glyph cells filled in order."""

    width, height = width_cells * 8, height_cells * 16
    image = blank(width, height)
    for index in range(inked_cells):
        put_glyph(image, width, index % width_cells, index // width_cells)
    return width, height, image


def write_ppm(directory: Path, name: str, data: bytes) -> Path:
    path = directory / name
    path.write_bytes(data)
    return path


def any_non_background_pixel(image: bytes) -> bool:
    """The weaker check the ink oracle has to be strictly stronger than."""

    return any(image[offset : offset + 3] != bytes(BACKGROUND) for offset in range(0, len(image), 3))


class TextCellOracleTests(unittest.TestCase):
    """Every failure mode of the oracle, on images built for the purpose."""

    def test_empty_image_file_is_rejected(self) -> None:
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "empty.ppm", b"")
            with self.assertRaisesRegex(ProcessError, "empty"):
                _validate_ppm(path, None, (), QmpTextCells())

    def test_missing_image_file_is_rejected(self) -> None:
        with test_tmpdir() as directory:
            with self.assertRaisesRegex(ProcessError, "was not created"):
                _validate_ppm(Path(directory) / "absent.ppm", None, (), QmpTextCells())

    def test_truncated_pixel_payload_is_rejected(self) -> None:
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "short.ppm", b"P6\n4 4\n255\n" + bytes(4 * 4 * 3 - 3))
            with self.assertRaisesRegex(ProcessError, "incomplete"):
                _validate_ppm(path, None, (), QmpTextCells())

    def test_all_background_image_is_rejected(self) -> None:
        width, height, image = console(16, 2)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "black.ppm", ppm(width, height, bytes(image)))
            with self.assertRaisesRegex(_ScreenshotColorMismatch, "0 ink pixels"):
                _validate_ppm(path, None, (), QmpTextCells())
            # The screen is a perfectly valid picture of nothing; only the ink
            # floor distinguishes it from a rendered console.
            self.assertFalse(any_non_background_pixel(bytes(image)))

    def test_one_glyph_of_ink_passes_only_the_floors_it_meets(self) -> None:
        width, height, image = console(16, 2, inked_cells=1)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "one.ppm", ppm(width, height, bytes(image)))
            with self.assertRaisesRegex(_ScreenshotColorMismatch, "below the 512"):
                _validate_ppm(path, None, (), QmpTextCells())
            # One glyph is real ink: a run whose screen holds a single glyph
            # must pass, which is what makes the default floor a policy rather
            # than a proxy for "is the image non-blank".
            measured = measure_text_cells(path, QmpTextCells(min_ink_pixels=1, min_inked_cells=1))
            self.assertEqual(measured, TextGridInk(16, 2, GLYPH_INK, 1, 0, 0))

    def test_rendered_console_text_passes(self) -> None:
        width, height, image = console(40, 4, inked_cells=40)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "text.ppm", ppm(width, height, bytes(image)))
            cells = QmpTextCells(min_ink_pixels=512, min_inked_cells=32)
            _validate_ppm(path, (width, height), (), cells)
            measured = measure_text_cells(path, cells)
            self.assertEqual((measured.columns, measured.rows), (40, 4))
            self.assertEqual(measured.inked_cells, 40)
            self.assertEqual(measured.ink_pixels, 40 * GLYPH_INK)
            self.assertEqual(measured.foreign_pixels, 0)
            self.assertEqual(measured.outside_ink_pixels, 0)

    def test_ink_only_outside_the_expected_region_is_rejected(self) -> None:
        # A grid that covers only the left half of a frame whose text is on the
        # right: "some pixels are non-black" is true, "text where text is
        # expected" is not.
        width, height, image = console(16, 2, inked_cells=0)
        for index in range(8):
            put_glyph(image, width, 8 + index % 8, index // 8)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "right.ppm", ppm(width, height, bytes(image)))
            cells = QmpTextCells(columns=8, min_ink_pixels=1, min_inked_cells=1)
            with self.assertRaisesRegex(_ScreenshotColorMismatch, "outside the 8x2-cell text grid"):
                _validate_ppm(path, None, (), cells)
            # The same image passes once the grid covers where the ink is,
            # which proves the rejection was about location and not ink count.
            _validate_ppm(path, None, (), cells.__class__(columns=16, min_ink_pixels=1,
                                                          min_inked_cells=1))

    def test_ink_in_the_unpainted_margin_is_rejected(self) -> None:
        width, height, image = console(16, 2, inked_cells=32)
        # One pixel of ink below the last whole text row.
        offset = ((height - 1) * width + 1) * 3
        image[offset : offset + 3] = bytes(INK)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "margin.ppm", ppm(width, height, bytes(image)))
            cells = QmpTextCells(rows=1, min_ink_pixels=1, min_inked_cells=1)
            with self.assertRaisesRegex(_ScreenshotColorMismatch, "outside the 16x1-cell text grid"):
                _validate_ppm(path, None, (), cells)

    def test_wrong_dimensions_are_rejected(self) -> None:
        width, height, image = console(16, 2, inked_cells=32)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "size.ppm", ppm(width, height, bytes(image)))
            with self.assertRaisesRegex(ProcessError, "expected 800x600"):
                _validate_ppm(path, (800, 600), (), QmpTextCells())
            # An image that is not a whole number of character cells cannot be
            # the grid the console draws on at all.
            odd = write_ppm(Path(directory), "odd.ppm", ppm(width, height - 1,
                                                           bytes(blank(width, height - 1))))
            with self.assertRaisesRegex(ProcessError, "not a whole number of 16-pixel"):
                _validate_ppm(odd, None, (), QmpTextCells())

    def test_declared_grid_larger_than_the_image_is_rejected(self) -> None:
        width, height, image = console(16, 2, inked_cells=32)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "small.ppm", ppm(width, height, bytes(image)))
            with self.assertRaisesRegex(ProcessError, "does not fit"):
                _validate_ppm(path, None, (), QmpTextCells(columns=17))

    def test_wrong_stride_render_is_rejected_while_a_blank_check_accepts_it(self) -> None:
        """The bug class this oracle exists for, reproduced end to end.

        A 16-bit-per-pixel surface painted by a console that writes a whole
        `u32` per pixel and advances four bytes: the ink lands on pixel pairs
        the surface does not have, so QEMU's conversion to RGB produces colours
        which are neither the console's ink nor its background.
        """

        width_cells, height_cells = 16, 2
        row_bytes = width_cells * 8 * 2
        surface = bytearray(row_bytes * height_cells * 16)
        for row in range(height_cells):
            for column in range(width_cells):
                for glyph_row, bits in enumerate(GLYPH):
                    for dy in (0, 1):
                        y = row * 16 + glyph_row * 2 + dy
                        for dx, bit in enumerate(bits):
                            if bit == "1":
                                # Four bytes per pixel on a two-byte surface.
                                offset = y * row_bytes + (column * 8 + dx) * 4
                                surface[offset : offset + 4] = b"\xd0\xd0\xd0\x00"
        pixels = bytearray()
        for index in range(0, len(surface), 2):
            value = surface[index] | (surface[index + 1] << 8)
            red = (value >> 11) & 0x1F
            green = (value >> 5) & 0x3F
            blue = value & 0x1F
            pixels += bytes((red << 3 | red >> 2, green << 2 | green >> 4, blue << 3 | blue >> 2))
        image = ppm(width_cells * 8, height_cells * 16, bytes(pixels))
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "stride.ppm", image)
            # A blank/not-blank check accepts this frame outright.
            self.assertTrue(any_non_background_pixel(pixels))
            with self.assertRaisesRegex(_ScreenshotColorMismatch,
                                        "wrong stride for the surface's pixel depth"):
                _validate_ppm(path, None, (), QmpTextCells(min_ink_pixels=1, min_inked_cells=1))

    def test_ink_on_a_cell_border_is_rejected_only_when_the_font_guarantees_it(self) -> None:
        """A misaligned render keeps the exact colours and still paints off-grid.

        The kernel's font never inks a cell's last column or its first and last
        rows, so nothing the console draws can put ink there.  This is the one
        rule that sees a frame shifted by a pixel rather than recoloured.
        """

        width, height, image = console(16, 2, inked_cells=0)
        put_glyph(image, width, 0, 0)
        # The same glyph, one pixel to the right: it now inks column 7 of its
        # cell, which no console glyph can do.
        for glyph_row, bits in enumerate(GLYPH):
            for dy in (0, 1):
                y = glyph_row * 2 + dy
                for dx, bit in enumerate(bits):
                    if bit == "1":
                        offset = (y * width + 1 + dx) * 3
                        image[offset : offset + 3] = bytes(INK)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "shifted.ppm", ppm(width, height, bytes(image)))
            strict = QmpTextCells(min_ink_pixels=1, min_inked_cells=1,
                                  require_clear_cell_borders=True)
            with self.assertRaisesRegex(_ScreenshotColorMismatch, "on a character cell's border"):
                _validate_ppm(path, None, (), strict)
            # Without the rule the frame is indistinguishable from real text:
            # the colours are exactly the console's, and the shift is
            # sub-cell.  That gap is why the rule exists.
            lenient = QmpTextCells(min_ink_pixels=1, min_inked_cells=1)
            _validate_ppm(path, None, (), lenient)
            measured = measure_text_cells(path, strict)
            self.assertGreater(measured.border_ink_pixels, 0)

    def test_foreign_pixels_inside_the_grid_are_rejected(self) -> None:
        width, height, image = console(16, 2, inked_cells=32)
        # Firmware splash residue: a colour the console never paints.
        offset = (height - 1) * width * 3
        image[offset : offset + 3] = bytes((0, 160, 0))
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "splash.ppm", ppm(width, height, bytes(image)))
            cells = QmpTextCells(min_ink_pixels=1, min_inked_cells=1)
            with self.assertRaisesRegex(_ScreenshotColorMismatch, "neither ink"):
                _validate_ppm(path, None, (), cells)
            # A declared budget is the only way to accept a known exception.
            _validate_ppm(path, None, (), QmpTextCells(min_ink_pixels=1, min_inked_cells=1,
                                                       max_foreign_pixels=1))

    def test_threshold_failures_are_retryable_and_structural_ones_are_not(self) -> None:
        """The QMP controller polls only the failures a repaint can fix."""

        width, height, image = console(16, 2, inked_cells=32)
        with test_tmpdir() as directory:
            path = write_ppm(Path(directory), "retry.ppm", ppm(width, height, bytes(image)))
            with self.assertRaises(_ScreenshotColorMismatch):
                _validate_ppm(path, None, (), QmpTextCells(min_ink_pixels=1 << 20))
            with self.assertRaises(ProcessError) as caught:
                _validate_ppm(path, None, (), QmpTextCells(columns=99))
            self.assertNotIsInstance(caught.exception, _ScreenshotColorMismatch)

    def test_text_cell_expectation_is_validated_before_any_image_is_read(self) -> None:
        for cells, message in (
            (QmpTextCells(x=-1), "non-negative origin"),
            (QmpTextCells(cell_width=0), "cell size must be positive"),
            (QmpTextCells(columns=0), "columns must be positive"),
            (QmpTextCells(rows=-2), "rows must be positive"),
            (QmpTextCells(ink=(256, 0, 0)), "channels must be in 0..255"),
            (QmpTextCells(ink=BACKGROUND), "must differ"),
            (QmpTextCells(min_ink_pixels=0), "floors must be positive"),
            (QmpTextCells(max_foreign_pixels=-1), "budget must be non-negative"),
        ):
            with self.subTest(cells=cells), self.assertRaisesRegex(ProcessError, message):
                _validate_text_cells(cells)

    def test_oracle_without_a_screenshot_is_rejected(self) -> None:
        with self.assertRaisesRegex(ProcessError, "requires a screenshot"):
            validate_qmp_controls(
                screenshot=None, input_events=(), input_after_marker=None,
                screenshot_after_marker=None, timeout_secs=1.0, screenshot_size=None,
                screenshot_color_blocks=(), checkpoints=(),
                screenshot_text_cells=QmpTextCells(),
            )
        with self.assertRaisesRegex(ProcessError, "checkpoint screenshot oracle"):
            validate_qmp_controls(
                screenshot=None, input_events=(), input_after_marker=None,
                screenshot_after_marker=None, timeout_secs=1.0, screenshot_size=None,
                screenshot_color_blocks=(),
                checkpoints=(QmpCheckpoint(input_after_marker="READY",
                                           screenshot_text_cells=QmpTextCells()),),
            )


if __name__ == "__main__":
    unittest.main()
