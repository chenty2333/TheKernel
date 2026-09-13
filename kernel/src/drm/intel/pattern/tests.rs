//! The pattern generator against ordinary memory.
//!
//! What these tests can establish is the geometry of the bytes: which pixel
//! gets which colour, that the padding outside the visible width is never
//! written, that the same frame always produces the same bytes, and that two
//! frames differ.  What they cannot establish is that the display engine reads
//! those bytes correctly, because nothing in this workstream has run against a
//! display engine.

use alloc::{format, vec, vec::Vec};

use super::*;

/// A surface with padding after the visible pixels of every row, so that "the
/// padding is untouched" is a statement a test can make.
struct Scratch {
    bytes: Vec<u8>,
    stride: usize,
    width: usize,
    height: usize,
    seed: u8,
}

impl Scratch {
    /// `padding` extra bytes per row, and the same again past the last row, so
    /// that the bytes beyond the end of the surface are checked too.
    fn new(width: usize, height: usize, padding: usize, seed: u8) -> Scratch {
        let stride = width * 4 + padding;
        Scratch {
            bytes: vec![seed; stride * height + padding],
            stride,
            width,
            height,
            seed,
        }
    }

    fn fill(&mut self, frame: u64) -> Result<PatternGeometry, PatternError> {
        fill_xrgb8888(&mut self.bytes, self.stride, self.width, self.height, frame)
    }

    fn pixel(&self, x: usize, y: usize) -> u32 {
        let at = y * self.stride + x * 4;
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("four bytes"))
    }

    /// Whether every byte this function promised not to write still holds the
    /// seed value.
    fn padding_untouched(&self) -> bool {
        for row in 0..self.height {
            let start = row * self.stride + self.width * 4;
            let end = (row + 1) * self.stride;
            if self.bytes[start..end].iter().any(|byte| *byte != self.seed) {
                return false;
            }
        }
        self.bytes[self.height * self.stride..]
            .iter()
            .all(|byte| *byte == self.seed)
    }
}

/// The four surface shapes the pixel-exact test runs over: a small one, the
/// target's, a ragged one, and the smallest legal one.
const SHAPES: [(usize, usize, usize); 4] = [
    (64, 48, 16),
    (1920, 1080, 256),
    (1002, 7, 64),
    (BAR_COUNT, BAR_COUNT, 0),
];

#[test]
fn every_visible_pixel_is_its_bar_colour_or_the_marker_over_it() {
    for (width, height, padding) in SHAPES {
        let mut scratch = Scratch::new(width, height, padding, 0xa5);
        let geometry = scratch
            .fill(3)
            .unwrap_or_else(|error| panic!("{width}x{height}: {}", error.describe()));
        assert_eq!(
            geometry.marker.side,
            (height / MARKER_SIDE_DIVISOR).max(1).min(width)
        );
        for y in 0..height {
            for x in 0..width {
                let under = BAR_COLORS[bar_index(width, x)];
                let in_marker = geometry.marker.contains(x, y);
                let expected = if in_marker {
                    let over = marker_colour(under);
                    assert_ne!(over, under, "the marker must not be invisible on bar {x}");
                    over
                } else {
                    under
                };
                assert_eq!(
                    scratch.pixel(x, y),
                    expected,
                    "{width}x{height}: pixel ({x}, {y})"
                );
            }
        }
    }
}

#[test]
fn the_padding_and_the_bytes_after_the_last_row_are_never_written() {
    for (width, height, padding) in SHAPES {
        let mut scratch = Scratch::new(width, height, padding, 0x5a);
        scratch
            .fill(1)
            .unwrap_or_else(|error| panic!("{width}x{height}: {}", error.describe()));
        assert!(
            scratch.padding_untouched(),
            "{width}x{height} stride {}: a byte outside the visible pixels changed",
            scratch.stride
        );
    }
}

