//! The bring-up test pattern: vertical colour bars with a marker that moves.
//!
//! Reference §11 phase 6.5 is why this module exists:
//!
//! > **Fill the framebuffer with a known pattern** — vertical colour bars, not
//! > solid black.  A solid black framebuffer is indistinguishable from a black
//! > screen caused by every other failure in this list.  **This is the single
//! > highest-value debugging decision in the whole document.**
//!
//! The whole of stage 2 is judged by what appears on a monitor, and the target
//! machine has no serial port: the screen is the only output device.  That
//! makes the framebuffer's *contents* a diagnostic instrument, and an
//! instrument has to be one a person can read.  So:
//!
//! * **Eight vertical bars**, left to right, in the fixed order below.  The
//!   set is the familiar colour-bar order rather than an arbitrary palette,
//!   because a person reading a photograph of the panel is comparing it against
//!   something they have seen before.
//! * **Every pixel is non-zero, the marker included.**  The darkest bar is a
//!   near-black grey (`0x0010_1010`) and not `0x0000_0000`, so that "the visible
//!   surface is all zeroes" is a statement about the *scanout* and never about
//!   the pattern.  If the pattern contained a black region, a black screen
//!   would be ambiguous again inside that region — which is the exact failure
//!   §11 phase 6.5 was written to remove.  The marker is the other place a
//!   black region could hide, so it too is never black: see
//!   [`MARKER_ON_WHITE`].
//! * **A marker whose position depends on the frame counter**, drawn as the
//!   complement of the bar beneath it.  A static image cannot be told from a
//!   frozen one, and "is it scanning?" is the question §11 phase 6.1 asks.
//!   [`PatternGeometry::marker`] says where the marker should be for the frame
//!   that was written, so a photograph or a second look at the screen can be
//!   checked against the log without measuring anything by eye.
//!
//! # The surface
//!
//! The pattern is written into a **linear XRGB8888** surface: one 32-bit
//! little-endian word per pixel, `0x00RRGGBB`, with the top byte written as
//! zero because the plane's format ignores it (`PLANE_CTL_FORMAT_XRGB_8888`,
//! reference §5.5).  The surface's **stride is a parameter and is not assumed
//! to be `width * 4`**: reference §11 phase 3.2 asks for a stride that is a
//! multiple of 64 bytes (256 is safest), so on a 1920-wide surface there are
//! 256 bytes of padding per row.  A generator that assumed `width * 4` would
//! shear the image, and the shear would look like a plane-position bug.
//!
//! Nothing outside the visible columns of a row is written, by construction and
//! by test.  The bytes past the last visible pixel of the last visible row need
//! not exist at all, so a tightly sized allocation is a legal surface.
//!
//! # What this module deliberately does not do
//!
//! * It does not allocate, map, or touch the GGTT.  The caller owns the
//!   surface and passes a byte slice; this module writes pixels into it and
//!   nothing else.  On the target that slice is the memory the display engine
//!   reads through the GGTT (reference §11 phase 3.2), which is a mapping
//!   decision that belongs to the framebuffer allocation, not here.
//! * It does not cache or resize anything.  The whole pattern is a pure
//!   function of `(width, height, frame)`, so the same frame always produces
//!   the same bytes and a regression is a byte comparison.
//! * It does not touch the hardware.  Nothing in this file has run against a
//!   real display engine; what the tests establish is the geometry of the
//!   bytes, on the host, over an ordinary buffer.

use alloc::{format, string::String};

/// The number of vertical bars.
///
/// Fixed rather than a parameter: the point of the pattern is that a person
/// recognises it, and a bar count that varies per call is a pattern that has to
/// be looked up before it can be read.
pub(crate) const BAR_COUNT: usize = 8;

/// The bar colours, left to right, as XRGB8888 words.
///
/// The top byte is zero: the plane's format is XRGB8888, whose X byte is
/// ignored (reference §5.5).
///
/// The last entry is a near-black grey rather than `0x0000_0000`; see the
/// module documentation for why the difference matters.
pub(crate) const BAR_COLORS: [u32; BAR_COUNT] = [
    0x00ff_ffff, // white
    0x00ff_ff00, // yellow
    0x0000_ffff, // cyan
    0x0000_ff00, // green
    0x00ff_00ff, // magenta
    0x00ff_0000, // red
    0x0000_00ff, // blue
    0x0010_1010, // near-black
];

/// How much of the surface's height one marker square covers.
///
/// The marker's side is `height / MARKER_SIDE_DIVISOR`, at least one pixel and
/// never wider than the surface.  On a 1080-line surface that is a 135-pixel
/// square: large enough to see from across a room, small enough that it does
/// not hide a bar.
pub(crate) const MARKER_SIDE_DIVISOR: usize = 8;

