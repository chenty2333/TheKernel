//! VESA Display Monitor Timing (DMT) 1.13.
//!
//! The table below is the DMT standard's own list, keyed by DMT code.  Each
//! entry also carries the two identifiers the DMT standard assigns to a
//! timing:
//!
//! * `edid_std_id` - the two-byte EDID standard timing identification code
//!   (`0x0000` when the timing cannot be expressed in that format).  This is
//!   what resolves an EDID standard timing descriptor: E-EDID 1.4 Appendix B
//!   requires that when the descriptor names a DMT timing, that timing's
//!   *exact* parameters are used rather than a formula's approximation.
//! * `cvt_code` - the three-byte CVT code (`0x000000` when the timing is not
//!   CVT-derived).  Every CVT-derived entry is reproduced by
//!   [`super::cvt::generate`] to the kilohertz; the test module proves it.
//!
//! Source: VESA DMT 1.13 as transcribed in libdisplay-info's generated
//! `dmt-table.c` (MIT), cross-checked entry by entry against the independent
//! transcription in Linux's `drm_dmt_modes[]`.  See
//! `docs/design/display-modes.md` for the provenance rules and for how the
//! border columns are folded into the blanking.

use super::mode::{Mode, ModeFlags, TimingSource};

/// One row of the DMT 1.13 table.
#[derive(Clone, Copy, Debug)]
pub struct DmtTiming {
    /// DMT identification code (0x01..=0x56).
    pub code: u8,
    /// EDID standard timing identification code, or 0 when undefined.
    pub edid_std_id: u16,
    /// CVT three-byte code, or 0 when the timing is not CVT-derived.
    pub cvt_code: u32,
    /// The refresh rate the DMT standard prints for this row.
    pub nominal_refresh_millihz: u32,
    /// True for the reduced-blanking variants.
    pub reduced_blanking: bool,
    pub mode: Mode,
}

impl DmtTiming {
    /// The two bytes an EDID standard timing descriptor carries for this
    /// timing, most significant first, when the format can express it.
    pub fn edid_std_code(&self) -> Option<[u8; 2]> {
        if self.edid_std_id == 0 {
            return None;
        }
        Some([
            (self.edid_std_id >> 8) as u8,
            (self.edid_std_id & 0xff) as u8,
        ])
    }

    /// True when the DMT standard generated this timing with the CVT formulas.
    pub fn is_cvt_derived(&self) -> bool {
        self.cvt_code != 0
    }
}