#[test]
fn the_marker_is_neither_black_nor_invisible_on_any_bar() {
    // The marker is the other place a black region could hide inside the
    // pattern, and the complement of the white bar is black: both halves of the
    // rule are checked on every bar.
    for (bar, &color) in BAR_COLORS.iter().enumerate() {
        let over = marker_colour(color);
        assert_ne!(over, color, "the marker on bar {bar} would be invisible");
        assert_ne!(over, 0, "the marker on bar {bar} would be black");
        assert_eq!(
            over & !MARKER_COMPLEMENT,
            0,
            "bar {bar}: the X byte must be 0"
        );
    }
    assert_eq!(marker_colour(BAR_COLORS[0]), MARKER_ON_WHITE);
    assert_eq!(
        marker_colour(BAR_COLORS[7]),
        BAR_COLORS[7] ^ MARKER_COMPLEMENT
    );
}

#[test]
fn the_darkest_bar_is_not_black_so_no_visible_pixel_is_zero() {
    // The whole point of reference §11 phase 6.5: a screen filled with this
    // pattern can never be confused with a black screen, and "every visible
    // pixel is zero" is therefore a statement about the scanout alone.
    for (width, height, padding) in SHAPES {
        let mut scratch = Scratch::new(width, height, padding, 0);
        scratch.fill(0).expect("a fillable surface");
        for (row, line) in scratch.bytes[..scratch.stride * height]
            .chunks_exact(scratch.stride)
            .enumerate()
        {
            for (column, pixel) in line[..width * 4].chunks_exact(4).enumerate() {
                let word = u32::from_le_bytes(pixel.try_into().expect("four bytes"));
                assert_ne!(
                    word, 0,
                    "{width}x{height}: pixel ({column}, {row}) is black"
                );
            }
        }
    }
}