/// The marker is drawn as the complement of the bar underneath it, so it is
/// visible on every bar without a per-bar colour table.
///
/// The price is recorded rather than hidden: the complement of the green bar is
/// the magenta bar's colour, so a marker sitting on green is *coloured* like
/// another bar.  What identifies it is its position, which is why
/// [`PatternGeometry::marker`] travels with the fill and is logged: the marker
/// is the thing that moves.
pub(crate) const MARKER_COMPLEMENT: u32 = 0x00ff_ffff;

/// What the marker is drawn in over the white bar.
///
/// The complement of white is black, and a black block would put back the exact
/// ambiguity §11 phase 6.5 exists to remove, inside the region a person is most
/// likely to be looking at.  Mid grey is neither a bar colour nor black, so
/// every pixel of the pattern stays non-zero.  A test found this: the first
/// version of this module drew the marker as an unconditional complement, and
/// `the_darkest_bar_is_not_black_so_no_visible_pixel_is_zero` failed on the
/// pixel at (0, 0) of frame 0.
pub(crate) const MARKER_ON_WHITE: u32 = 0x0080_8080;

/// The colour of the marker over one bar.  See [`MARKER_ON_WHITE`].
pub(crate) const fn marker_colour(under: u32) -> u32 {
    let complement = (under ^ MARKER_COMPLEMENT) & MARKER_COMPLEMENT;
    if complement == 0 {
        MARKER_ON_WHITE
    } else {
        complement
    }
}

/// Why a surface cannot carry the pattern.
///
/// Every variant names the number that was wrong, because the caller is a
/// boot path with no interactive debugger: the log line is the whole
/// diagnostic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PatternError {
    /// The surface has no pixels to fill.
    Empty { width: usize, height: usize },
    /// Fewer columns than bars.  Some bars would have zero width, so the
    /// pattern would no longer be the pattern this module promises.
    TooNarrow { width: usize },
    /// A row is shorter than the visible pixels it must hold, so consecutive
    /// rows would overlap and the image would shear.
    StrideTooSmall { stride: usize, needed: usize },
    /// The buffer does not reach the last visible pixel.
    SurfaceTooSmall { len: usize, needed: usize },
}

impl PatternError {
    /// One line, in the shape the rest of the driver's errors use.
    pub(crate) fn describe(&self) -> String {
        match self {
            PatternError::Empty { width, height } => {
                format!("cannot fill a {width}x{height} surface: it has no pixels")
            }
            PatternError::TooNarrow { width } => format!(
                "cannot fill a {width}-column surface: the pattern is {BAR_COUNT} bars and every \
                 one of them must be at least a column wide"
            ),
            PatternError::StrideTooSmall { stride, needed } => format!(
                "stride {stride} is smaller than the {needed} bytes a row of visible pixels \
                 needs, so rows would overlap"
            ),
            PatternError::SurfaceTooSmall { len, needed } => {
                format!("the surface is {len} bytes but the last visible pixel ends at {needed}")
            }
        }
    }
}

/// Where the moving marker is in one frame.
///
/// Exposed because it is the answer to "what should be on the screen for frame
/// N?", which is the only way a photograph taken minutes later can be checked
/// against the frame the log recorded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct MarkerRect {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) side: usize,
}

impl MarkerRect {
    /// Whether a pixel is inside the marker.
    pub(crate) const fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.side && y >= self.y && y < self.y + self.side
    }
}

/// What one fill drew, for the log and for the debug file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PatternGeometry {
    pub(crate) stride: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// The frame counter the pattern was generated for.
    pub(crate) frame: u64,
    pub(crate) marker: MarkerRect,
}

impl PatternGeometry {
    /// The one line the boot log and the debug file carry.
    pub(crate) fn describe(&self) -> String {
        format!(
            "pattern: {}x{} xrgb8888, stride {} B, frame {}, marker {}x{} at ({}, {})",
            self.width,
            self.height,
            self.stride,
            self.frame,
            self.marker.side,
            self.marker.side,
            self.marker.x,
            self.marker.y,
        )
    }
}

/// The first column of each bar, with the width as a terminator.
///
/// A width that is not a multiple of [`BAR_COUNT`] is divided as evenly as
/// integer arithmetic allows: bar `i` starts at `i * width / BAR_COUNT`, so no
/// bar is more than one column wider than any other and every bar is at least
/// one column wide whenever `width >= BAR_COUNT`.  Which bars get the extra
/// column when the width does not divide is decided by the formula, not by a
/// special case.
pub(crate) fn bar_boundaries(width: usize) -> [usize; BAR_COUNT + 1] {
    let mut edges = [0usize; BAR_COUNT + 1];
    for (bar, edge) in edges.iter_mut().enumerate() {
        *edge = bar * width / BAR_COUNT;
    }
    edges[BAR_COUNT] = width;
    edges
}

