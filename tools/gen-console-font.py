#!/usr/bin/env python3
"""Generate `glyphs.rs`, the 8x16 bitmap console font table the kernel draws with.

Glyphs for 0x20..=0x7e are rasterised from Liberation Mono with Pillow on a
fixed scratch grid with a known baseline, thresholded at mid grey, and cropped
into the 8x16 console cell.  The font size is not hardcoded: every size from
`MAX_SIZE` down to `MIN_SIZE` is measured and the largest one whose advance
width, ink bounding box and per-glyph coverage all fit the cell is used.

The output is data only.  The interface, the constants and the tests live in
the hand-written `console_font/mod.rs` and `console_font/tests.rs` beside it,
so regenerating the table can never delete anything a person wrote.

Usage:
    python3 tools/gen-console-font.py [kernel/src/pseudofs/dev/console_font/glyphs.rs]

The default output is the file the kernel builds, so regenerating it is a
no-op on a tree that already matches the source font.  The generator refuses
to run unless Liberation Mono is installed at the path below; the table it
emits is committed, so no build depends on this script or on that font.

The generator is deterministic: the same Pillow, font file and interpreter
produce byte-identical output.  It reads no clock, no environment and no
random source, and it writes exactly one file -- the output path above.
"""

import re
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

FONT_PATH = Path("/usr/share/fonts/liberation-mono-fonts/LiberationMono-Regular.ttf")
FONT_NAME = "Liberation Mono Regular"
FONT_LICENSE = "SIL Open Font License, Version 1.1"

FIRST_BYTE = 0x20
LAST_BYTE = 0x7E
GLYPH_COUNT = LAST_BYTE - FIRST_BYTE + 1

GLYPH_WIDTH = 8
GLYPH_HEIGHT = 16

# Mid grey.  A fully inked pixel rasterises to 255 and always survives; faint
# anti-aliasing fringes below mid grey are dropped so stems stay one pixel wide.
THRESHOLD = 128

# Scratch render target.  Every glyph is drawn with its pen at a fixed origin
# and on a fixed baseline, so all glyphs share one grid; the cell is then cut
# out of the scratch image at the offset the fit chose.
SCRATCH_WIDTH = 64
SCRATCH_HEIGHT = 64
SCRATCH_PEN_X = 16
SCRATCH_BASELINE = 32

# Candidate point sizes, largest first.
MIN_SIZE = 8
MAX_SIZE = 16

DEFAULT_OUTPUT = "kernel/src/pseudofs/dev/console_font/glyphs.rs"

CHARS = [chr(code) for code in range(FIRST_BYTE, LAST_BYTE + 1)]


def load_font(size):
    # The layout engine only affects shaping, which plain ASCII does not need,
    # but Pillow defaults to Raqm when it was built with it and the two engines
    # report slightly different advances.  Pinning BASIC keeps the measurement
    # and the raster identical on hosts without Raqm.
    return ImageFont.truetype(
        str(FONT_PATH), size, layout_engine=ImageFont.Layout.BASIC
    )


def render_mask(font, char):
    """Rasterise `char` on the scratch grid.

    Returns the anti-aliased greyscale image and its thresholded mask, in which
    an inked pixel is 255 and a clear pixel is 0.
    """
    image = Image.new("L", (SCRATCH_WIDTH, SCRATCH_HEIGHT), 0)
    ImageDraw.Draw(image).text(
        (SCRATCH_PEN_X, SCRATCH_BASELINE),
        char,
        font=font,
        anchor="ls",
        fill=255,
    )
    mask = image.point(lambda grey: 255 if grey >= THRESHOLD else 0)
    return image, mask


def measure(size):
    """Measure every glyph at `size`.

    Returns `(advance, boxes)` where `boxes` maps a character to its ink
    bounding box in scratch coordinates plus the brightest grey it produced.
    """
    font = load_font(size)
    advance = max(font.getlength(char) for char in CHARS)
    boxes = {}
    for char in CHARS:
        image, mask = render_mask(font, char)
        boxes[char] = (mask.getbbox(), image.getextrema()[1])
    return advance, boxes