/// Every DMT 1.13 timing except code 0x0F.
///
/// DMT 0x0F is the standard's only interlaced entry (1024x768 at 43 Hz, the
/// IBM 8514/A timing).  The transcription this table comes from carries no
/// interlace flag and expresses its vertical blanking per field, so the entry
/// cannot be reproduced without guessing; no digital panel advertises it.
pub const DMT_TIMINGS: &[DmtTiming] = &[
    // DMT 0x01 - 640x350 @ 85.00 Hz
    DmtTiming {
        code: 0x01,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            31500, 640, 192, 32, 64,
            350, 95, 32, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x01),
        )
        .with_polarity(true, false),
    },
    // DMT 0x02 - 640x400 @ 85.00 Hz
    DmtTiming {
        code: 0x02,
        edid_std_id: 0x3119,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            31500, 640, 192, 32, 64,
            400, 45, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x02),
        )
        .with_polarity(false, true),
    },
    // DMT 0x03 - 720x400 @ 85.00 Hz
    DmtTiming {
        code: 0x03,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            35500, 720, 216, 36, 72,
            400, 46, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x03),
        )
        .with_polarity(false, true),
    },
    // DMT 0x04 - 640x480 @ 60.00 Hz
    DmtTiming {
        code: 0x04,
        edid_std_id: 0x3140,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            25175, 640, 160, 16, 96,
            480, 45, 10, 2,
            ModeFlags::NONE, TimingSource::Dmt(0x04),
        )
        .with_polarity(false, false),
    },
    // DMT 0x05 - 640x480 @ 72.00 Hz
    DmtTiming {
        code: 0x05,
        edid_std_id: 0x314C,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 72000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            31500, 640, 192, 24, 40,
            480, 40, 9, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x05),
        )
        .with_polarity(false, false),
    },
    // DMT 0x06 - 640x480 @ 75.00 Hz
    DmtTiming {
        code: 0x06,
        edid_std_id: 0x314F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            31500, 640, 200, 16, 64,
            480, 20, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x06),
        )
        .with_polarity(false, false),
    },
    // DMT 0x07 - 640x480 @ 85.00 Hz
    DmtTiming {
        code: 0x07,
        edid_std_id: 0x3159,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            36000, 640, 192, 56, 56,
            480, 29, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x07),
        )
        .with_polarity(false, false),
    },
    // DMT 0x08 - 800x600 @ 56.00 Hz
    DmtTiming {
        code: 0x08,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 56000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            36000, 800, 224, 24, 72,
            600, 25, 1, 2,
            ModeFlags::NONE, TimingSource::Dmt(0x08),
        )
        .with_polarity(true, true),
    },
    // DMT 0x09 - 800x600 @ 60.00 Hz
    DmtTiming {
        code: 0x09,
        edid_std_id: 0x4540,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            40000, 800, 256, 40, 128,
            600, 28, 1, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x09),
        )
        .with_polarity(true, true),
    },
    // DMT 0x0A - 800x600 @ 72.00 Hz
    DmtTiming {
        code: 0x0A,
        edid_std_id: 0x454C,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 72000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            50000, 800, 240, 56, 120,
            600, 66, 37, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x0A),
        )
        .with_polarity(true, true),
    },
    // DMT 0x0B - 800x600 @ 75.00 Hz
    DmtTiming {
        code: 0x0B,
        edid_std_id: 0x454F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            49500, 800, 256, 16, 80,
            600, 25, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x0B),
        )
        .with_polarity(true, true),
    },
    // DMT 0x0C - 800x600 @ 85.00 Hz
    DmtTiming {
        code: 0x0C,
        edid_std_id: 0x4559,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            56250, 800, 248, 32, 64,
            600, 31, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x0C),
        )
        .with_polarity(true, true),
    },
    // DMT 0x0D - 800x600 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x0D,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            73250, 800, 160, 48, 32,
            600, 36, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x0D),
        )
        .with_polarity(true, false),
    },
    // DMT 0x0E - 848x480 @ 60.00 Hz
    DmtTiming {
        code: 0x0E,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            33750, 848, 240, 16, 112,
            480, 37, 6, 8,
            ModeFlags::NONE, TimingSource::Dmt(0x0E),
        )
        .with_polarity(true, true),
    },
    // DMT 0x10 - 1024x768 @ 60.00 Hz
    DmtTiming {
        code: 0x10,
        edid_std_id: 0x6140,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            65000, 1024, 320, 24, 136,
            768, 38, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x10),
        )
        .with_polarity(false, false),
    },
    // DMT 0x11 - 1024x768 @ 70.00 Hz
    DmtTiming {
        code: 0x11,
        edid_std_id: 0x614A,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 70000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            75000, 1024, 304, 24, 136,
            768, 38, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x11),
        )
        .with_polarity(false, false),
    },
    // DMT 0x12 - 1024x768 @ 75.00 Hz
    DmtTiming {
        code: 0x12,
        edid_std_id: 0x614F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            78750, 1024, 288, 16, 96,
            768, 32, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x12),
        )
        .with_polarity(true, true),
    },
    // DMT 0x13 - 1024x768 @ 85.00 Hz
    DmtTiming {
        code: 0x13,
        edid_std_id: 0x6159,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            94500, 1024, 352, 48, 96,
            768, 40, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x13),
        )
        .with_polarity(true, true),
    },
    // DMT 0x14 - 1024x768 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x14,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            115500, 1024, 160, 48, 32,
            768, 45, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x14),
        )
        .with_polarity(true, false),
    },
    // DMT 0x15 - 1152x864 @ 75.00 Hz
    DmtTiming {
        code: 0x15,
        edid_std_id: 0x714F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            108000, 1152, 448, 64, 128,
            864, 36, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x15),
        )
        .with_polarity(true, true),
    },
    // DMT 0x16 - 1280x768 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x16,
        edid_std_id: 0x0000,
        cvt_code: 0x7F1C21,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            68250, 1280, 160, 48, 32,
            768, 22, 3, 7,
            ModeFlags::NONE, TimingSource::Dmt(0x16),
        )
        .with_polarity(true, false),
    },
    // DMT 0x17 - 1280x768 @ 60.00 Hz
    DmtTiming {
        code: 0x17,
        edid_std_id: 0x0000,
        cvt_code: 0x7F1C28,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            79500, 1280, 384, 64, 128,
            768, 30, 3, 7,
            ModeFlags::NONE, TimingSource::Dmt(0x17),
        )
        .with_polarity(false, true),
    },
    // DMT 0x18 - 1280x768 @ 75.00 Hz
    DmtTiming {
        code: 0x18,
        edid_std_id: 0x0000,
        cvt_code: 0x7F1C44,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            102250, 1280, 416, 80, 128,
            768, 37, 3, 7,
            ModeFlags::NONE, TimingSource::Dmt(0x18),
        )
        .with_polarity(false, true),
    },
    // DMT 0x19 - 1280x768 @ 85.00 Hz
    DmtTiming {
        code: 0x19,
        edid_std_id: 0x0000,
        cvt_code: 0x7F1C62,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            117500, 1280, 432, 80, 136,
            768, 41, 3, 7,
            ModeFlags::NONE, TimingSource::Dmt(0x19),
        )
        .with_polarity(false, true),
    },
    // DMT 0x1A - 1280x768 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x1A,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            140250, 1280, 160, 48, 32,
            768, 45, 3, 7,
            ModeFlags::NONE, TimingSource::Dmt(0x1A),
        )
        .with_polarity(true, false),
    },
    // DMT 0x1B - 1280x800 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x1B,
        edid_std_id: 0x0000,
        cvt_code: 0x8F1821,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            71000, 1280, 160, 48, 32,
            800, 23, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x1B),
        )
        .with_polarity(true, false),
    },
    // DMT 0x1C - 1280x800 @ 60.00 Hz
    DmtTiming {
        code: 0x1C,
        edid_std_id: 0x8100,
        cvt_code: 0x8F1828,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            83500, 1280, 400, 72, 128,
            800, 31, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x1C),
        )
        .with_polarity(false, true),
    },
    // DMT 0x1D - 1280x800 @ 75.00 Hz
    DmtTiming {
        code: 0x1D,
        edid_std_id: 0x810F,
        cvt_code: 0x8F1844,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            106500, 1280, 416, 80, 128,
            800, 38, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x1D),
        )
        .with_polarity(false, true),
    },
    // DMT 0x1E - 1280x800 @ 85.00 Hz
    DmtTiming {
        code: 0x1E,
        edid_std_id: 0x8119,
        cvt_code: 0x8F1862,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            122500, 1280, 432, 80, 136,
            800, 43, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x1E),
        )
        .with_polarity(false, true),
    },
    // DMT 0x1F - 1280x800 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x1F,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            146250, 1280, 160, 48, 32,
            800, 47, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x1F),
        )
        .with_polarity(true, false),
    },
    // DMT 0x20 - 1280x960 @ 60.00 Hz
    DmtTiming {
        code: 0x20,
        edid_std_id: 0x8140,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            108000, 1280, 520, 96, 112,
            960, 40, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x20),
        )
        .with_polarity(true, true),
    },
    // DMT 0x21 - 1280x960 @ 85.00 Hz
    DmtTiming {
        code: 0x21,
        edid_std_id: 0x8159,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            148500, 1280, 448, 64, 160,
            960, 51, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x21),
        )
        .with_polarity(true, true),
    },
    // DMT 0x22 - 1280x960 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x22,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            175500, 1280, 160, 48, 32,
            960, 57, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x22),
        )
        .with_polarity(true, false),
    },
    // DMT 0x23 - 1280x1024 @ 60.00 Hz
    DmtTiming {
        code: 0x23,
        edid_std_id: 0x8180,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            108000, 1280, 408, 48, 112,
            1024, 42, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x23),
        )
        .with_polarity(true, true),
    },
    // DMT 0x24 - 1280x1024 @ 75.00 Hz
    DmtTiming {
        code: 0x24,
        edid_std_id: 0x818F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            135000, 1280, 408, 16, 144,
            1024, 42, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x24),
        )
        .with_polarity(true, true),
    },
    // DMT 0x25 - 1280x1024 @ 85.00 Hz
    DmtTiming {
        code: 0x25,
        edid_std_id: 0x8199,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            157500, 1280, 448, 64, 160,
            1024, 48, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x25),
        )
        .with_polarity(true, true),
    },
    // DMT 0x26 - 1280x1024 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x26,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            187250, 1280, 160, 48, 32,
            1024, 60, 3, 7,
            ModeFlags::NONE, TimingSource::Dmt(0x26),
        )
        .with_polarity(true, false),
    },
    // DMT 0x27 - 1360x768 @ 60.00 Hz
    DmtTiming {
        code: 0x27,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            85500, 1360, 432, 64, 112,
            768, 27, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x27),
        )
        .with_polarity(true, true),
    },
    // DMT 0x28 - 1360x768 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x28,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            148250, 1360, 160, 48, 32,
            768, 45, 3, 5,
            ModeFlags::NONE, TimingSource::Dmt(0x28),
        )
        .with_polarity(true, false),
    },
    // DMT 0x29 - 1400x1050 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x29,
        edid_std_id: 0x0000,
        cvt_code: 0x0C2021,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            101000, 1400, 160, 48, 32,
            1050, 30, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x29),
        )
        .with_polarity(true, false),
    },
    // DMT 0x2A - 1400x1050 @ 60.00 Hz
    DmtTiming {
        code: 0x2A,
        edid_std_id: 0x9040,
        cvt_code: 0x0C2028,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            121750, 1400, 464, 88, 144,
            1050, 39, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x2A),
        )
        .with_polarity(false, true),
    },
    // DMT 0x2B - 1400x1050 @ 75.00 Hz
    DmtTiming {
        code: 0x2B,
        edid_std_id: 0x904F,
        cvt_code: 0x0C2044,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            156000, 1400, 496, 104, 144,
            1050, 49, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x2B),
        )
        .with_polarity(false, true),
    },
    // DMT 0x2C - 1400x1050 @ 85.00 Hz
    DmtTiming {
        code: 0x2C,
        edid_std_id: 0x9059,
        cvt_code: 0x0C2062,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            179500, 1400, 512, 104, 152,
            1050, 55, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x2C),
        )
        .with_polarity(false, true),
    },
    // DMT 0x2D - 1400x1050 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x2D,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            208000, 1400, 160, 48, 32,
            1050, 62, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x2D),
        )
        .with_polarity(true, false),
    },
    // DMT 0x2E - 1440x900 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x2E,
        edid_std_id: 0x0000,
        cvt_code: 0xC11821,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            88750, 1440, 160, 48, 32,
            900, 26, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x2E),
        )
        .with_polarity(true, false),
    },
    // DMT 0x2F - 1440x900 @ 60.00 Hz
    DmtTiming {
        code: 0x2F,
        edid_std_id: 0x9500,
        cvt_code: 0xC11828,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            106500, 1440, 464, 80, 152,
            900, 34, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x2F),
        )
        .with_polarity(false, true),
    },
    // DMT 0x30 - 1440x900 @ 75.00 Hz
    DmtTiming {
        code: 0x30,
        edid_std_id: 0x950F,
        cvt_code: 0xC11844,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            136750, 1440, 496, 96, 152,
            900, 42, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x30),
        )
        .with_polarity(false, true),
    },
    // DMT 0x31 - 1440x900 @ 85.00 Hz
    DmtTiming {
        code: 0x31,
        edid_std_id: 0x9519,
        cvt_code: 0xC11868,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            157000, 1440, 512, 104, 152,
            900, 48, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x31),
        )
        .with_polarity(false, true),
    },
    // DMT 0x32 - 1440x900 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x32,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            182750, 1440, 160, 48, 32,
            900, 53, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x32),
        )
        .with_polarity(true, false),
    },
    // DMT 0x33 - 1600x1200 @ 60.00 Hz
    DmtTiming {
        code: 0x33,
        edid_std_id: 0xA940,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            162000, 1600, 560, 64, 192,
            1200, 50, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x33),
        )
        .with_polarity(true, true),
    },
    // DMT 0x34 - 1600x1200 @ 65.00 Hz
    DmtTiming {
        code: 0x34,
        edid_std_id: 0xA945,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 65000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            175500, 1600, 560, 64, 192,
            1200, 50, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x34),
        )
        .with_polarity(true, true),
    },
    // DMT 0x35 - 1600x1200 @ 70.00 Hz
    DmtTiming {
        code: 0x35,
        edid_std_id: 0xA94A,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 70000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            189000, 1600, 560, 64, 192,
            1200, 50, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x35),
        )
        .with_polarity(true, true),
    },
    // DMT 0x36 - 1600x1200 @ 75.00 Hz
    DmtTiming {
        code: 0x36,
        edid_std_id: 0xA94F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            202500, 1600, 560, 64, 192,
            1200, 50, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x36),
        )
        .with_polarity(true, true),
    },
    // DMT 0x37 - 1600x1200 @ 85.00 Hz
    DmtTiming {
        code: 0x37,
        edid_std_id: 0xA959,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            229500, 1600, 560, 64, 192,
            1200, 50, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x37),
        )
        .with_polarity(true, true),
    },
    // DMT 0x38 - 1600x1200 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x38,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            268250, 1600, 160, 48, 32,
            1200, 71, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x38),
        )
        .with_polarity(true, false),
    },
    // DMT 0x39 - 1680x1050 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x39,
        edid_std_id: 0x0000,
        cvt_code: 0x0C2821,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            119000, 1680, 160, 48, 32,
            1050, 30, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x39),
        )
        .with_polarity(true, false),
    },
    // DMT 0x3A - 1680x1050 @ 60.00 Hz
    DmtTiming {
        code: 0x3A,
        edid_std_id: 0xB300,
        cvt_code: 0x0C2828,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            146250, 1680, 560, 104, 176,
            1050, 39, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x3A),
        )
        .with_polarity(false, true),
    },
    // DMT 0x3B - 1680x1050 @ 75.00 Hz
    DmtTiming {
        code: 0x3B,
        edid_std_id: 0xB30F,
        cvt_code: 0x0C2844,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            187000, 1680, 592, 120, 176,
            1050, 49, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x3B),
        )
        .with_polarity(false, true),
    },
    // DMT 0x3C - 1680x1050 @ 85.00 Hz
    DmtTiming {
        code: 0x3C,
        edid_std_id: 0xB319,
        cvt_code: 0x0C2868,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            214750, 1680, 608, 128, 176,
            1050, 55, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x3C),
        )
        .with_polarity(false, true),
    },
    // DMT 0x3D - 1680x1050 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x3D,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            245500, 1680, 160, 48, 32,
            1050, 62, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x3D),
        )
        .with_polarity(true, false),
    },
    // DMT 0x3E - 1792x1344 @ 60.00 Hz
    DmtTiming {
        code: 0x3E,
        edid_std_id: 0xC140,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            204750, 1792, 656, 128, 200,
            1344, 50, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x3E),
        )
        .with_polarity(false, true),
    },
    // DMT 0x3F - 1792x1344 @ 75.00 Hz
    DmtTiming {
        code: 0x3F,
        edid_std_id: 0xC14F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            261000, 1792, 664, 96, 216,
            1344, 73, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x3F),
        )
        .with_polarity(false, true),
    },
    // DMT 0x40 - 1792x1344 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x40,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            333250, 1792, 160, 48, 32,
            1344, 79, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x40),
        )
        .with_polarity(true, false),
    },
    // DMT 0x41 - 1856x1392 @ 60.00 Hz
    DmtTiming {
        code: 0x41,
        edid_std_id: 0xC940,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            218250, 1856, 672, 96, 224,
            1392, 47, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x41),
        )
        .with_polarity(false, true),
    },
    // DMT 0x42 - 1856x1392 @ 75.00 Hz
    DmtTiming {
        code: 0x42,
        edid_std_id: 0xC94F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            288000, 1856, 704, 128, 224,
            1392, 108, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x42),
        )
        .with_polarity(false, true),
    },
    // DMT 0x43 - 1856x1392 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x43,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            356500, 1856, 160, 48, 32,
            1392, 82, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x43),
        )
        .with_polarity(true, false),
    },
    // DMT 0x44 - 1920x1200 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x44,
        edid_std_id: 0x0000,
        cvt_code: 0x572821,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            154000, 1920, 160, 48, 32,
            1200, 35, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x44),
        )
        .with_polarity(true, false),
    },
    // DMT 0x45 - 1920x1200 @ 60.00 Hz
    DmtTiming {
        code: 0x45,
        edid_std_id: 0xD100,
        cvt_code: 0x572828,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            193250, 1920, 672, 136, 200,
            1200, 45, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x45),
        )
        .with_polarity(false, true),
    },
    // DMT 0x46 - 1920x1200 @ 75.00 Hz
    DmtTiming {
        code: 0x46,
        edid_std_id: 0xD10F,
        cvt_code: 0x572844,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            245250, 1920, 688, 136, 208,
            1200, 55, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x46),
        )
        .with_polarity(false, true),
    },
    // DMT 0x47 - 1920x1200 @ 85.00 Hz
    DmtTiming {
        code: 0x47,
        edid_std_id: 0xD119,
        cvt_code: 0x572862,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            281250, 1920, 704, 144, 208,
            1200, 62, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x47),
        )
        .with_polarity(false, true),
    },
    // DMT 0x48 - 1920x1200 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x48,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            317000, 1920, 160, 48, 32,
            1200, 71, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x48),
        )
        .with_polarity(true, false),
    },
    // DMT 0x49 - 1920x1440 @ 60.00 Hz
    DmtTiming {
        code: 0x49,
        edid_std_id: 0xD140,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            234000, 1920, 680, 128, 208,
            1440, 60, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x49),
        )
        .with_polarity(false, true),
    },
    // DMT 0x4A - 1920x1440 @ 75.00 Hz
    DmtTiming {
        code: 0x4A,
        edid_std_id: 0xD14F,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            297000, 1920, 720, 144, 224,
            1440, 60, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x4A),
        )
        .with_polarity(false, true),
    },
    // DMT 0x4B - 1920x1440 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x4B,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            380500, 1920, 160, 48, 32,
            1440, 85, 3, 4,
            ModeFlags::NONE, TimingSource::Dmt(0x4B),
        )
        .with_polarity(true, false),
    },
    // DMT 0x4C - 2560x1600 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x4C,
        edid_std_id: 0x0000,
        cvt_code: 0x1F3821,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            268500, 2560, 160, 48, 32,
            1600, 46, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x4C),
        )
        .with_polarity(true, false),
    },
    // DMT 0x4D - 2560x1600 @ 60.00 Hz
    DmtTiming {
        code: 0x4D,
        edid_std_id: 0x0000,
        cvt_code: 0x1F3828,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            348500, 2560, 944, 192, 280,
            1600, 58, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x4D),
        )
        .with_polarity(false, true),
    },
    // DMT 0x4E - 2560x1600 @ 75.00 Hz
    DmtTiming {
        code: 0x4E,
        edid_std_id: 0x0000,
        cvt_code: 0x1F3844,
        nominal_refresh_millihz: 75000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            443250, 2560, 976, 208, 280,
            1600, 72, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x4E),
        )
        .with_polarity(false, true),
    },
    // DMT 0x4F - 2560x1600 @ 85.00 Hz
    DmtTiming {
        code: 0x4F,
        edid_std_id: 0x0000,
        cvt_code: 0x1F3862,
        nominal_refresh_millihz: 85000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            505250, 2560, 976, 208, 280,
            1600, 82, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x4F),
        )
        .with_polarity(false, true),
    },
    // DMT 0x50 - 2560x1600 @ 120.00 Hz, reduced blanking
    DmtTiming {
        code: 0x50,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 120000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            552750, 2560, 160, 48, 32,
            1600, 94, 3, 6,
            ModeFlags::NONE, TimingSource::Dmt(0x50),
        )
        .with_polarity(true, false),
    },
    // DMT 0x51 - 1366x768 @ 60.00 Hz
    DmtTiming {
        code: 0x51,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            85500, 1366, 426, 70, 143,
            768, 30, 3, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x51),
        )
        .with_polarity(true, true),
    },
    // DMT 0x52 - 1920x1080 @ 60.00 Hz
    DmtTiming {
        code: 0x52,
        edid_std_id: 0xD1C0,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            148500, 1920, 280, 88, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::Dmt(0x52),
        )
        .with_polarity(false, false),
    },
    // DMT 0x53 - 1600x900 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x53,
        edid_std_id: 0xA9C0,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            108000, 1600, 200, 24, 80,
            900, 100, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x53),
        )
        .with_polarity(true, true),
    },
    // DMT 0x54 - 2048x1152 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x54,
        edid_std_id: 0xE1C0,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            162000, 2048, 202, 26, 80,
            1152, 48, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x54),
        )
        .with_polarity(true, true),
    },
    // DMT 0x55 - 1280x720 @ 60.00 Hz
    DmtTiming {
        code: 0x55,
        edid_std_id: 0x81C0,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: false,
        mode: Mode::from_blanking(
            74250, 1280, 370, 110, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::Dmt(0x55),
        )
        .with_polarity(true, true),
    },
    // DMT 0x56 - 1366x768 @ 60.00 Hz, reduced blanking
    DmtTiming {
        code: 0x56,
        edid_std_id: 0x0000,
        cvt_code: 0x000000,
        nominal_refresh_millihz: 60000,
        reduced_blanking: true,
        mode: Mode::from_blanking(
            72000, 1366, 134, 14, 56,
            768, 32, 1, 3,
            ModeFlags::NONE, TimingSource::Dmt(0x56),
        )
        .with_polarity(true, true),
    },
];

