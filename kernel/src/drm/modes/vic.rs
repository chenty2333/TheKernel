//! CTA-861 video identification codes (VIC).
//!
//! An HDMI sink names the formats it accepts with short video descriptors,
//! each carrying a 7-bit VIC.  The timing that a VIC stands for is fixed by
//! the CTA-861 standard, so it is reproduced here as a table rather than
//! computed.
//!
//! Only VIC 1..=127 are listed, because that is exactly what a short video
//! descriptor can carry.  CTA-861-H defines higher codes for the "extended"
//! ranges, which reach a sink only through an HDMI Forum vendor block; see
//! `docs/design/display-modes.md` for why they are out of scope.
//!
//! Source: ANSI/CTA-861-I (with errata) as transcribed in libdisplay-info's
//! generated `cta-vic-table.c` (MIT), cross-checked entry by entry against the
//! independent transcription in Linux's `edid_cea_modes_1[]`.  The kernel's
//! table stores pixel-repeated formats at half the clock with its `DBLCLK`
//! flag, so the cross-check halves the clock and horizontal edges of our rows
//! before comparing them.

use super::mode::{Mode, ModeFlags, TimingSource};

/// One CTA-861 video identification code.
#[derive(Clone, Copy, Debug)]
pub struct VicTiming {
    /// The video identification code, 1..=127.
    pub vic: u8,
    /// The rate the format list names for this code, in millihertz.  CTA-861
    /// names the NTSC family as 59.94/60 and the pixel-repeated SD formats as
    /// 200 or 240 Hz; the value is what the timing computes.
    pub nominal_refresh_millihz: u32,
    pub mode: Mode,
}