def choose_size():
    """Pick the largest size whose glyphs fit the 8x16 cell.

    A size qualifies when the advance width is at most the cell width, every
    glyph still inks at least one pixel, the ink bounding box leaves at least
    one clear column and one clear row on each side of the cell for separation,
    and no glyph has lost a stroke to the threshold.
    """
    for size in range(MAX_SIZE, MIN_SIZE - 1, -1):
        advance, boxes = measure(size)
        if advance > GLYPH_WIDTH:
            continue
        inks = {char: box for char, (box, _) in boxes.items() if char != " "}
        if any(box is None for box in inks.values()):
            continue
        if any(boxes[char][1] < THRESHOLD for char in inks):
            continue
        left = min(box[0] for box in inks.values())
        top = min(box[1] for box in inks.values())
        right = max(box[2] - 1 for box in inks.values())
        bottom = max(box[3] - 1 for box in inks.values())
        if right - left + 1 > GLYPH_WIDTH - 1:
            # Ink would touch both outer columns, so glyphs could run together.
            continue
        if bottom - top + 1 > GLYPH_HEIGHT - 2:
            # No clear row would be left above and below the ink.
            continue
        return size, advance, (left, top, right, bottom)
    raise SystemExit(f"no size in {MIN_SIZE}..={MAX_SIZE} fits an 8x16 cell")


def place(ink):
    """Centre the ink block vertically and seat its left edge on column 0.

    Returns `(pen_left, baseline_row, top_margin, bottom_margin)`: the pen
    position in cell coordinates plus the clear rows that are left over.
    """
    left, top, right, bottom = ink
    height = bottom - top + 1
    top_margin = (GLYPH_HEIGHT - height) // 2
    pen_left = SCRATCH_PEN_X - left
    baseline_row = top_margin - top + SCRATCH_BASELINE
    return pen_left, baseline_row, top_margin, GLYPH_HEIGHT - height - top_margin


def build_glyphs(size, pen_left, baseline_row):
    """Render the final cells: one list of `GLYPH_HEIGHT` row bytes per glyph."""
    font = load_font(size)
    crop = (
        SCRATCH_PEN_X - pen_left,
        SCRATCH_BASELINE - baseline_row,
        SCRATCH_PEN_X - pen_left + GLYPH_WIDTH,
        SCRATCH_BASELINE - baseline_row + GLYPH_HEIGHT,
    )
    glyphs = []
    for char in CHARS:
        _, mask = render_mask(font, char)
        ink = mask.getbbox()
        if ink is not None:
            # Every inked pixel must land inside the cell: nothing is clipped.
            inside = crop[0] <= ink[0] and ink[2] <= crop[2]
            assert inside and crop[1] <= ink[1] and ink[3] <= crop[3], (
                f"{char!r}: ink {ink} escapes the cell {crop}"
            )
        cell = mask.crop(crop)
        rows = []
        for y in range(GLYPH_HEIGHT):
            byte = 0
            for x in range(GLYPH_WIDTH):
                if cell.getpixel((x, y)):
                    byte |= 0x80 >> x
            rows.append(byte)
        glyphs.append(rows)
    return glyphs


def check_cells(glyphs):
    """Verify the invariants the console relies on."""
    assert len(glyphs) == GLYPH_COUNT, f"{len(glyphs)} glyphs, want {GLYPH_COUNT}"
    for char, rows in zip(CHARS, glyphs):
        assert len(rows) == GLYPH_HEIGHT, f"{char!r} has {len(rows)} rows"
        assert all(0 <= row <= 0xFF for row in rows), f"{char!r} has a bad row byte"
        if char == " ":
            assert rows == [0] * GLYPH_HEIGHT, "space must be blank"
            continue
        assert any(rows), f"{char!r} is blank"
        inked = [
            (y, x)
            for y, row in enumerate(rows)
            for x in range(GLYPH_WIDTH)
            if row & (0x80 >> x)
        ]
        rows_used = {y for y, _ in inked}
        columns_used = {x for _, x in inked}
        assert 0 not in rows_used, f"{char!r} is clipped at row 0"
        assert GLYPH_HEIGHT - 1 not in rows_used, f"{char!r} is clipped at row 15"
        assert not (0 in columns_used and GLYPH_WIDTH - 1 in columns_used), (
            f"{char!r} inks both outer columns"
        )
        assert GLYPH_WIDTH - 1 not in columns_used, (
            f"{char!r} inks the separator column, so neighbours could touch"
        )


