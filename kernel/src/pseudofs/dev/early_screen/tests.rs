//! What the early screen must get right, exercised on a plain buffer.
//!
//! Every test here drives the same [`paint`] the live aperture goes through,
//! over a `Vec<u8>` instead of display memory, so a layout rule that is wrong
//! fails here rather than on a machine whose only output is the screen.

use alloc::{vec, vec::Vec};

use super::{
    BACKGROUND, BoundedText, Content, LOG_TEXT, MAX_ROWS, MESSAGE_TEXT, PANIC_BAR, RowRef, Rows,
    STATUS_BAR, TRACE_TEXT, TextView, paint, wrap_rows,
};
use crate::pseudofs::dev::{
    console_font::{GLYPH_HEIGHT, GLYPH_WIDTH, glyph},
    scanout::{ColorChannel, PixelLayout},
};

/// The 32-bit B8G8R8A8 layout a firmware most often reports.
const B8G8R8A8: PixelLayout = PixelLayout::B8G8R8A8;

/// The 16-bit RGB565 layout a firmware reports for a 16-bit mode.
const RGB565: PixelLayout = PixelLayout {
    bits: 16,
    red: ColorChannel {
        position: 11,
        size: 5,
    },
    green: ColorChannel {
        position: 5,
        size: 6,
    },
    blue: ColorChannel {
        position: 0,
        size: 5,
    },
};

/// A 24-bit layout whose channels are in the opposite order to B8G8R8A8.
const RGB888: PixelLayout = PixelLayout {
    bits: 24,
    red: ColorChannel {
        position: 0,
        size: 8,
    },
    green: ColorChannel {
        position: 8,
        size: 8,
    },
    blue: ColorChannel {
        position: 16,
        size: 8,
    },
};

/// A surface of `width` x `height` pixels whose scan lines are `pitch` bytes.
fn view(width: u32, height: u32, pitch: u32, layout: PixelLayout) -> TextView {
    TextView {
        width,
        height,
        pitch,
        len: pitch as usize * height as usize,
        layout,
    }
}

/// A slice of `view`'s own length, pre-filled with a byte no pixel uses.
///
/// A fill which is not the background is how a test tells "the renderer wrote
/// this pixel" from "the renderer never touched it".
fn surface(view: &TextView) -> Vec<u8> {
    vec![0xa5u8; view.len]
}

/// The encoded pixel at `(x, y)`, as raw bytes.
fn pixel(view: &TextView, bytes: &[u8], x: usize, y: usize) -> Vec<u8> {
    let offset = view
        .offset_of(x, y)
        .expect("test pixel is inside the surface");
    let count = view.layout.bytes_per_pixel() as usize;
    bytes[offset..offset + count].to_vec()
}

/// Whether the pixel at `(x, y)` differs from `background`.
fn is_ink(view: &TextView, bytes: &[u8], x: usize, y: usize, background: u32) -> bool {
    let offset = view
        .offset_of(x, y)
        .expect("test pixel is inside the surface");
    let (pattern, count) = view.pattern(background).expect("layout is encodable");
    bytes[offset..offset + count] != pattern[..count]
}

/// Read one scan line of cells back out of the surface as text.
///
/// This is the renderer's inverse: it rebuilds each cell's bitmap from the
/// pixels and asks the font which byte produces it.  A cell the font cannot
/// produce -- including one an out-of-range byte left blank -- reads as a
/// space, which is what the renderer drew.
fn rendered_row(view: &TextView, bytes: &[u8], row: usize, background: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for col in 0..view.cols() {
        let cell: Vec<u8> = (0..GLYPH_HEIGHT)
            .map(|dy| {
                let mut line = 0u8;
                for dx in 0..GLYPH_WIDTH {
                    if is_ink(
                        view,
                        bytes,
                        col * GLYPH_WIDTH + dx,
                        row * GLYPH_HEIGHT + dy,
                        background,
                    ) {
                        line |= 0x80 >> dx;
                    }
                }
                line
            })
            .collect();
        let byte = (0x20u8..=0x7e)
            .find(|&candidate| {
                glyph(candidate).is_some_and(|rows| rows.as_slice() == cell.as_slice())
            })
            .unwrap_or(b'?');
        out.push(byte);
    }
    while out.last() == Some(&b' ') {
        out.pop();
    }
    out
}

