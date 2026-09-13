//! VESA Coordinated Video Timings (CVT) generation.
//!
//! A modern panel frequently advertises a resolution and a refresh rate but
//! pins no timing for it: the sink either lists a standard timing descriptor
//! or states the range of frequencies it accepts and expects the source to
//! compute the rest.  CVT is the formula that turns a resolution, a refresh
//! rate and a blanking choice into a complete timing.
//!
//! The arithmetic here is integer only.  Every intermediate the formulas
//! define is in units that are exact in integers - picoseconds for periods,
//! milli-percent for the duty cycle, kilohertz for the clock - so the results
//! are bit-for-bit reproducible on any host and need no floating point in the
//! kernel.  The test module proves the implementation by reproducing all 28
//! CVT-derived rows of the VESA DMT 1.13 table exactly.
//!
//! Provenance: the formulas and their constants are VESA CVT 1.1/1.2, as
//! published in the standard's own text.  They are re-expressed here from the
//! description of the algorithm, not translated from any implementation.

use super::mode::{Mode, ModeFlags, TimingSource};

/// Which blanking the sink wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CvtBlanking {
    /// CVT with standard blanking: the horizontal blanking follows from the
    /// ideal duty cycle, which is what CRTs and many older panels expect.
    Standard,
    /// CVT reduced blanking version 1: a fixed 160 pixel horizontal blank and
    /// a 3 line vertical front porch.  The common choice for LCD panels.
    ReducedV1,
    /// CVT reduced blanking version 2: an 80 pixel horizontal blank, an 8 line
    /// vertical sync and a 1 kHz clock granularity.
    ReducedV2,
}

/// Everything CVT needs to produce a timing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CvtRequest {
    pub hdisplay: u16,
    pub vdisplay: u16,
    /// Requested refresh rate in millihertz (whole hertz is the common case).
    pub refresh_millihz: u32,
    pub blanking: CvtBlanking,
}

impl CvtRequest {
    pub const fn new(hdisplay: u16, vdisplay: u16, refresh_millihz: u32, blanking: CvtBlanking) -> Self {
        Self {
            hdisplay,
            vdisplay,
            refresh_millihz,
            blanking,
        }
    }
}

/// Minimum vertical sync plus back porch for standard blanking, in
/// microseconds (`MIN_VSYNC_BP`).
const MIN_VSYNC_BP_PS: u64 = 550_000_000;
/// Minimum vertical front porch in lines (`MIN_V_PORCH`).
const MIN_V_PORCH: u64 = 3;
/// Minimum vertical back porch for CVT-RBv1.
const MIN_V_BPORCH: u64 = 7;
/// Fixed vertical back porch for CVT-RBv2.
const FIXED_V_BPORCH: u64 = 6;
/// `C'` and `M'` of the ideal duty cycle formula.
const C_PRIME_MILLI: u64 = 30_000;
const M_PRIME_MILLI: u64 = 300_000;
/// Minimum vertical blanking period for reduced blanking, in microseconds.
const RB_MIN_VBLANK_US: u64 = 460;
/// Horizontal cell granularity: 8 pixels for CVT and RBv1, 1 for RBv2.
const CELL_GRAN_STANDARD: u64 = 8;
/// Horizontal sync width as a fraction of the total line, in percent.
const H_SYNC_PERCENT: u64 = 8;
/// Horizontal sync width for reduced blanking, in pixels.
const RB_H_SYNC: u64 = 32;

/// One second in picoseconds, used to turn a rate into a period exactly.
const PS_PER_SECOND: u64 = 1_000_000_000_000;
/// Millihertz to hertz.
const MHZ_PER_HZ: u64 = 1000;
/// Highest pixel clock CVT may produce, in kHz.  Four gigahertz is far beyond
/// any link this kernel drives; the bound exists so a nonsensical request
/// cannot overflow a timing field later.
const MAX_PIXEL_CLOCK_KHZ: u64 = 4_000_000;