def comment_for(code):
    """The `// 0xNN 'c'` comment above one glyph, valid Rust in every case."""
    char = chr(code)
    if char == "'":
        literal = "'\\''"
    elif char == "\\":
        literal = "'\\\\'"
    else:
        literal = f"'{char}'"
    return f"    // 0x{code:02x} {literal}"


def rust_source(glyphs, size, advance, pen_left, baseline_row, top_margin, bottom_margin):
    """Render the generated Rust module text.

    Data only, on purpose: everything a person would want to edit -- the cell
    dimensions, the accessor and the invariant tests -- lives in the module
    beside this file, which this generator does not touch.
    """
    lines = [
        "//! The 8x16 bitmap glyph table, generated by `tools/gen-console-font.py`.",
        "//!",
        "//! **Generated file -- do not edit.**  Regenerate with",
        "//! `python3 tools/gen-console-font.py`; put interface changes in `mod.rs`",
        "//! and invariant changes in `tests.rs`, which are not generated.",
        "//!",
        f"//! Rasterised from {FONT_NAME} (`{FONT_PATH}`),",
        f"//! licensed under the {FONT_LICENSE}.",
        "//!",
        f"//! Outlines at {size} px (advance {advance:.2f} px), thresholded at grey {THRESHOLD},",
        f"//! cropped into the {GLYPH_WIDTH}x{GLYPH_HEIGHT} cell with the glyph pen at column {pen_left} and the",
        f"//! baseline on row {baseline_row}: the union ink box leaves {top_margin} clear row(s) above and",
        f"//! {bottom_margin} below, and the last column clear.",
        "",
        "/// The first byte this table has a glyph for.",
        f"pub(super) const FIRST_BYTE: u8 = 0x{FIRST_BYTE:02x};",
        "",
        "/// The last byte this table has a glyph for.",
        f"pub(super) const LAST_BYTE: u8 = 0x{LAST_BYTE:02x};",
        "",
        f"/// One glyph per byte in `FIRST_BYTE..=LAST_BYTE`, in order.",
        "///",
        f"/// Each glyph is {GLYPH_HEIGHT} rows of {GLYPH_WIDTH} pixels; within a row byte the most",
        "/// significant bit is the leftmost pixel.",
        "#[rustfmt::skip]",
        f"pub(super) const GLYPHS: [[u8; {GLYPH_HEIGHT}]; {GLYPH_COUNT}] = [",
    ]
    for code, rows in zip(range(FIRST_BYTE, LAST_BYTE + 1), glyphs):
        body = ", ".join(f"0x{row:02x}" for row in rows)
        lines.append(comment_for(code))
        lines.append(f"    [{body}],")
    lines.extend(["];", ""])
    return "\n".join(lines)


def reparse(text):
    """Parse the emitted `GLYPHS` rows back out, to prove the text is faithful."""
    rows = re.findall(r"\[((?:0x[0-9a-f]{2}, ){15}0x[0-9a-f]{2})\]", text)
    return [[int(byte, 16) for byte in row.split(", ")] for row in rows]


def main(argv):
    output = Path(argv[1]) if len(argv) > 1 else Path(DEFAULT_OUTPUT)

    if not FONT_PATH.is_file():
        raise SystemExit(f"source font not found: {FONT_PATH}")

    size, advance, ink = choose_size()
    pen_left, baseline_row, top_margin, bottom_margin = place(ink)
    glyphs = build_glyphs(size, pen_left, baseline_row)
    check_cells(glyphs)

    text = rust_source(glyphs, size, advance, pen_left, baseline_row, top_margin, bottom_margin)

    # The text must contain exactly what was rendered, no more and no less.
    assert reparse(text) == glyphs, "generated Rust does not match the rendered glyphs"

    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(text, encoding="utf-8", newline="\n")

    left, top, right, bottom = ink
    print(f"source font  : {FONT_PATH}")
    print(f"size         : {size} px (advance {advance:.2f} px, cell {GLYPH_WIDTH}x{GLYPH_HEIGHT})")
    print(f"pen/baseline : column {pen_left}, row {baseline_row}")
    print(f"ink box      : {right - left + 1}x{bottom - top + 1} px, {top_margin} clear rows above, "
          f"{bottom_margin} below, columns {left - SCRATCH_PEN_X}..{right - SCRATCH_PEN_X} used")
    print(f"glyphs       : {len(glyphs)} x {GLYPH_HEIGHT} bytes")
    print(f"output       : {output} ({output.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