/// The first lit pixel of `byte`'s glyph outside the leftmost column, as
/// cell-relative offsets.
///
/// The leftmost column is excluded so that a renderer which walked the surface
/// at the wrong pixel depth would land somewhere else for this pixel and be
/// caught by the assertion rather than coincidentally agreeing.
fn first_ink(byte: u8) -> (usize, usize) {
    let rows = glyph(byte).expect("the test only asks for printable bytes");
    for dy in 0..GLYPH_HEIGHT {
        for dx in 1..GLYPH_WIDTH {
            if rows[dy] & (0x80 >> dx) != 0 {
                return (dx, dy);
            }
        }
    }
    panic!("glyph {byte:#04x} has no ink outside its first column");
}

#[test]
fn stride_comes_from_the_surface_pitch_not_the_visible_width() {
    // The failure this pins down is a renderer which multiplies the visible
    // width by the pixel size instead of asking for the stride: on a surface
    // whose firmware padded its scan lines, every row after the first lands
    // somewhere else and the screen shears.
    let view = view(8, 32, 96, B8G8R8A8);
    let mut bytes = surface(&view);
    let painted = paint(&view, &mut bytes, &Content::milestone(b"", b"A"), None);
    assert_eq!(painted.used, 2, "a status row and one tail row");
    assert_eq!(painted.dirty, 2 * GLYPH_HEIGHT);

    let (dx, dy) = first_ink(b'A');
    let expected = view
        .offset_of(dx, GLYPH_HEIGHT + dy)
        .expect("inside the surface");
    assert_eq!(expected, (GLYPH_HEIGHT + dy) * 96 + dx * 4);
    assert_eq!(
        pixel(&view, &bytes, dx, GLYPH_HEIGHT + dy),
        LOG_TEXT.to_le_bytes(),
        "the tail row's ink must sit at the surface's own pitch"
    );
    // Where a width-derived stride would have looked for it, the surface holds
    // something else.
    let wrong = (GLYPH_HEIGHT + dy) * 32 + dx * 4;
    assert_ne!(&bytes[wrong..wrong + 4], &LOG_TEXT.to_le_bytes()[..]);
}

#[test]
fn a_16_bit_surface_takes_two_bytes_per_pixel() {
    // A renderer which encoded to a `u32` and wrote all four bytes would walk
    // this surface at twice its stride, so the ink would land at the depth-4
    // offset instead.  The pitch is padded as well, so one pair of assertions
    // covers the depth and the stride together.
    let view = view(8, 48, 20, RGB565);
    let mut bytes = surface(&view);
    paint(&view, &mut bytes, &Content::milestone(b"", b"A"), None);

    let (dx, dy) = first_ink(b'A');
    let offset = view
        .offset_of(dx, GLYPH_HEIGHT + dy)
        .expect("inside the surface");
    assert_eq!(offset, (GLYPH_HEIGHT + dy) * 20 + dx * 2);
    let encoded = RGB565.encode(LOG_TEXT).to_le_bytes();
    assert_eq!(&bytes[offset..offset + 2], &encoded[..2]);
    let depth_four = (GLYPH_HEIGHT + dy) * 20 + dx * 4;
    assert_ne!(&bytes[depth_four..depth_four + 2], &encoded[..2]);
}

#[test]
fn a_24_bit_surface_takes_three_bytes_per_pixel() {
    let view = view(8, 32, 32, RGB888);
    let mut bytes = surface(&view);
    paint(&view, &mut bytes, &Content::milestone(b"", b"A"), None);

    let (dx, dy) = first_ink(b'A');
    let offset = (GLYPH_HEIGHT + dy) * 32 + dx * 3;
    assert_eq!(view.offset_of(dx, GLYPH_HEIGHT + dy), Some(offset));
    assert_eq!(
        &bytes[offset..offset + 3],
        &RGB888.encode(LOG_TEXT).to_le_bytes()[..3]
    );
}

#[test]
fn a_line_longer_than_the_screen_wraps_at_the_column_limit() {
    // Two columns of text, so five bytes need three rows.
    let view = view(16, 80, 64, B8G8R8A8);
    let mut bytes = surface(&view);
    let painted = paint(&view, &mut bytes, &Content::milestone(b"", b"abcde"), None);
    assert_eq!(painted.used, 4, "status, then ab / cd / e");
    assert_eq!(rendered_row(&view, &bytes, 0, STATUS_BAR), b"");
    assert_eq!(rendered_row(&view, &bytes, 1, BACKGROUND), b"ab");
    assert_eq!(rendered_row(&view, &bytes, 2, BACKGROUND), b"cd");
    assert_eq!(rendered_row(&view, &bytes, 3, BACKGROUND), b"e");
}

