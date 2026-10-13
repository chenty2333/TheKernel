//! The 8x16 bitmap font the framebuffer console draws text with.
//!
//! One cell is [`GLYPH_WIDTH`] by [`GLYPH_HEIGHT`] pixels, and a row byte
//! carries its leftmost pixel in the most significant bit.  The table itself
//! is generated (see `glyphs.rs`); this module owns the interface the console
//! uses and the tests that hold the table's invariants down, so regenerating
//! the table cannot remove either.

mod glyphs;

#[cfg(test)]
mod tests;

/// Rows in one glyph cell.
pub(crate) const GLYPH_HEIGHT: usize = 16;

/// Columns in one glyph cell.
pub(crate) const GLYPH_WIDTH: usize = 8;

/// Keep 4K consoles readable when a capture device reduces them to 1080p.
/// Early/panic and normal VT consoles must use the same cell geometry.
pub(crate) const UHD_WIDTH: u32 = 3840;
pub(crate) const UHD_HEIGHT: u32 = 2160;
pub(crate) const UHD_GLYPH_SCALE: usize = 3;

pub(crate) fn glyph_scale(width: usize, height: usize) -> usize {
    if width >= UHD_WIDTH as usize && height >= UHD_HEIGHT as usize {
        UHD_GLYPH_SCALE
    } else {
        1
    }
}

/// The row bitmap for `byte`, or `None` when the font has no glyph for it.
///
/// A caller which gets `None` draws nothing.  The console is handed control
/// bytes, UTF-8 continuation bytes and bytes outside the font's range, and a
/// substitute box for each of them would bury the text around them.
pub(crate) fn glyph(byte: u8) -> Option<&'static [u8; GLYPH_HEIGHT]> {
    let index = usize::from(byte.checked_sub(glyphs::FIRST_BYTE)?);
    glyphs::GLYPHS.get(index)
}