/// Generates a CVT timing.
///
/// Returns `None` when the request has no representable answer: a zero
/// refresh rate, a mode whose totals do not fit the 16-bit timing fields, or
/// a rate so high that the vertical blanking constraint cannot be met.  The
/// caller must treat `None` as "this sink cannot be driven at this size and
/// rate", never as a reason to fall back to a different size silently.
pub fn generate(request: CvtRequest) -> Option<Mode> {
    let CvtRequest {
        hdisplay,
        vdisplay,
        refresh_millihz,
        blanking,
    } = request;
    if hdisplay == 0 || vdisplay == 0 || refresh_millihz == 0 || refresh_millihz > 1000 * 1000 {
        return None;
    }
    let reduced = blanking != CvtBlanking::Standard;
    let cell_gran = if blanking == CvtBlanking::ReducedV2 {
        1
    } else {
        CELL_GRAN_STANDARD
    };
    let hactive = u64::from(hdisplay) / cell_gran * cell_gran;
    let vactive = u64::from(vdisplay);

    // Vertical sync width follows the aspect ratio for CVT and RBv1; RBv2
    // fixes it at eight lines.
    let vsync = if blanking == CvtBlanking::ReducedV2 {
        8
    } else {
        vertical_sync_width(hactive, vactive)
    };

    let period_ps = PS_PER_SECOND / u64::from(refresh_millihz);

    let (h_blank, h_sync, h_front, v_blank, v_sync_bp, v_front, total_pixels, clock_khz) =
        if !reduced {
            // Standard blanking: estimate the line period from the frame
            // period minus the minimum vertical sync plus back porch, then
            // derive the blanking from the ideal duty cycle at that line rate.
            let h_period_ps = subtract(period_ps, MIN_VSYNC_BP_PS)?
                .checked_mul(2)?
                .checked_div(vactive * 2 + MIN_V_PORCH * 2)?;
            if h_period_ps == 0 {
                return None;
            }
            let mut v_sync_bp = MIN_VSYNC_BP_PS / h_period_ps + 1;
            if v_sync_bp < vsync + MIN_V_BPORCH {
                v_sync_bp = vsync + MIN_V_BPORCH;
            }
            let v_blank = v_sync_bp + MIN_V_PORCH;

            // Ideal duty cycle = C' - M' * line period (in milliseconds),
            // expressed in milli-percent.
            let duty_milli = C_PRIME_MILLI
                .saturating_sub(M_PRIME_MILLI * h_period_ps / 1_000_000_000)
                .max(20_000);
            if duty_milli >= 100_000 {
                return None;
            }
            let h_blank =
                hactive * duty_milli / (100_000 - duty_milli) / (2 * cell_gran) * (2 * cell_gran);
            let total_pixels = hactive.checked_add(h_blank)?;
            let h_sync = total_pixels * H_SYNC_PERCENT / 100 / cell_gran * cell_gran;
            let h_front = h_blank / 2;
            if h_front <= h_sync {
                return None;
            }
            let h_front = h_front - h_sync;
            // 0.25 MHz clock granularity.
            let clock_khz = total_pixels * 1_000_000_000 / h_period_ps / 250 * 250;
            (
                h_blank,
                h_sync,
                h_front,
                v_blank,
                v_sync_bp,
                v_blank - v_sync_bp,
                total_pixels,
                clock_khz,
            )
        } else {
            let (h_blank, rb_v_fporch, v_back_porch, clock_step) = match blanking {
                CvtBlanking::ReducedV1 => (160, 3, MIN_V_BPORCH, 250),
                _ => (80, 1, FIXED_V_BPORCH, 1),
            };
            let h_period_ps =
                subtract(period_ps, RB_MIN_VBLANK_US * 1_000_000)?.checked_div(vactive)?;
            if h_period_ps == 0 {
                return None;
            }
            let vbi_lines = RB_MIN_VBLANK_US * 1_000_000 / h_period_ps + 1;
            let v_blank = vbi_lines.max(rb_v_fporch + vsync + v_back_porch);
            let total_pixels = hactive.checked_add(h_blank)?;
            // A reduced-blanking line is fixed, so the clock is the product of
            // the frame's lines, its pixels and the frame rate.
            let clock_khz = u64::from(refresh_millihz) * (v_blank + vactive) * total_pixels
                / 1_000_000
                / clock_step
                * clock_step;
            let v_sync_bp = if blanking == CvtBlanking::ReducedV1 {
                v_blank - rb_v_fporch
            } else {
                vsync + v_back_porch
            };
            let h_front = if blanking == CvtBlanking::ReducedV1 {
                h_blank / 2 - RB_H_SYNC
            } else {
                8
            };
            (
                h_blank,
                RB_H_SYNC,
                h_front,
                v_blank,
                v_sync_bp,
                v_blank - v_sync_bp,
                total_pixels,
                clock_khz,
            )
        };

    // Everything must fit the timing fields of a display engine and be
    // internally ordered; a CVT request that cannot say so has no answer.
    let htotal = hactive + h_blank;
    let vtotal = vactive + v_blank;
    if clock_khz == 0
        || htotal > u64::from(u16::MAX)
        || vtotal > u64::from(u16::MAX)
        || clock_khz > MAX_PIXEL_CLOCK_KHZ
        || h_sync == 0
        || vsync == 0
        || h_sync >= total_pixels
    {
        return None;
    }

    let mode = Mode::from_blanking(
        clock_khz as u32,
        hactive as u16,
        h_blank as u16,
        h_front as u16,
        h_sync as u16,
        vactive as u16,
        v_blank as u16,
        v_front as u16,
        vsync as u16,
        ModeFlags::NONE,
        TimingSource::Cvt {
            reduced_blanking: reduced,
        },
    );
    mode.is_well_formed().then_some(mode)
}

