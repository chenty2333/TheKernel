//! Invariants the console relies on the glyph table to hold.
//!
//! These live beside the generated table rather than inside it, so that
//! regenerating `glyphs.rs` cannot delete them.

use super::{
    GLYPH_HEIGHT, GLYPH_WIDTH, glyph,
    glyphs::{FIRST_BYTE, GLYPHS, LAST_BYTE},
};

/// Every byte the console can be handed, so a coverage test cannot pass by
/// only looking at the bytes the font happens to define.
const ALL_BYTES: core::ops::RangeInclusive<u8> = 0..=u8::MAX;

#[test]
fn the_glyph_table_covers_exactly_the_printable_range() {
    assert_eq!(GLYPHS.len(), (LAST_BYTE - FIRST_BYTE + 1) as usize);
    for byte in ALL_BYTES {
        let expected = (FIRST_BYTE..=LAST_BYTE).contains(&byte);
        assert_eq!(glyph(byte).is_some(), expected, "byte {byte:#04x}");
    }
}

#[test]
fn every_printable_glyph_except_space_has_ink() {
    // A blank glyph is indistinguishable from a font that failed to generate,
    // and on a machine whose only console is the screen it would silently
    // erase characters from the kernel log.
    for byte in FIRST_BYTE..=LAST_BYTE {
        let rows = glyph(byte).expect("covered by the previous test");
        let ink = rows.iter().map(|row| row.count_ones()).sum::<u32>();
        if byte == b' ' {
            assert_eq!(ink, 0, "space must be blank");
        } else {
            assert!(ink > 0, "glyph {byte:#04x} ({:?}) is blank", byte as char);
        }
    }
}

#[test]
fn no_glyph_touches_the_cell_border() {
    // Ink in the rightmost column would make adjacent characters run together
    // into a line that cannot be read, and ink in the first or last row would
    // be clipped by the cell the renderer draws into.  Bit 0 is the rightmost
    // of the columns, because the renderer takes bit 7 as the leftmost.
    assert_eq!(GLYPH_WIDTH, 8);
    for byte in FIRST_BYTE..=LAST_BYTE {
        let rows = glyph(byte).expect("covered by the first test");
        assert_eq!(rows[0], 0, "glyph {byte:#04x} inks the top row");
        assert_eq!(
            rows[GLYPH_HEIGHT - 1],
            0,
            "glyph {byte:#04x} inks the bottom row"
        );
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(
                row & 1,
                0,
                "glyph {byte:#04x} inks the right column at row {index}"
            );
        }
    }
}

#[test]
fn lowercase_is_not_the_uppercase_glyph() {
    // The table this replaced folded every byte through
    // `to_ascii_uppercase`, which is what made a boot log unreadable.
    for byte in b'a'..=b'z' {
        assert_ne!(
            glyph(byte),
            glyph(byte.to_ascii_uppercase()),
            "{:?} must not render as {:?}",
            byte as char,
            byte.to_ascii_uppercase() as char
        );
    }
}