#[test]
fn two_frames_differ_exactly_by_the_two_marker_positions() {
    let (width, height, padding) = (640, 480, 64);
    let mut first = Scratch::new(width, height, padding, 0);
    let mut second = Scratch::new(width, height, padding, 0);
    let mut repeat = Scratch::new(width, height, padding, 0);
    let one = first.fill(0).expect("a fillable surface");
    let two = second.fill(1).expect("a fillable surface");
    let again = repeat.fill(0).expect("a fillable surface");
    assert_ne!(one.marker, two.marker);
    assert_eq!(one, again, "the same frame must produce the same geometry");
    assert_eq!(
        first.bytes, repeat.bytes,
        "the same frame must produce the same bytes"
    );
    assert_ne!(first.bytes, second.bytes);
    // Only the marker moved, so the two frames differ in exactly two squares:
    // the one it left, which is back to its bar colours, and the one it
    // arrived in, which is now the marker colour.  Nothing else may differ.
    assert!(
        !(one.marker.contains(two.marker.x, two.marker.y)
            || two.marker.contains(one.marker.x, one.marker.y)),
        "this test needs two distinct marker cells"
    );
    // Counted in pixels, not bytes.  The X byte of XRGB8888 is zero in every
    // pixel this generator writes, so two pixels that differ differ in at most
    // three bytes: a byte count is not four times a pixel count, and a test
    // that assumed it was failed on the first run.
    let differing = first
        .bytes
        .chunks_exact(4)
        .zip(second.bytes.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(differing, 2 * one.marker.side * one.marker.side);
}

#[test]
fn the_marker_never_leaves_the_surface_however_large_the_frame_counter() {
    for (width, height) in [(8, 8), (9, 17), (1, 1), (2, 64), (320, 200), (1920, 1080)] {
        for frame in [0u64, 1, 7, 111, 4096, u64::MAX - 1] {
            let marker = marker_rect(width, height, frame);
            assert!(marker.side >= 1, "{width}x{height}: a zero-sided marker");
            assert!(
                marker.x + marker.side <= width && marker.y + marker.side <= height,
                "{width}x{height} frame {frame}: {}",
                format!("{marker:?}")
            );
        }
    }
}

#[test]
fn the_bars_divides_the_width_within_one_column_and_every_bar_is_addressable() {
    for width in BAR_COUNT..=4096 {
        let edges = bar_boundaries(width);
        assert_eq!(edges[0], 0);
        assert_eq!(edges[BAR_COUNT], width);
        let narrowest = width / BAR_COUNT;
        let widest = narrowest + 1;
        for bar in 0..BAR_COUNT {
            let columns = edges[bar + 1] - edges[bar];
            assert!(
                columns == narrowest || columns == widest,
                "width {width}: bar {bar} has {columns} columns"
            );
            // The run the fill draws and the index the marker uses must agree
            // at both ends of every bar: the marker reads the index, the fill
            // walks the boundaries.
            assert_eq!(
                bar_index(width, edges[bar]),
                bar,
                "width {width}, bar {bar}"
            );
            assert_eq!(
                bar_index(width, edges[bar + 1] - 1),
                bar,
                "width {width}, bar {bar}, last column"
            );
        }
    }
}

#[test]
fn a_surface_that_cannot_hold_the_pattern_is_refused_by_name() {
    let mut empty: [u8; 0] = [];
    assert_eq!(
        fill_xrgb8888(&mut empty, 0, 0, 0, 0),
        Err(PatternError::Empty {
            width: 0,
            height: 0
        })
    );
    assert_eq!(
        fill_xrgb8888(&mut [0u8; 64], 64, 4, 4, 0),
        Err(PatternError::TooNarrow { width: 4 })
    );
    assert_eq!(
        fill_xrgb8888(&mut [0u8; 1024], 16, 8, 8, 0),
        Err(PatternError::StrideTooSmall {
            stride: 16,
            needed: 32
        })
    );
    assert_eq!(
        fill_xrgb8888(&mut [0u8; 64], 32, 8, 8, 0),
        Err(PatternError::SurfaceTooSmall {
            len: 64,
            needed: 256
        })
    );
    // Every error has a line a boot log can carry.
    for error in [
        PatternError::Empty {
            width: 0,
            height: 0,
        },
        PatternError::TooNarrow { width: 4 },
        PatternError::StrideTooSmall {
            stride: 16,
            needed: 32,
        },
        PatternError::SurfaceTooSmall {
            len: 64,
            needed: 256,
        },
    ] {
        assert!(!error.describe().is_empty());
    }
}

#[test]
fn a_tightly_sized_last_row_is_a_legal_surface() {
    // The padding of the last row is not written, so it is not required to
    // exist: a caller that allocated exactly `(height - 1) * stride + width * 4`
    // bytes is not refused.
    let (width, height, stride) = (BAR_COUNT, 4, 64);
    let mut bytes = vec![0u8; (height - 1) * stride + width * 4];
    let geometry = fill_xrgb8888(&mut bytes, stride, width, height, 0)
        .expect("the visible pixels all fit in this buffer");
    assert_eq!(geometry.height, height);
    // And one byte less is not.
    let mut short = vec![0u8; (height - 1) * stride + width * 4 - 1];
    assert!(matches!(
        fill_xrgb8888(&mut short, stride, width, height, 0),
        Err(PatternError::SurfaceTooSmall { .. })
    ));
}

#[test]
fn the_geometry_line_names_the_frame_and_the_marker() {
    let mut scratch = Scratch::new(320, 200, 64, 0);
    let geometry = scratch.fill(7).expect("a fillable surface");
    let text = geometry.describe();
    assert!(text.contains("320x200"), "{text}");
    assert!(text.contains("stride 1344 B"), "{text}");
    assert!(text.contains("frame 7"), "{text}");
    assert!(
        text.contains(&format!(
            "marker {}x{} at ({}, {})",
            geometry.marker.side, geometry.marker.side, geometry.marker.x, geometry.marker.y
        )),
        "{text}"
    );
}