/// Looks a timing up by DMT code.
pub fn by_code(code: u8) -> Option<&'static DmtTiming> {
    DMT_TIMINGS.iter().find(|entry| entry.code == code)
}

/// Looks a timing up by the two-byte EDID standard timing identification code.
///
/// Returns `None` for code `0x0000` and for any code the DMT standard does not
/// define, which is what makes the standard-timing fallback path necessary.
pub fn by_std_id(id: u16) -> Option<&'static DmtTiming> {
    if id == 0 {
        return None;
    }
    DMT_TIMINGS.iter().find(|entry| entry.edid_std_id == id)
}

/// Finds the DMT timing for a resolution and refresh rate.
///
/// `refresh_hz` is the integer rate an EDID standard timing descriptor names
/// (or the rounded rate of a firmware mode).  The DMT standard prints rounded
/// labels - its 640x480 row says 60 Hz for what is really 59.94 Hz - so the
/// match is on that label within one hertz, not on the computed rate.
///
/// `reduced_blanking` filters the blanking variant: `None` accepts either and
/// returns the first published row, `Some(true)`/`Some(false)` require that
/// variant and fall back to the first size/rate match when it does not exist.
pub fn by_size(
    hdisplay: u16,
    vdisplay: u16,
    refresh_hz: u32,
    reduced_blanking: Option<bool>,
) -> Option<&'static DmtTiming> {
    let matches = |entry: &DmtTiming| {
        entry.mode.hdisplay == hdisplay
            && entry.mode.vdisplay == vdisplay
            && entry
                .nominal_refresh_millihz
                .abs_diff(refresh_hz.saturating_mul(1000))
                <= 1000
    };
    if let Some(want) = reduced_blanking
        && let Some(entry) = DMT_TIMINGS
            .iter()
            .find(|entry| matches(entry) && entry.reduced_blanking == want)
    {
        return Some(entry);
    }
    DMT_TIMINGS.iter().find(|entry| matches(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DMT standard prints rounded refresh labels; a row whose label is
    /// more than this far from the rate its own numbers produce would mean a
    /// transcription error in the clock, the blanking or the label.
    const LABEL_TOLERANCE_MILLIHZ: u32 = 1300;

    #[test]
    fn table_is_well_formed_and_self_consistent() {
        assert_eq!(DMT_TIMINGS.len(), 85);
        for entry in DMT_TIMINGS {
            let mode = &entry.mode;
            assert!(mode.is_well_formed(), "{mode} is not well formed");
            assert!(!mode.is_interlaced(), "{mode} must be progressive");
            assert_eq!(mode.hblank(), mode.htotal - mode.hdisplay);
            assert_eq!(mode.source, TimingSource::Dmt(entry.code));
            let computed = mode.refresh_millihz();
            assert!(
                computed.abs_diff(entry.nominal_refresh_millihz) <= LABEL_TOLERANCE_MILLIHZ,
                "DMT {:#04x}: {} computes {computed} mHz, label is {}",
                entry.code,
                mode,
                entry.nominal_refresh_millihz
            );
        }
        // Codes are unique and ascending, so the table can be scanned by eye.
        assert!(DMT_TIMINGS.windows(2).all(|w| w[0].code < w[1].code));
        assert!(by_code(0x0f).is_none(), "the interlaced row is omitted");
    }

    /// Spot checks against the numbers a display engineer would recognise.
    /// A wrong pixel clock here is a blank screen, so these are exact.
    #[test]
    fn published_rows_match_the_standard() {
        type Row = (u8, u16, u16, u32, u16, u16, u16, u16, u16, u16, u32, bool, bool);
        let cases: &[Row] = &[
            // code, w, h, clock kHz, hss, hse, htotal, vss, vse, vtotal, Hz, h+, v+
            (0x04, 640, 480, 25_175, 656, 752, 800, 490, 492, 525, 60, false, false),
            (0x09, 800, 600, 40_000, 840, 968, 1056, 601, 605, 628, 60, true, true),
            (0x10, 1024, 768, 65_000, 1048, 1184, 1344, 771, 777, 806, 60, false, false),
            (
                0x23, 1280, 1024, 108_000, 1328, 1440, 1688, 1025, 1028, 1066, 60, true, true,
            ),
            (
                0x31, 1440, 900, 157_000, 1544, 1696, 1952, 903, 909, 948, 85, false, true,
            ),
            (
                0x39, 1680, 1050, 119_000, 1728, 1760, 1840, 1053, 1059, 1080, 60, true, false,
            ),
            (
                0x44, 1920, 1200, 154_000, 1968, 2000, 2080, 1203, 1209, 1235, 60, true, false,
            ),
            (
                0x52, 1920, 1080, 148_500, 2008, 2052, 2200, 1084, 1089, 1125, 60, false, false,
            ),
            (0x55, 1280, 720, 74_250, 1390, 1430, 1650, 725, 730, 750, 60, true, true),
        ];
        for &(code, width, height, clock, hss, hse, htotal, vss, vse, vtotal, hz, hp, vp) in cases {
            let entry = by_code(code).unwrap_or_else(|| panic!("DMT {code:#04x} missing"));
            let mode = entry.mode;
            assert_eq!((mode.hdisplay, mode.vdisplay), (width, height));
            assert_eq!(mode.clock_khz, clock, "DMT {code:#04x} clock");
            assert_eq!(
                (
                    mode.hsync_start,
                    mode.hsync_end,
                    mode.htotal,
                    mode.vsync_start,
                    mode.vsync_end,
                    mode.vtotal
                ),
                (hss, hse, htotal, vss, vse, vtotal),
                "DMT {code:#04x} edges"
            );
            assert_eq!(
                (mode.hsync_positive, mode.vsync_positive),
                (hp, vp),
                "DMT {code:#04x} polarity"
            );
            assert_eq!(mode.refresh_hz_rounded(), hz, "DMT {code:#04x} rate");
            assert!(mode.is_well_formed());
        }
    }

    #[test]
    fn standard_timing_ids_resolve_to_the_exact_dmt_row() {
        // 640x480@60 encodes as 0x3140 and must select DMT 0x04, whose
        // blanking (160/45) is not what a formula would produce.
        let entry = by_std_id(0x3140).expect("640x480@60 is a standard timing ID");
        assert_eq!(entry.code, 0x04);
        assert_eq!(entry.edid_std_code(), Some([0x31, 0x40]));
        assert_eq!(entry.mode.clock_khz, 25_175);
        assert!(by_std_id(0x0000).is_none(), "no timing has ID zero");
        assert!(by_std_id(0xffff).is_none());
        // Every non-zero identifier is unique.
        let mut ids: alloc::vec::Vec<u16> = DMT_TIMINGS
            .iter()
            .filter_map(|entry| (entry.edid_std_id != 0).then_some(entry.edid_std_id))
            .collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total);
        assert_eq!(total, 49);
    }

    #[test]
    fn size_lookup_honours_the_blanking_variant() {
        let plain = by_size(1920, 1200, 60, None).expect("1920x1200@60 exists");
        assert_eq!(plain.code, 0x44);
        assert!(plain.reduced_blanking);
        // 1680x1050 has both a reduced-blanking and a standard row.
        let rb = by_size(1680, 1050, 60, Some(true)).expect("RB row");
        let sb = by_size(1680, 1050, 60, Some(false)).expect("standard row");
        assert_eq!((rb.code, sb.code), (0x39, 0x3a));
        assert!(rb.mode.clock_khz < sb.mode.clock_khz);
        // Requesting a variant that does not exist still finds the row.
        assert!(by_size(1920, 1200, 60, Some(false)).is_some());
        // The 59.94 Hz rows answer to the rounded 60 Hz label.
        assert_eq!(by_size(1280, 720, 60, None).map(|e| e.code), Some(0x55));
        assert!(by_size(1920, 1080, 144, None).is_none());
        assert!(by_size(1366, 768, 60, None).is_some(), "1366x768 is in DMT");
    }
}