#[test]
fn a_line_longer_than_the_screen_is_clipped_on_the_status_row() {
    // The status line is composed outside the wrapper, so it is the one row
    // which can arrive longer than the screen.  It must be clipped inside the
    // surface rather than run off the end of the last scan line.
    let view = view(16, 16, 80, B8G8R8A8);
    let mut bytes = surface(&view);
    let painted = paint(
        &view,
        &mut bytes,
        &Content::milestone(b"0123456789", b""),
        None,
    );
    assert_eq!(painted.used, 1);
    assert_eq!(rendered_row(&view, &bytes, 0, STATUS_BAR), b"01");
    // Every byte after the two visible pixels -- the rest of the scan line
    // and its padding -- carries the bar colour, so nothing of the firmware's
    // own image is left in the rows this frame covers.
    let bar = STATUS_BAR.to_le_bytes();
    assert!(
        bytes[8..80].chunks_exact(4).all(|pixel| pixel == bar),
        "the frame repaints the whole scan line, padding included"
    );
}

#[test]
fn the_screen_keeps_the_newest_log_lines() {
    // Four rows in total: one status line and the three newest log lines.
    let view = view(80, 64, 320, B8G8R8A8);
    let mut bytes = surface(&view);
    paint(
        &view,
        &mut bytes,
        &Content::milestone(b"", b"one\ntwo\nthree\nfour\nfive\n"),
        None,
    );
    assert_eq!(rendered_row(&view, &bytes, 1, BACKGROUND), b"three");
    assert_eq!(rendered_row(&view, &bytes, 2, BACKGROUND), b"four");
    assert_eq!(rendered_row(&view, &bytes, 3, BACKGROUND), b"five");
}

#[test]
fn a_shorter_frame_clears_the_rows_the_longer_one_used() {
    let view = view(80, 64, 320, B8G8R8A8);
    let mut bytes = surface(&view);
    let first = paint(
        &view,
        &mut bytes,
        &Content::milestone(b"", b"one\ntwo\nthree\n"),
        None,
    );
    assert_eq!(first.used, 4);
    paint(
        &view,
        &mut bytes,
        &Content::milestone(b"", b"one\n"),
        Some(first.used),
    );
    assert_eq!(rendered_row(&view, &bytes, 3, BACKGROUND), b"");
    assert_eq!(rendered_row(&view, &bytes, 1, BACKGROUND), b"one");
}

#[test]
fn an_out_of_range_glyph_byte_draws_a_blank_cell() {
    // The log carries control bytes and UTF-8 continuation bytes.  A renderer
    // which substituted a box for each of them would bury the text around
    // them, so the cell must simply be empty.
    let view = view(80, 64, 320, B8G8R8A8);
    let mut bytes = surface(&view);
    paint(
        &view,
        &mut bytes,
        &Content::milestone(b"", &[b'A', 0x01, 0xff, 0x80, b'B']),
        None,
    );
    assert_eq!(rendered_row(&view, &bytes, 1, BACKGROUND), b"A   B");
}

#[test]
fn the_first_frame_erases_everything_the_firmware_left() {
    // The aperture arrives holding whatever the firmware's splash put there,
    // and on a machine whose only output is the screen a stale image is
    // indistinguishable from a kernel which never booted.
    let view = view(48, 80, 224, B8G8R8A8);
    let mut bytes = surface(&view);
    let painted = paint(&view, &mut bytes, &Content::milestone(b"boot", b"log\n"), None);
    assert_eq!(
        painted.dirty, view.height as usize,
        "the first frame clears the whole surface"
    );
    assert!(
        !bytes.contains(&0xa5),
        "every byte of the aperture is repainted"
    );
}

#[test]
fn a_panic_frame_shows_the_message_the_backtrace_and_the_log_tail() {
    // Wide enough that every logical line is exactly one screen row, so the
    // expected layout is the panic text's own shape.
    let view = view(320, 320, 1280, B8G8R8A8);
    let mut bytes = surface(&view);
    let content = Content {
        status: b"*** PANIC ***",
        alert: true,
        message: b"panicked at src/main.rs:1:1:\nno boot device",
        trace: b"fp=0x1, ip=0x2\nfp=0x3, ip=0x4",
        tail: b"last log line\n",
    };
    let painted = paint(&view, &mut bytes, &content, None);
    assert_eq!(painted.used, 6, "status, two message rows, two frames, one log row");
    assert_eq!(rendered_row(&view, &bytes, 0, PANIC_BAR), b"*** PANIC ***");
    assert_eq!(
        rendered_row(&view, &bytes, 1, BACKGROUND),
        b"panicked at src/main.rs:1:1:"
    );
    assert_eq!(rendered_row(&view, &bytes, 2, BACKGROUND), b"no boot device");
    assert_eq!(rendered_row(&view, &bytes, 3, BACKGROUND), b"fp=0x1, ip=0x2");
    assert_eq!(rendered_row(&view, &bytes, 4, BACKGROUND), b"fp=0x3, ip=0x4");
    assert_eq!(rendered_row(&view, &bytes, 5, BACKGROUND), b"last log line");
}