/// Vertical sync width in lines, chosen from the aspect ratio as CVT directs.
fn vertical_sync_width(hactive: u64, vactive: u64) -> u64 {
    if vactive * 4 / 3 == hactive {
        4
    } else if vactive * 16 / 9 == hactive {
        5
    } else if vactive * 16 / 10 == hactive {
        6
    } else if vactive % 4 == 0 && vactive * 5 / 4 == hactive {
        7
    } else if vactive * 15 / 9 == hactive {
        7
    } else {
        10
    }
}

/// `a - b`, or `None` when it would wrap: a request whose frame period is
/// shorter than the minimum vertical blanking has no CVT answer.
fn subtract(a: u64, b: u64) -> Option<u64> {
    a.checked_sub(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::modes::dmt;

    fn mode(h: u16, v: u16, hz: u32, blanking: CvtBlanking) -> Mode {
        generate(CvtRequest::new(h, v, hz * 1000, blanking)).expect("CVT must produce a timing")
    }

    /// Published CVT reference timings.  These are the numbers a display
    /// engineer checks a new modeset against, so they are exact.
    #[test]
    fn reference_timings_match_the_cvt_standard() {
        type Row = (u16, u16, u32, CvtBlanking, u32, u16, u16, u16, u16);
        let cases: &[Row] = &[
            // w, h, Hz, blanking, clock kHz, hss, hse, htotal, vtotal
            (
                1920,
                1080,
                60,
                CvtBlanking::Standard,
                173_000,
                2048,
                2248,
                2576,
                1120,
            ),
            (
                1920,
                1080,
                60,
                CvtBlanking::ReducedV1,
                138_500,
                1968,
                2000,
                2080,
                1111,
            ),
            (
                1920,
                1080,
                60,
                CvtBlanking::ReducedV2,
                133_320,
                1928,
                1960,
                2000,
                1111,
            ),
            (
                1680,
                1050,
                60,
                CvtBlanking::ReducedV1,
                119_000,
                1728,
                1760,
                1840,
                1080,
            ),
            (
                1920,
                1200,
                60,
                CvtBlanking::ReducedV1,
                154_000,
                1968,
                2000,
                2080,
                1235,
            ),
            (
                1440,
                900,
                60,
                CvtBlanking::ReducedV1,
                88_750,
                1488,
                1520,
                1600,
                926,
            ),
            (
                2560,
                1440,
                60,
                CvtBlanking::ReducedV1,
                241_500,
                2608,
                2640,
                2720,
                1481,
            ),
            (
                3840,
                2160,
                60,
                CvtBlanking::ReducedV2,
                522_614,
                3848,
                3880,
                3920,
                2222,
            ),
        ];
        for &(w, h, hz, blanking, clock, hss, hse, htotal, vtotal) in cases {
            let generated = mode(w, h, hz, blanking);
            assert_eq!(
                (
                    generated.clock_khz,
                    generated.hsync_start,
                    generated.hsync_end,
                    generated.htotal,
                    generated.vtotal
                ),
                (clock, hss, hse, htotal, vtotal),
                "{w}x{h}@{hz} {blanking:?}"
            );
            assert!(generated.is_well_formed());
            assert_eq!(
                generated.source,
                TimingSource::Cvt {
                    reduced_blanking: blanking != CvtBlanking::Standard
                }
            );
            // The generated timing must actually hit the requested rate.
            assert!(
                generated.refresh_millihz().abs_diff(hz * 1000) <= 100,
                "{generated} misses the requested rate"
            );
        }
    }

    /// The strongest available check on the formulas: the DMT standard
    /// published 28 rows that it generated with CVT itself.  Reproducing all
    /// of them exactly pins the duty cycle, the granularity and the clock
    /// rounding at once.
    #[test]
    fn reproduces_every_cvt_derived_dmt_row() {
        let mut checked = 0;
        for entry in dmt::DMT_TIMINGS {
            if !entry.is_cvt_derived() {
                continue;
            }
            checked += 1;
            let blanking = if entry.reduced_blanking {
                CvtBlanking::ReducedV1
            } else {
                CvtBlanking::Standard
            };
            let generated = generate(CvtRequest::new(
                entry.mode.hdisplay,
                entry.mode.vdisplay,
                entry.nominal_refresh_millihz,
                blanking,
            ))
            .unwrap_or_else(|| panic!("DMT {:#04x} must be generatable", entry.code));
            assert_eq!(
                (
                    generated.clock_khz,
                    generated.hsync_start,
                    generated.hsync_end,
                    generated.htotal,
                    generated.vsync_start,
                    generated.vsync_end,
                    generated.vtotal
                ),
                (
                    entry.mode.clock_khz,
                    entry.mode.hsync_start,
                    entry.mode.hsync_end,
                    entry.mode.htotal,
                    entry.mode.vsync_start,
                    entry.mode.vsync_end,
                    entry.mode.vtotal
                ),
                "DMT {:#04x} ({}) is not reproduced",
                entry.code,
                entry.mode
            );
        }
        assert_eq!(checked, 28, "the DMT table has 28 CVT-derived rows");
    }

    #[test]
    fn every_generated_timing_is_well_formed_over_a_grid() {
        let blankings = [
            CvtBlanking::Standard,
            CvtBlanking::ReducedV1,
            CvtBlanking::ReducedV2,
        ];
        let mut generated = 0;
        for &blanking in &blankings {
            for width in (640..=3840).step_by(160) {
                for height in (480..=2160).step_by(120) {
                    for hz in [24, 30, 50, 60, 75, 120] {
                        let Some(mode) = generate(CvtRequest::new(
                            width as u16,
                            height as u16,
                            hz * 1000,
                            blanking,
                        )) else {
                            continue;
                        };
                        generated += 1;
                        assert!(mode.is_well_formed(), "{mode}");
                        assert!(mode.hblank() > 0 && mode.vblank() > 0, "{mode}");
                        assert!(mode.hsync_len() > 0 && mode.vsync_len() > 0, "{mode}");
                        assert!(
                            mode.refresh_millihz().abs_diff(hz * 1000) <= hz * 20,
                            "{mode} is far from its requested rate"
                        );
                    }
                }
            }
        }
        assert!(generated > 1000, "the grid must exercise real work");
    }

    #[test]
    fn impossible_requests_produce_none_rather_than_a_bad_timing() {
        // Zero refresh: no period to divide by.
        assert!(generate(CvtRequest::new(1920, 1080, 0, CvtBlanking::Standard)).is_none());
        assert!(generate(CvtRequest::new(1920, 1080, 0, CvtBlanking::ReducedV1)).is_none());
        // Zero size.
        assert!(generate(CvtRequest::new(0, 1080, 60_000, CvtBlanking::Standard)).is_none());
        assert!(generate(CvtRequest::new(1920, 0, 60_000, CvtBlanking::ReducedV2)).is_none());
        // A width below one eight-pixel cell rounds away to nothing.
        assert!(generate(CvtRequest::new(1, 1, 60_000, CvtBlanking::Standard)).is_none());
        assert!(generate(CvtRequest::new(7, 480, 60_000, CvtBlanking::ReducedV1)).is_none());
        // Totals that cannot fit the 16-bit timing fields.
        assert!(generate(CvtRequest::new(60_000, 480, 60_000, CvtBlanking::Standard)).is_none());
        assert!(generate(CvtRequest::new(60_000, 480, 60_000, CvtBlanking::ReducedV1)).is_none());
        assert!(generate(CvtRequest::new(640, 65_000, 60_000, CvtBlanking::Standard)).is_none());
    }
}