/// Every CTA-861 VIC a short video descriptor can name.
pub const CTA_VIC_TIMINGS: &[VicTiming] = &[
    // VIC 1 - 640x480p @ 59.94 Hz
    VicTiming {
        vic: 1,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_blanking(
            25175, 640, 160, 16, 96,
            480, 45, 10, 2,
            ModeFlags::NONE, TimingSource::CtaVic(1),
        )
        .with_polarity(false, false),
    },
    // VIC 2 - 720x480p @ 59.94 Hz
    VicTiming {
        vic: 2,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_blanking(
            27000, 720, 138, 16, 62,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(2),
        )
        .with_polarity(false, false),
    },
    // VIC 3 - 720x480p @ 59.94 Hz
    VicTiming {
        vic: 3,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_blanking(
            27000, 720, 138, 16, 62,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(3),
        )
        .with_polarity(false, false),
    },
    // VIC 4 - 1280x720p @ 60.00 Hz
    VicTiming {
        vic: 4,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            74250, 1280, 370, 110, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(4),
        )
        .with_polarity(true, true),
    },
    // VIC 5 - 1920x1080i @ 60.00 Hz
    VicTiming {
        vic: 5,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_fields(
            74250, 1920, 2008, 2052, 2200,
            1080, 1084, 1094, 1125,
            ModeFlags::INTERLACE, TimingSource::CtaVic(5),
        )
        .with_polarity(true, true),
    },
    // VIC 6 - 1440x480i @ 59.94 Hz
    VicTiming {
        vic: 6,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_fields(
            27000, 1440, 1478, 1602, 1716,
            480, 488, 494, 525,
            ModeFlags::INTERLACE, TimingSource::CtaVic(6),
        )
        .with_polarity(false, false),
    },
    // VIC 7 - 1440x480i @ 59.94 Hz
    VicTiming {
        vic: 7,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_fields(
            27000, 1440, 1478, 1602, 1716,
            480, 488, 494, 525,
            ModeFlags::INTERLACE, TimingSource::CtaVic(7),
        )
        .with_polarity(false, false),
    },
    // VIC 8 - 1440x240p @ 60.05 Hz
    VicTiming {
        vic: 8,
        nominal_refresh_millihz: 60054,
        mode: Mode::from_blanking(
            27000, 1440, 276, 38, 124,
            240, 22, 4, 3,
            ModeFlags::NONE, TimingSource::CtaVic(8),
        )
        .with_polarity(false, false),
    },
    // VIC 9 - 1440x240p @ 60.05 Hz
    VicTiming {
        vic: 9,
        nominal_refresh_millihz: 60054,
        mode: Mode::from_blanking(
            27000, 1440, 276, 38, 124,
            240, 22, 4, 3,
            ModeFlags::NONE, TimingSource::CtaVic(9),
        )
        .with_polarity(false, false),
    },
    // VIC 10 - 2880x480i @ 59.94 Hz
    VicTiming {
        vic: 10,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_fields(
            54000, 2880, 2956, 3204, 3432,
            480, 488, 494, 525,
            ModeFlags::INTERLACE, TimingSource::CtaVic(10),
        )
        .with_polarity(false, false),
    },
    // VIC 11 - 2880x480i @ 59.94 Hz
    VicTiming {
        vic: 11,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_fields(
            54000, 2880, 2956, 3204, 3432,
            480, 488, 494, 525,
            ModeFlags::INTERLACE, TimingSource::CtaVic(11),
        )
        .with_polarity(false, false),
    },
    // VIC 12 - 2880x240p @ 60.05 Hz
    VicTiming {
        vic: 12,
        nominal_refresh_millihz: 60054,
        mode: Mode::from_blanking(
            54000, 2880, 552, 76, 248,
            240, 22, 4, 3,
            ModeFlags::NONE, TimingSource::CtaVic(12),
        )
        .with_polarity(false, false),
    },
    // VIC 13 - 2880x240p @ 60.05 Hz
    VicTiming {
        vic: 13,
        nominal_refresh_millihz: 60054,
        mode: Mode::from_blanking(
            54000, 2880, 552, 76, 248,
            240, 22, 4, 3,
            ModeFlags::NONE, TimingSource::CtaVic(13),
        )
        .with_polarity(false, false),
    },
    // VIC 14 - 1440x480p @ 59.94 Hz
    VicTiming {
        vic: 14,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_blanking(
            54000, 1440, 276, 32, 124,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(14),
        )
        .with_polarity(false, false),
    },
    // VIC 15 - 1440x480p @ 59.94 Hz
    VicTiming {
        vic: 15,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_blanking(
            54000, 1440, 276, 32, 124,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(15),
        )
        .with_polarity(false, false),
    },
    // VIC 16 - 1920x1080p @ 60.00 Hz
    VicTiming {
        vic: 16,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            148500, 1920, 280, 88, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(16),
        )
        .with_polarity(true, true),
    },
    // VIC 17 - 720x576p @ 50.00 Hz
    VicTiming {
        vic: 17,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            27000, 720, 144, 12, 64,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(17),
        )
        .with_polarity(false, false),
    },
    // VIC 18 - 720x576p @ 50.00 Hz
    VicTiming {
        vic: 18,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            27000, 720, 144, 12, 64,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(18),
        )
        .with_polarity(false, false),
    },
    // VIC 19 - 1280x720p @ 50.00 Hz
    VicTiming {
        vic: 19,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            74250, 1280, 700, 440, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(19),
        )
        .with_polarity(true, true),
    },
    // VIC 20 - 1920x1080i @ 50.00 Hz
    VicTiming {
        vic: 20,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_fields(
            74250, 1920, 2448, 2492, 2640,
            1080, 1084, 1094, 1125,
            ModeFlags::INTERLACE, TimingSource::CtaVic(20),
        )
        .with_polarity(true, true),
    },
    // VIC 21 - 1440x576i @ 50.00 Hz
    VicTiming {
        vic: 21,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_fields(
            27000, 1440, 1464, 1590, 1728,
            576, 580, 586, 625,
            ModeFlags::INTERLACE, TimingSource::CtaVic(21),
        )
        .with_polarity(false, false),
    },
    // VIC 22 - 1440x576i @ 50.00 Hz
    VicTiming {
        vic: 22,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_fields(
            27000, 1440, 1464, 1590, 1728,
            576, 580, 586, 625,
            ModeFlags::INTERLACE, TimingSource::CtaVic(22),
        )
        .with_polarity(false, false),
    },
    // VIC 23 - 1440x288p @ 50.08 Hz
    VicTiming {
        vic: 23,
        nominal_refresh_millihz: 50080,
        mode: Mode::from_blanking(
            27000, 1440, 288, 24, 126,
            288, 24, 2, 3,
            ModeFlags::NONE, TimingSource::CtaVic(23),
        )
        .with_polarity(false, false),
    },
    // VIC 24 - 1440x288p @ 50.08 Hz
    VicTiming {
        vic: 24,
        nominal_refresh_millihz: 50080,
        mode: Mode::from_blanking(
            27000, 1440, 288, 24, 126,
            288, 24, 2, 3,
            ModeFlags::NONE, TimingSource::CtaVic(24),
        )
        .with_polarity(false, false),
    },
    // VIC 25 - 2880x576i @ 50.00 Hz
    VicTiming {
        vic: 25,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_fields(
            54000, 2880, 2928, 3180, 3456,
            576, 580, 586, 625,
            ModeFlags::INTERLACE, TimingSource::CtaVic(25),
        )
        .with_polarity(false, false),
    },
    // VIC 26 - 2880x576i @ 50.00 Hz
    VicTiming {
        vic: 26,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_fields(
            54000, 2880, 2928, 3180, 3456,
            576, 580, 586, 625,
            ModeFlags::INTERLACE, TimingSource::CtaVic(26),
        )
        .with_polarity(false, false),
    },
    // VIC 27 - 2880x288p @ 50.08 Hz
    VicTiming {
        vic: 27,
        nominal_refresh_millihz: 50080,
        mode: Mode::from_blanking(
            54000, 2880, 576, 48, 252,
            288, 24, 2, 3,
            ModeFlags::NONE, TimingSource::CtaVic(27),
        )
        .with_polarity(false, false),
    },
    // VIC 28 - 2880x288p @ 50.08 Hz
    VicTiming {
        vic: 28,
        nominal_refresh_millihz: 50080,
        mode: Mode::from_blanking(
            54000, 2880, 576, 48, 252,
            288, 24, 2, 3,
            ModeFlags::NONE, TimingSource::CtaVic(28),
        )
        .with_polarity(false, false),
    },
    // VIC 29 - 1440x576p @ 50.00 Hz
    VicTiming {
        vic: 29,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            54000, 1440, 288, 24, 128,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(29),
        )
        .with_polarity(false, false),
    },
    // VIC 30 - 1440x576p @ 50.00 Hz
    VicTiming {
        vic: 30,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            54000, 1440, 288, 24, 128,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(30),
        )
        .with_polarity(false, false),
    },
    // VIC 31 - 1920x1080p @ 50.00 Hz
    VicTiming {
        vic: 31,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            148500, 1920, 720, 528, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(31),
        )
        .with_polarity(true, true),
    },
    // VIC 32 - 1920x1080p @ 24.00 Hz
    VicTiming {
        vic: 32,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            74250, 1920, 830, 638, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(32),
        )
        .with_polarity(true, true),
    },
    // VIC 33 - 1920x1080p @ 25.00 Hz
    VicTiming {
        vic: 33,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            74250, 1920, 720, 528, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(33),
        )
        .with_polarity(true, true),
    },
    // VIC 34 - 1920x1080p @ 30.00 Hz
    VicTiming {
        vic: 34,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            74250, 1920, 280, 88, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(34),
        )
        .with_polarity(true, true),
    },
    // VIC 35 - 2880x480p @ 59.94 Hz
    VicTiming {
        vic: 35,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_blanking(
            108000, 2880, 552, 64, 248,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(35),
        )
        .with_polarity(false, false),
    },
    // VIC 36 - 2880x480p @ 59.94 Hz
    VicTiming {
        vic: 36,
        nominal_refresh_millihz: 59940,
        mode: Mode::from_blanking(
            108000, 2880, 552, 64, 248,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(36),
        )
        .with_polarity(false, false),
    },
    // VIC 37 - 2880x576p @ 50.00 Hz
    VicTiming {
        vic: 37,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            108000, 2880, 576, 48, 256,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(37),
        )
        .with_polarity(false, false),
    },
    // VIC 38 - 2880x576p @ 50.00 Hz
    VicTiming {
        vic: 38,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            108000, 2880, 576, 48, 256,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(38),
        )
        .with_polarity(false, false),
    },
    // VIC 39 - 1920x1080i @ 50.00 Hz
    VicTiming {
        vic: 39,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_fields(
            72000, 1920, 1952, 2120, 2304,
            1080, 1126, 1136, 1250,
            ModeFlags::INTERLACE, TimingSource::CtaVic(39),
        )
        .with_polarity(true, false),
    },
    // VIC 40 - 1920x1080i @ 100.00 Hz
    VicTiming {
        vic: 40,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_fields(
            148500, 1920, 2448, 2492, 2640,
            1080, 1084, 1094, 1125,
            ModeFlags::INTERLACE, TimingSource::CtaVic(40),
        )
        .with_polarity(true, true),
    },
    // VIC 41 - 1280x720p @ 100.00 Hz
    VicTiming {
        vic: 41,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            148500, 1280, 700, 440, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(41),
        )
        .with_polarity(true, true),
    },
    // VIC 42 - 720x576p @ 100.00 Hz
    VicTiming {
        vic: 42,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            54000, 720, 144, 12, 64,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(42),
        )
        .with_polarity(false, false),
    },
    // VIC 43 - 720x576p @ 100.00 Hz
    VicTiming {
        vic: 43,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            54000, 720, 144, 12, 64,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(43),
        )
        .with_polarity(false, false),
    },
    // VIC 44 - 1440x576i @ 100.00 Hz
    VicTiming {
        vic: 44,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_fields(
            54000, 1440, 1464, 1590, 1728,
            576, 580, 586, 625,
            ModeFlags::INTERLACE, TimingSource::CtaVic(44),
        )
        .with_polarity(false, false),
    },
    // VIC 45 - 1440x576i @ 100.00 Hz
    VicTiming {
        vic: 45,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_fields(
            54000, 1440, 1464, 1590, 1728,
            576, 580, 586, 625,
            ModeFlags::INTERLACE, TimingSource::CtaVic(45),
        )
        .with_polarity(false, false),
    },
    // VIC 46 - 1920x1080i @ 120.00 Hz
    VicTiming {
        vic: 46,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_fields(
            148500, 1920, 2008, 2052, 2200,
            1080, 1084, 1094, 1125,
            ModeFlags::INTERLACE, TimingSource::CtaVic(46),
        )
        .with_polarity(true, true),
    },
    // VIC 47 - 1280x720p @ 120.00 Hz
    VicTiming {
        vic: 47,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_blanking(
            148500, 1280, 370, 110, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(47),
        )
        .with_polarity(true, true),
    },
    // VIC 48 - 720x480p @ 119.88 Hz
    VicTiming {
        vic: 48,
        nominal_refresh_millihz: 119880,
        mode: Mode::from_blanking(
            54000, 720, 138, 16, 62,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(48),
        )
        .with_polarity(false, false),
    },
    // VIC 49 - 720x480p @ 119.88 Hz
    VicTiming {
        vic: 49,
        nominal_refresh_millihz: 119880,
        mode: Mode::from_blanking(
            54000, 720, 138, 16, 62,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(49),
        )
        .with_polarity(false, false),
    },
    // VIC 50 - 1440x480i @ 119.88 Hz
    VicTiming {
        vic: 50,
        nominal_refresh_millihz: 119880,
        mode: Mode::from_fields(
            54000, 1440, 1478, 1602, 1716,
            480, 488, 494, 525,
            ModeFlags::INTERLACE, TimingSource::CtaVic(50),
        )
        .with_polarity(false, false),
    },
    // VIC 51 - 1440x480i @ 119.88 Hz
    VicTiming {
        vic: 51,
        nominal_refresh_millihz: 119880,
        mode: Mode::from_fields(
            54000, 1440, 1478, 1602, 1716,
            480, 488, 494, 525,
            ModeFlags::INTERLACE, TimingSource::CtaVic(51),
        )
        .with_polarity(false, false),
    },
    // VIC 52 - 720x576p @ 200.00 Hz
    VicTiming {
        vic: 52,
        nominal_refresh_millihz: 200000,
        mode: Mode::from_blanking(
            108000, 720, 144, 12, 64,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(52),
        )
        .with_polarity(false, false),
    },
    // VIC 53 - 720x576p @ 200.00 Hz
    VicTiming {
        vic: 53,
        nominal_refresh_millihz: 200000,
        mode: Mode::from_blanking(
            108000, 720, 144, 12, 64,
            576, 49, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(53),
        )
        .with_polarity(false, false),
    },
    // VIC 54 - 1440x576i @ 200.00 Hz
    VicTiming {
        vic: 54,
        nominal_refresh_millihz: 200000,
        mode: Mode::from_fields(
            108000, 1440, 1464, 1590, 1728,
            576, 580, 586, 625,
            ModeFlags::INTERLACE, TimingSource::CtaVic(54),
        )
        .with_polarity(false, false),
    },
    // VIC 55 - 1440x576i @ 200.00 Hz
    VicTiming {
        vic: 55,
        nominal_refresh_millihz: 200000,
        mode: Mode::from_fields(
            108000, 1440, 1464, 1590, 1728,
            576, 580, 586, 625,
            ModeFlags::INTERLACE, TimingSource::CtaVic(55),
        )
        .with_polarity(false, false),
    },
    // VIC 56 - 720x480p @ 239.76 Hz
    VicTiming {
        vic: 56,
        nominal_refresh_millihz: 239760,
        mode: Mode::from_blanking(
            108000, 720, 138, 16, 62,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(56),
        )
        .with_polarity(false, false),
    },
    // VIC 57 - 720x480p @ 239.76 Hz
    VicTiming {
        vic: 57,
        nominal_refresh_millihz: 239760,
        mode: Mode::from_blanking(
            108000, 720, 138, 16, 62,
            480, 45, 9, 6,
            ModeFlags::NONE, TimingSource::CtaVic(57),
        )
        .with_polarity(false, false),
    },
    // VIC 58 - 1440x480i @ 239.76 Hz
    VicTiming {
        vic: 58,
        nominal_refresh_millihz: 239760,
        mode: Mode::from_fields(
            108000, 1440, 1478, 1602, 1716,
            480, 488, 494, 525,
            ModeFlags::INTERLACE, TimingSource::CtaVic(58),
        )
        .with_polarity(false, false),
    },
    // VIC 59 - 1440x480i @ 239.76 Hz
    VicTiming {
        vic: 59,
        nominal_refresh_millihz: 239760,
        mode: Mode::from_fields(
            108000, 1440, 1478, 1602, 1716,
            480, 488, 494, 525,
            ModeFlags::INTERLACE, TimingSource::CtaVic(59),
        )
        .with_polarity(false, false),
    },
    // VIC 60 - 1280x720p @ 24.00 Hz
    VicTiming {
        vic: 60,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            59400, 1280, 2020, 1760, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(60),
        )
        .with_polarity(true, true),
    },
    // VIC 61 - 1280x720p @ 25.00 Hz
    VicTiming {
        vic: 61,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            74250, 1280, 2680, 2420, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(61),
        )
        .with_polarity(true, true),
    },
    // VIC 62 - 1280x720p @ 30.00 Hz
    VicTiming {
        vic: 62,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            74250, 1280, 2020, 1760, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(62),
        )
        .with_polarity(true, true),
    },
    // VIC 63 - 1920x1080p @ 120.00 Hz
    VicTiming {
        vic: 63,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_blanking(
            297000, 1920, 280, 88, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(63),
        )
        .with_polarity(true, true),
    },
    // VIC 64 - 1920x1080p @ 100.00 Hz
    VicTiming {
        vic: 64,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            297000, 1920, 720, 528, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(64),
        )
        .with_polarity(true, true),
    },
    // VIC 65 - 1280x720p @ 24.00 Hz
    VicTiming {
        vic: 65,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            59400, 1280, 2020, 1760, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(65),
        )
        .with_polarity(true, true),
    },
    // VIC 66 - 1280x720p @ 25.00 Hz
    VicTiming {
        vic: 66,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            74250, 1280, 2680, 2420, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(66),
        )
        .with_polarity(true, true),
    },
    // VIC 67 - 1280x720p @ 30.00 Hz
    VicTiming {
        vic: 67,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            74250, 1280, 2020, 1760, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(67),
        )
        .with_polarity(true, true),
    },
    // VIC 68 - 1280x720p @ 50.00 Hz
    VicTiming {
        vic: 68,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            74250, 1280, 700, 440, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(68),
        )
        .with_polarity(true, true),
    },
    // VIC 69 - 1280x720p @ 60.00 Hz
    VicTiming {
        vic: 69,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            74250, 1280, 370, 110, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(69),
        )
        .with_polarity(true, true),
    },
    // VIC 70 - 1280x720p @ 100.00 Hz
    VicTiming {
        vic: 70,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            148500, 1280, 700, 440, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(70),
        )
        .with_polarity(true, true),
    },
    // VIC 71 - 1280x720p @ 120.00 Hz
    VicTiming {
        vic: 71,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_blanking(
            148500, 1280, 370, 110, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(71),
        )
        .with_polarity(true, true),
    },
    // VIC 72 - 1920x1080p @ 24.00 Hz
    VicTiming {
        vic: 72,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            74250, 1920, 830, 638, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(72),
        )
        .with_polarity(true, true),
    },
    // VIC 73 - 1920x1080p @ 25.00 Hz
    VicTiming {
        vic: 73,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            74250, 1920, 720, 528, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(73),
        )
        .with_polarity(true, true),
    },
    // VIC 74 - 1920x1080p @ 30.00 Hz
    VicTiming {
        vic: 74,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            74250, 1920, 280, 88, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(74),
        )
        .with_polarity(true, true),
    },
    // VIC 75 - 1920x1080p @ 50.00 Hz
    VicTiming {
        vic: 75,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            148500, 1920, 720, 528, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(75),
        )
        .with_polarity(true, true),
    },
    // VIC 76 - 1920x1080p @ 60.00 Hz
    VicTiming {
        vic: 76,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            148500, 1920, 280, 88, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(76),
        )
        .with_polarity(true, true),
    },
    // VIC 77 - 1920x1080p @ 100.00 Hz
    VicTiming {
        vic: 77,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            297000, 1920, 720, 528, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(77),
        )
        .with_polarity(true, true),
    },
    // VIC 78 - 1920x1080p @ 120.00 Hz
    VicTiming {
        vic: 78,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_blanking(
            297000, 1920, 280, 88, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(78),
        )
        .with_polarity(true, true),
    },
    // VIC 79 - 1680x720p @ 24.00 Hz
    VicTiming {
        vic: 79,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            59400, 1680, 1620, 1360, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(79),
        )
        .with_polarity(true, true),
    },
    // VIC 80 - 1680x720p @ 25.00 Hz
    VicTiming {
        vic: 80,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            59400, 1680, 1488, 1228, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(80),
        )
        .with_polarity(true, true),
    },
    // VIC 81 - 1680x720p @ 30.00 Hz
    VicTiming {
        vic: 81,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            59400, 1680, 960, 700, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(81),
        )
        .with_polarity(true, true),
    },
    // VIC 82 - 1680x720p @ 50.00 Hz
    VicTiming {
        vic: 82,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            82500, 1680, 520, 260, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(82),
        )
        .with_polarity(true, true),
    },
    // VIC 83 - 1680x720p @ 60.00 Hz
    VicTiming {
        vic: 83,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            99000, 1680, 520, 260, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(83),
        )
        .with_polarity(true, true),
    },
    // VIC 84 - 1680x720p @ 100.00 Hz
    VicTiming {
        vic: 84,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            165000, 1680, 320, 60, 40,
            720, 105, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(84),
        )
        .with_polarity(true, true),
    },
    // VIC 85 - 1680x720p @ 120.00 Hz
    VicTiming {
        vic: 85,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_blanking(
            198000, 1680, 320, 60, 40,
            720, 105, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(85),
        )
        .with_polarity(true, true),
    },
    // VIC 86 - 2560x1080p @ 24.00 Hz
    VicTiming {
        vic: 86,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            99000, 2560, 1190, 998, 44,
            1080, 20, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(86),
        )
        .with_polarity(true, true),
    },
    // VIC 87 - 2560x1080p @ 25.00 Hz
    VicTiming {
        vic: 87,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            90000, 2560, 640, 448, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(87),
        )
        .with_polarity(true, true),
    },
    // VIC 88 - 2560x1080p @ 30.00 Hz
    VicTiming {
        vic: 88,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            118800, 2560, 960, 768, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(88),
        )
        .with_polarity(true, true),
    },
    // VIC 89 - 2560x1080p @ 50.00 Hz
    VicTiming {
        vic: 89,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            185625, 2560, 740, 548, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(89),
        )
        .with_polarity(true, true),
    },
    // VIC 90 - 2560x1080p @ 60.00 Hz
    VicTiming {
        vic: 90,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            198000, 2560, 440, 248, 44,
            1080, 20, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(90),
        )
        .with_polarity(true, true),
    },
    // VIC 91 - 2560x1080p @ 100.00 Hz
    VicTiming {
        vic: 91,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            371250, 2560, 410, 218, 44,
            1080, 170, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(91),
        )
        .with_polarity(true, true),
    },
    // VIC 92 - 2560x1080p @ 120.00 Hz
    VicTiming {
        vic: 92,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_blanking(
            495000, 2560, 740, 548, 44,
            1080, 170, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(92),
        )
        .with_polarity(true, true),
    },
    // VIC 93 - 3840x2160p @ 24.00 Hz
    VicTiming {
        vic: 93,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            297000, 3840, 1660, 1276, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(93),
        )
        .with_polarity(true, true),
    },
    // VIC 94 - 3840x2160p @ 25.00 Hz
    VicTiming {
        vic: 94,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            297000, 3840, 1440, 1056, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(94),
        )
        .with_polarity(true, true),
    },
    // VIC 95 - 3840x2160p @ 30.00 Hz
    VicTiming {
        vic: 95,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            297000, 3840, 560, 176, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(95),
        )
        .with_polarity(true, true),
    },
    // VIC 96 - 3840x2160p @ 50.00 Hz
    VicTiming {
        vic: 96,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            594000, 3840, 1440, 1056, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(96),
        )
        .with_polarity(true, true),
    },
    // VIC 97 - 3840x2160p @ 60.00 Hz
    VicTiming {
        vic: 97,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            594000, 3840, 560, 176, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(97),
        )
        .with_polarity(true, true),
    },
    // VIC 98 - 4096x2160p @ 24.00 Hz
    VicTiming {
        vic: 98,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            297000, 4096, 1404, 1020, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(98),
        )
        .with_polarity(true, true),
    },
    // VIC 99 - 4096x2160p @ 25.00 Hz
    VicTiming {
        vic: 99,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            297000, 4096, 1184, 968, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(99),
        )
        .with_polarity(true, true),
    },
    // VIC 100 - 4096x2160p @ 30.00 Hz
    VicTiming {
        vic: 100,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            297000, 4096, 304, 88, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(100),
        )
        .with_polarity(true, true),
    },
    // VIC 101 - 4096x2160p @ 50.00 Hz
    VicTiming {
        vic: 101,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            594000, 4096, 1184, 968, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(101),
        )
        .with_polarity(true, true),
    },
    // VIC 102 - 4096x2160p @ 60.00 Hz
    VicTiming {
        vic: 102,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            594000, 4096, 304, 88, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(102),
        )
        .with_polarity(true, true),
    },
    // VIC 103 - 3840x2160p @ 24.00 Hz
    VicTiming {
        vic: 103,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            297000, 3840, 1660, 1276, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(103),
        )
        .with_polarity(true, true),
    },
    // VIC 104 - 3840x2160p @ 25.00 Hz
    VicTiming {
        vic: 104,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            297000, 3840, 1440, 1056, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(104),
        )
        .with_polarity(true, true),
    },
    // VIC 105 - 3840x2160p @ 30.00 Hz
    VicTiming {
        vic: 105,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            297000, 3840, 560, 176, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(105),
        )
        .with_polarity(true, true),
    },
    // VIC 106 - 3840x2160p @ 50.00 Hz
    VicTiming {
        vic: 106,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            594000, 3840, 1440, 1056, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(106),
        )
        .with_polarity(true, true),
    },
    // VIC 107 - 3840x2160p @ 60.00 Hz
    VicTiming {
        vic: 107,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            594000, 3840, 560, 176, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(107),
        )
        .with_polarity(true, true),
    },
    // VIC 108 - 1280x720p @ 48.00 Hz
    VicTiming {
        vic: 108,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            90000, 1280, 1220, 960, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(108),
        )
        .with_polarity(true, true),
    },
    // VIC 109 - 1280x720p @ 48.00 Hz
    VicTiming {
        vic: 109,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            90000, 1280, 1220, 960, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(109),
        )
        .with_polarity(true, true),
    },
    // VIC 110 - 1680x720p @ 48.00 Hz
    VicTiming {
        vic: 110,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            99000, 1680, 1070, 810, 40,
            720, 30, 5, 5,
            ModeFlags::NONE, TimingSource::CtaVic(110),
        )
        .with_polarity(true, true),
    },
    // VIC 111 - 1920x1080p @ 48.00 Hz
    VicTiming {
        vic: 111,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            148500, 1920, 830, 638, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(111),
        )
        .with_polarity(true, true),
    },
    // VIC 112 - 1920x1080p @ 48.00 Hz
    VicTiming {
        vic: 112,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            148500, 1920, 830, 638, 44,
            1080, 45, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(112),
        )
        .with_polarity(true, true),
    },
    // VIC 113 - 2560x1080p @ 48.00 Hz
    VicTiming {
        vic: 113,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            198000, 2560, 1190, 998, 44,
            1080, 20, 4, 5,
            ModeFlags::NONE, TimingSource::CtaVic(113),
        )
        .with_polarity(true, true),
    },
    // VIC 114 - 3840x2160p @ 48.00 Hz
    VicTiming {
        vic: 114,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            594000, 3840, 1660, 1276, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(114),
        )
        .with_polarity(true, true),
    },
    // VIC 115 - 4096x2160p @ 48.00 Hz
    VicTiming {
        vic: 115,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            594000, 4096, 1404, 1020, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(115),
        )
        .with_polarity(true, true),
    },
    // VIC 116 - 3840x2160p @ 48.00 Hz
    VicTiming {
        vic: 116,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            594000, 3840, 1660, 1276, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(116),
        )
        .with_polarity(true, true),
    },
    // VIC 117 - 3840x2160p @ 100.00 Hz
    VicTiming {
        vic: 117,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            1188000, 3840, 1440, 1056, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(117),
        )
        .with_polarity(true, true),
    },
    // VIC 118 - 3840x2160p @ 120.00 Hz
    VicTiming {
        vic: 118,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_blanking(
            1188000, 3840, 560, 176, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(118),
        )
        .with_polarity(true, true),
    },
    // VIC 119 - 3840x2160p @ 100.00 Hz
    VicTiming {
        vic: 119,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            1188000, 3840, 1440, 1056, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(119),
        )
        .with_polarity(true, true),
    },
    // VIC 120 - 3840x2160p @ 120.00 Hz
    VicTiming {
        vic: 120,
        nominal_refresh_millihz: 120000,
        mode: Mode::from_blanking(
            1188000, 3840, 560, 176, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(120),
        )
        .with_polarity(true, true),
    },
    // VIC 121 - 5120x2160p @ 24.00 Hz
    VicTiming {
        vic: 121,
        nominal_refresh_millihz: 24000,
        mode: Mode::from_blanking(
            396000, 5120, 2380, 1996, 88,
            2160, 40, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(121),
        )
        .with_polarity(true, true),
    },
    // VIC 122 - 5120x2160p @ 25.00 Hz
    VicTiming {
        vic: 122,
        nominal_refresh_millihz: 25000,
        mode: Mode::from_blanking(
            396000, 5120, 2080, 1696, 88,
            2160, 40, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(122),
        )
        .with_polarity(true, true),
    },
    // VIC 123 - 5120x2160p @ 30.00 Hz
    VicTiming {
        vic: 123,
        nominal_refresh_millihz: 30000,
        mode: Mode::from_blanking(
            396000, 5120, 880, 664, 88,
            2160, 40, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(123),
        )
        .with_polarity(true, true),
    },
    // VIC 124 - 5120x2160p @ 48.00 Hz
    VicTiming {
        vic: 124,
        nominal_refresh_millihz: 48000,
        mode: Mode::from_blanking(
            742500, 5120, 1130, 746, 88,
            2160, 315, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(124),
        )
        .with_polarity(true, true),
    },
    // VIC 125 - 5120x2160p @ 50.00 Hz
    VicTiming {
        vic: 125,
        nominal_refresh_millihz: 50000,
        mode: Mode::from_blanking(
            742500, 5120, 1480, 1096, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(125),
        )
        .with_polarity(true, true),
    },
    // VIC 126 - 5120x2160p @ 60.00 Hz
    VicTiming {
        vic: 126,
        nominal_refresh_millihz: 60000,
        mode: Mode::from_blanking(
            742500, 5120, 380, 164, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(126),
        )
        .with_polarity(true, true),
    },
    // VIC 127 - 5120x2160p @ 100.00 Hz
    VicTiming {
        vic: 127,
        nominal_refresh_millihz: 100000,
        mode: Mode::from_blanking(
            1485000, 5120, 1480, 1096, 88,
            2160, 90, 8, 10,
            ModeFlags::NONE, TimingSource::CtaVic(127),
        )
        .with_polarity(true, true),
    },
];