#[test]
fn a_panic_never_spends_the_whole_screen_on_the_message() {
    // A panic whose message is thousands of bytes long must still leave the
    // log tail -- the record of where the machine got to -- on the screen.
    let view = view(80, 320, 320, B8G8R8A8);
    let mut bytes = surface(&view);
    let message = vec![b'x'; 8192];
    let content = Content {
        status: b"*** PANIC ***",
        alert: true,
        message: &message,
        trace: b"",
        tail: b"the last thing the kernel said\n",
    };
    let painted = paint(&view, &mut bytes, &content, None);
    assert!(
        painted.used < view.rows(),
        "the message and the backtrace must not take every row"
    );
    assert!(
        rendered_row(&view, &bytes, 1, BACKGROUND).starts_with(b"xxxx"),
        "the message still comes first"
    );
    assert_eq!(
        rendered_row(&view, &bytes, painted.used - 1, BACKGROUND),
        b"ernel said",
        "the newest log line is the last row this frame drew"
    );
}

#[test]
fn carriage_returns_do_not_become_blank_cells() {
    // A CRLF pair is one line break, not a break and a blank cell.
    let rows = wrap_rows(b"ab\r\ncd\n\n", 80, 8, false);
    let text: Vec<RowRef> = rows.iter().collect();
    assert_eq!(text.len(), 3, "ab, cd and the deliberate blank line");
    assert_eq!(
        (text[0].range(), text[1].range(), text[2].range()),
        (0..2, 4..6, 7..7)
    );
}

#[test]
fn rows_beyond_the_ceiling_are_never_addressed() {
    // A surface taller than the renderer's ceiling keeps the rows above it and
    // stops, rather than looping for a geometry it did not choose.
    let view = view(
        8,
        (MAX_ROWS as u32 + 40) * GLYPH_HEIGHT as u32,
        32,
        B8G8R8A8,
    );
    assert_eq!(view.rows(), MAX_ROWS);
    let mut bytes = surface(&view);
    let painted = paint(&view, &mut bytes, &Content::milestone(b"", b"one\n"), None);
    assert!(painted.used <= MAX_ROWS);
    assert_eq!(painted.dirty, view.height as usize);
}

#[test]
fn degenerate_geometry_paints_nothing() {
    for view in [
        view(0, 0, 0, B8G8R8A8),
        view(4, 8, 16, B8G8R8A8),
        view(8, 8, 32, B8G8R8A8),
    ] {
        let mut bytes = surface(&view);
        let before = bytes.clone();
        let painted = paint(&view, &mut bytes, &Content::milestone(b"x", b"y"), None);
        assert_eq!(painted.used, 0);
        assert_eq!(painted.dirty, 0);
        assert_eq!(bytes, before, "a screen with no cell must stay untouched");
    }
}

#[test]
fn an_empty_row_set_reports_no_rows() {
    let rows = Rows::new(false);
    assert_eq!(rows.count, 0);
    assert_eq!(rows.iter().count(), 0);
    assert_eq!(rows.newest(4).count(), 0);
}

#[test]
fn a_bounded_text_reports_truncation_on_a_character_boundary() {
    use core::fmt::Write as _;

    let mut text = BoundedText::<16>::new();
    // Two-byte characters, so a naive cut would split one and leave the buffer
    // unprintable.
    let _ = write!(text, "{}", "\u{e9}".repeat(20));
    let rendered = core::str::from_utf8(text.as_bytes()).expect("truncation keeps UTF-8 valid");
    assert!(rendered.ends_with(" [truncated]"), "{rendered:?}");
    assert!(rendered.starts_with('\u{e9}'));
}

#[test]
fn a_panic_message_and_a_backtrace_are_drawn_in_their_own_colours() {
    let view = view(80, 320, 320, B8G8R8A8);
    let mut bytes = surface(&view);
    let content = Content {
        status: b"status",
        alert: false,
        message: b"M",
        trace: b"T",
        tail: b"",
    };
    paint(&view, &mut bytes, &content, None);
    let (dx, dy) = first_ink(b'M');
    assert_eq!(
        pixel(&view, &bytes, dx, GLYPH_HEIGHT + dy),
        MESSAGE_TEXT.to_le_bytes()
    );
    let (dx, dy) = first_ink(b'T');
    assert_eq!(
        pixel(&view, &bytes, dx, 2 * GLYPH_HEIGHT + dy),
        TRACE_TEXT.to_le_bytes()
    );
}