/// Which bar a column belongs to: the inverse of [`bar_boundaries`].
///
/// This **searches the boundaries** rather than computing a second formula.
/// The obvious-looking formula, `x * BAR_COUNT / width`, is not the inverse of
/// `i * width / BAR_COUNT`: at width 1002 with eight bars the first bar starts
/// at column 125, and `125 * 8 / 1002` is 0, so the two disagree exactly at a
/// bar edge.  The fill walks the boundaries and the marker asks for the index,
/// so a disagreement is a marker drawn in the wrong bar's colour — and it is
/// what `every_visible_pixel_is_its_bar_colour_or_the_marker_over_it` found on
/// the first run of these tests.
pub(crate) fn bar_index(width: usize, x: usize) -> usize {
    let edges = bar_boundaries(width);
    edges
        .partition_point(|edge| *edge <= x)
        .saturating_sub(1)
        .min(BAR_COUNT - 1)
}

/// The marker's position for a frame.
///
/// The marker walks the surface on a grid of its own size, one cell per frame,
/// and wraps when it reaches the end.  A grid rather than a free position so
/// that the marker always lies wholly inside the surface: a marker that clipped
/// at an edge would change its visible size between frames, which is a second
/// difference between frames that has nothing to do with the frame counter.
///
/// The caller must pass a non-zero `width` and `height`; [`fill_xrgb8888`]
/// refuses those before it gets here.
pub(crate) fn marker_rect(width: usize, height: usize, frame: u64) -> MarkerRect {
    let side = (height / MARKER_SIDE_DIVISOR).max(1).min(width);
    let columns = (width - side) / side + 1;
    let rows = (height - side) / side + 1;
    let positions = (columns * rows) as u64;
    let index = (frame % positions) as usize;
    MarkerRect {
        x: (index % columns) * side,
        y: (index / columns) * side,
        side,
    }
}

/// Fill a linear XRGB8888 surface with the bring-up test pattern.
///
/// `stride` is the distance in bytes between the starts of two rows and is
/// deliberately independent of `width`; the bytes from `width * 4` to `stride`
/// in each row, and everything past the last visible row, are never written.
///
/// The cost is linear in the visible pixels: every visible pixel is written
/// exactly once (the marker overwrites the pixels it covers), and the padding
/// is not touched at all.  A 1920x1080 surface is 2,073,600 pixels, which is a
/// single pass over 8 MiB of memory.
pub(crate) fn fill_xrgb8888(
    surface: &mut [u8],
    stride: usize,
    width: usize,
    height: usize,
    frame: u64,
) -> Result<PatternGeometry, PatternError> {
    if width == 0 || height == 0 {
        return Err(PatternError::Empty { width, height });
    }
    if width < BAR_COUNT {
        return Err(PatternError::TooNarrow { width });
    }
    let row_bytes = width * 4;
    if stride < row_bytes {
        return Err(PatternError::StrideTooSmall {
            stride,
            needed: row_bytes,
        });
    }
    // The last row's *padding* is not part of the surface this function
    // promises to write, so it is not required to exist either.
    let needed = (height - 1) * stride + row_bytes;
    if surface.len() < needed {
        return Err(PatternError::SurfaceTooSmall {
            len: surface.len(),
            needed,
        });
    }

    let edges = bar_boundaries(width);
    for row in 0..height {
        let start = row * stride;
        let line = &mut surface[start..start + row_bytes];
        for (bar, color) in BAR_COLORS.iter().enumerate() {
            write_color(&mut line[edges[bar] * 4..edges[bar + 1] * 4], *color);
        }
    }

    let marker = marker_rect(width, height, frame);
    for row in marker.y..marker.y + marker.side {
        let start = row * stride;
        for column in marker.x..marker.x + marker.side {
            let under = BAR_COLORS[bar_index(width, column)];
            write_pixel(
                &mut surface[start + column * 4..start + column * 4 + 4],
                marker_colour(under),
            );
        }
    }

    Ok(PatternGeometry {
        stride,
        width,
        height,
        frame,
        marker,
    })
}

/// Write one pixel about which the caller already knows everything.
fn write_pixel(pixel: &mut [u8], color: u32) {
    pixel.copy_from_slice(&color.to_le_bytes());
}

/// Write a run of pixels of one colour.
///
/// The surface is little-endian in memory, which is what the display engine
/// reads; `to_le_bytes` says so in the code rather than relying on the host's
/// byte order.
fn write_color(pixels: &mut [u8], color: u32) {
    let bytes = color.to_le_bytes();
    for pixel in pixels.as_chunks_mut::<4>().0 {
        *pixel = bytes;
    }
}

#[cfg(test)]
mod tests;