/// The highest code a short video descriptor can carry.
pub const MAX_VIC: u8 = 127;

/// Looks a VIC up, returning `None` for codes the standard does not define.
pub fn by_vic(vic: u8) -> Option<&'static VicTiming> {
    if vic == 0 || vic > MAX_VIC {
        return None;
    }
    CTA_VIC_TIMINGS.iter().find(|entry| entry.vic == vic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::modes::mode::{ModeFlags, TimingSource};

    /// CTA-861 rounds its printed rates; anything beyond this would mean a
    /// wrong clock or blanking in the row itself.
    const LABEL_TOLERANCE_MILLIHZ: u32 = 2000;

    #[test]
    fn table_covers_every_short_video_descriptor_code() {
        assert_eq!(CTA_VIC_TIMINGS.len(), MAX_VIC as usize);
        for (index, entry) in CTA_VIC_TIMINGS.iter().enumerate() {
            assert_eq!(entry.vic as usize, index + 1, "codes must be dense");
            assert_eq!(entry.mode.source, TimingSource::CtaVic(entry.vic));
            assert!(entry.mode.is_well_formed(), "VIC {} malformed", entry.vic);
            let computed = entry.mode.refresh_millihz();
            assert!(
                computed.abs_diff(entry.nominal_refresh_millihz) <= LABEL_TOLERANCE_MILLIHZ,
                "VIC {}: {} computes {computed} mHz, list says {}",
                entry.vic,
                entry.mode,
                entry.nominal_refresh_millihz
            );
        }
        assert!(by_vic(0).is_none());
        assert!(by_vic(128).is_none());
        assert!(by_vic(255).is_none());
    }

    #[test]
    fn interlaced_formats_carry_frame_line_totals() {
        // VIC 5 is 1920x1080i: 1125 frame lines, field rate 60 Hz.
        let vic5 = by_vic(5).expect("VIC 5");
        assert_eq!(vic5.mode.vtotal, 1125);
        assert_eq!(vic5.mode.vdisplay, 1080);
        assert_eq!(vic5.mode.refresh_millihz(), 60_000);
        assert!(vic5.mode.flags.contains(ModeFlags::INTERLACE));
        assert_eq!((vic5.mode.vsync_start, vic5.mode.vsync_end), (1084, 1094));
        // VIC 39 is the 50 Hz interlaced format with a 1250 line frame.
        let vic39 = by_vic(39).expect("VIC 39");
        assert_eq!(vic39.mode.vtotal, 1250);
        assert_eq!(vic39.mode.refresh_millihz(), 50_000);
        assert_eq!(by_vic(40).expect("VIC 40").mode.refresh_millihz(), 100_000);
    }

    /// Spot checks against the CTA-861 format list.
    #[test]
    fn published_formats_match_the_standard() {
        type Row = (u8, u16, u16, u32, u16, u16, u16, u16, u16, u16, u32, bool, bool);
        let cases: &[Row] = &[
            // vic, w, h, clock, hss, hse, htotal, vss, vse, vtotal, Hz, h+, v+
            (1, 640, 480, 25_175, 656, 752, 800, 490, 492, 525, 60, false, false),
            (
                16, 1920, 1080, 148_500, 2008, 2052, 2200, 1084, 1089, 1125, 60, true, true,
            ),
            (
                31, 1920, 1080, 148_500, 2448, 2492, 2640, 1084, 1089, 1125, 50, true, true,
            ),
            (4, 1280, 720, 74_250, 1390, 1430, 1650, 725, 730, 750, 60, true, true),
            (19, 1280, 720, 74_250, 1720, 1760, 1980, 725, 730, 750, 50, true, true),
            (
                97, 3840, 2160, 594_000, 4016, 4104, 4400, 2168, 2178, 2250, 60, true, true,
            ),
            (
                95, 3840, 2160, 297_000, 4016, 4104, 4400, 2168, 2178, 2250, 30, true, true,
            ),
            // Pixel-repeated SD formats: CTA-861 lists the repeated width and
            // the multiplied clock, for example 720(1440)x576 at 200 Hz.
            (21, 1440, 576, 27_000, 1464, 1590, 1728, 580, 586, 625, 50, false, false),
            (56, 720, 480, 108_000, 736, 798, 858, 489, 495, 525, 240, false, false),
        ];
        for &(vic, width, height, clock, hss, hse, htotal, vss, vse, vtotal, hz, hp, vp) in cases {
            let entry = by_vic(vic).unwrap_or_else(|| panic!("VIC {vic} missing"));
            let mode = entry.mode;
            assert_eq!((mode.hdisplay, mode.vdisplay), (width, height), "VIC {vic}");
            assert_eq!(mode.clock_khz, clock, "VIC {vic} clock");
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
                "VIC {vic} edges"
            );
            assert_eq!(
                (mode.hsync_positive, mode.vsync_positive),
                (hp, vp),
                "VIC {vic} polarity"
            );
            assert_eq!(mode.refresh_hz_rounded(), hz, "VIC {vic} rate");
        }
    }
}
