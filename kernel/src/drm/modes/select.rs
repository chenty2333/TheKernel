//! From "this EDID" to "program this mode".
//!
//! [`collect_modes`] enumerates every timing an EDID advertises, tagging each
//! with the part of the block it came from.  [`select`] then picks one, using a
//! preference order that is stated here rather than implied by iteration
//! order, and reports *why* - including when it had to fall back to a timing
//! the sink never advertised, which is the one decision a display engineer
//! must never have to guess at from a blank screen.
//!
//! Preference order, highest first:
//!
//! 1. the sink's **preferred timing** (the first detailed timing descriptor of
//!    the base block);
//! 2. the **firmware mode**, when the caller passes the mode the firmware
//!    already programmed and the sink advertises the same timing: an
//!    already-validated timing is safer than a fresh one during bring-up;
//! 3. **established timings** from the base block bitmaps - resolved through
//!    the DMT table, because those bitmaps name DMT rows;
//! 4. **standard timings**, resolved through the DMT table's standard timing
//!    identifiers first and generated with CVT only when DMT has no row;
//! 5. other **detailed timing descriptors** (base block, then CTA-861);
//! 6. **CTA-861 video identification codes**, which is how an HDMI sink names
//!    the formats it accepts;
//! 7. an explicit, warned **built-in fallback**.
//!
//! Within one step the better mode wins: larger active area, then higher
//! refresh rate, then lower pixel clock, then lower horizontal total.  Every
//! comparison is total, so the same EDID and the same constraints always
//! produce the same mode.

use core::fmt;

use super::{
    MAX_MODES,
    cvt::{self, CvtBlanking, CvtRequest},
    dmt,
    edid::{Descriptor, Edid, RangeLimitsKind, StandardTiming},
    mode::{Mode, ModeFlags, TimingSource},
    vic,
};

/// The conservative timing this kernel programs when a sink advertises nothing
/// it can use.
///
/// 640x480 at 60 Hz (VESA DMT 0x04) is the timing every VGA-compatible sink
/// accepts, and it is also CTA-861 VIC 1, which every HDMI sink must accept.
/// It is deliberately the smallest common denominator: the point of the
/// fallback is to get *something* on the panel and log the fact, not to guess
/// at a native mode.
pub const FALLBACK_MODE: Mode = Mode::from_blanking(
    25_175,
    640,
    160,
    16,
    96,
    480,
    45,
    10,
    2,
    ModeFlags::NONE,
    TimingSource::Builtin,
)
.with_polarity(false, false);

/// Which part of an EDID named a mode.  The order of the variants is the
/// selection order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ModeClass {
    Preferred,
    Established,
    Standard,
    DetailedTiming,
    CtaVic,
}

/// What an enumeration had to skip or could not fit.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct CollectReport {
    pub added: u8,
    pub duplicates: u8,
    /// Established-timing bits and standard timing codes that name a mode the
    /// DMT table does not define, so no exact timing could be resolved.
    pub skipped_without_a_table_row: u8,
    /// Video data block codes with no CTA-861 entry.
    pub skipped_unknown_vic: u8,
    /// The mode list filled up; later modes were dropped.
    pub overflowed: bool,
}

/// A mode plus where it came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Candidate {
    pub mode: Mode,
    pub class: ModeClass,
    /// True for a CTA-861 short video descriptor the sink marked native, or
    /// for the sink's preferred timing.
    pub native: bool,
}

/// A bounded list of advertised modes.
///
/// The list is a fixed array, not a `Vec`: enumeration happens during display
/// bring-up, before it is known that the allocator is usable, and the number
/// of modes an EDID can name is bounded anyway (four detailed timings,
/// seventeen established bits, fourteen standard timing codes and 127 VICs).
#[derive(Clone, Copy)]
pub struct ModeList {
    entries: [Candidate; MAX_MODES],
    len: usize,
}

impl Default for ModeList {
    fn default() -> Self {
        Self::new()
    }
}

impl ModeList {
    pub const fn new() -> ModeList {
        ModeList {
            entries: [Candidate {
                mode: FALLBACK_MODE,
                class: ModeClass::CtaVic,
                native: false,
            }; MAX_MODES],
            len: 0,
        }
    }

    /// Adds a mode unless an identical timing is already listed.
    ///
    /// Returns false when the mode was a duplicate or the list is full; the
    /// caller keeps both counts in [`CollectReport`].
    pub fn push(&mut self, candidate: Candidate) -> bool {
        if self
            .iter()
            .any(|entry| entry.mode.same_timing(&candidate.mode))
        {
            return false;
        }
        let Some(slot) = self.entries.get_mut(self.len) else {
            return false;
        };
        *slot = candidate;
        self.len += 1;
        true
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len >= MAX_MODES
    }

    pub fn get(&self, index: usize) -> Option<&Candidate> {
        self.entries.get(index).filter(|_| index < self.len)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Candidate> {
        self.entries.iter().take(self.len)
    }

    pub fn modes(&self) -> impl Iterator<Item = &Mode> {
        self.iter().map(|candidate| &candidate.mode)
    }

    /// True when a timing identical to `mode` is listed.
    pub fn contains_timing(&self, mode: &Mode) -> bool {
        self.iter().any(|entry| entry.mode.same_timing(mode))
    }
}

impl fmt::Debug for ModeList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Enumerates every timing an EDID advertises.
///
/// The sink's preferred timing is enumerated first, so a truncated list can
/// only ever lose modes a selection would have ranked lower.
pub fn collect_modes(edid: &Edid<'_>, list: &mut ModeList) -> CollectReport {
    let mut report = CollectReport::default();

    // 1. The sink's preferred timing, then the base block's other detailed
    //    timings.
    for (index, timing) in edid.detailed_timings().enumerate() {
        let preferred = index == 0 && edid.has_preferred_timing();
        push(
            list,
            &mut report,
            timing.mode,
            if preferred {
                ModeClass::Preferred
            } else {
                ModeClass::DetailedTiming
            },
            preferred,
        );
    }

    // 2. Established timings: the bitmaps name DMT rows.
    let established = edid.established_timings();
    for (byte_index, byte) in established.i_ii.iter().enumerate() {
        for bit in 0..8 {
            if byte & (0x80 >> bit) == 0 {
                continue;
            }
            match established_timing_mode(byte_index * 8 + bit) {
                Some(mode) => push(list, &mut report, mode, ModeClass::Established, false),
                None => report.skipped_without_a_table_row += 1,
            }
        }
    }
    if let Some(bits) = established.iii {
        for (byte_index, byte) in bits.iter().enumerate() {
            for bit in 0..8 {
                if byte & (0x80 >> bit) == 0 {
                    continue;
                }
                match ESTABLISHED_III_DMT
                    .get(byte_index * 8 + bit)
                    .and_then(|code| dmt::by_code(*code))
                {
                    Some(entry) => {
                        push(list, &mut report, entry.mode, ModeClass::Established, false)
                    }
                    None => report.skipped_without_a_table_row += 1,
                }
            }
        }
    }

    // 3. Standard timings, including the six extra codes a 0xFA descriptor
    //    carries.
    let mut codes = StandardCodes::new();
    for timing in edid.standard_timings() {
        if let StandardTiming::Named { code, .. } = timing {
            codes.push(*code);
        }
    }
    if let Some(extra) = edid.standard_timing_ids() {
        for chunk in extra.chunks_exact(2) {
            let code = [chunk[0], chunk[1]];
            if code != [0x01, 0x01] && code[0] != 0 {
                codes.push(code);
            }
        }
    }
    for code in codes.iter() {
        match standard_timing_mode(edid, *code) {
            Some(mode) => push(list, &mut report, mode, ModeClass::Standard, false),
            None => report.skipped_without_a_table_row += 1,
        }
    }

    // 4. CVT three-byte timing codes: the sink names a size and a rate and
    //    expects the source to compute the rest.
    if let Some(block) = edid.cvt_timing_codes() {
        for code in block.codes() {
            if !code.supports_preferred_refresh() {
                report.skipped_without_a_table_row += 1;
                continue;
            }
            let vdisplay = code.lines_per_field;
            let hdisplay = match code.aspect_bits & 0x3 {
                0 => vdisplay * 4 / 3,
                1 => vdisplay * 16 / 9,
                2 => vdisplay * 16 / 10,
                _ => vdisplay * 15 / 9,
            };
            let blanking = if code.prefers_reduced_blanking() {
                CvtBlanking::ReducedV1
            } else {
                CvtBlanking::Standard
            };
            match cvt::generate(CvtRequest::new(
                hdisplay,
                vdisplay,
                code.preferred_refresh_millihz(),
                blanking,
            )) {
                Some(mode) => push(list, &mut report, mode, ModeClass::Standard, false),
                None => report.skipped_without_a_table_row += 1,
            }
        }
    }

    // 5. CTA-861: detailed timings first (native ones first, as CTA-861 orders
    //    them), then the short video descriptors.
    for cta in edid.cta_extensions() {
        let native_count = cta.native_dtd_count();
        for (index, descriptor) in cta.detailed_timings() {
            let Descriptor::DetailedTiming(timing) = descriptor else {
                continue;
            };
            push(
                list,
                &mut report,
                timing.mode,
                ModeClass::DetailedTiming,
                index < native_count,
            );
        }
        for svd in cta.short_video_descriptors() {
            match vic::by_vic(svd.vic) {
                Some(entry) => push(list, &mut report, entry.mode, ModeClass::CtaVic, svd.native),
                None => report.skipped_unknown_vic += 1,
            }
        }
    }

    report
}

fn push(
    list: &mut ModeList,
    report: &mut CollectReport,
    mode: Mode,
    class: ModeClass,
    native: bool,
) {
    if list.push(Candidate {
        mode,
        class,
        native,
    }) {
        report.added += 1;
    } else if list.is_full() {
        report.overflowed = true;
    } else {
        report.duplicates += 1;
    }
}

/// A fixed-capacity buffer for the standard timing codes of one EDID.
///
/// Fourteen two-byte codes are the most a base block can carry: eight in the
/// bitmap area plus six in a 0xFA descriptor.
#[derive(Clone, Copy)]
struct StandardCodes {
    bytes: [[u8; 2]; 14],
    len: usize,
}

impl StandardCodes {
    const fn new() -> StandardCodes {
        StandardCodes {
            bytes: [[0x01, 0x01]; 14],
            len: 0,
        }
    }

    fn push(&mut self, code: [u8; 2]) {
        if let Some(slot) = self.bytes.get_mut(self.len) {
            *slot = code;
            self.len += 1;
        }
    }

    fn iter(&self) -> impl Iterator<Item = &[u8; 2]> {
        self.bytes.iter().take(self.len)
    }
}

/// The resolution and refresh of each established-timing-I/II bit, in bit
/// order.
///
/// The bitmaps are ordered by the standard; the blanking is not in the bitmap,
/// so each entry is resolved through the DMT table.  Five of the seventeen bits
/// name legacy modes DMT 1.13 does not define (720x400 at 70 and 88 Hz,
/// 640x480 at 67 Hz, 832x624 at 75 Hz, 1024x768 at 87 Hz interlaced and
/// 1152x870 at 75 Hz); those are counted as skipped rather than approximated,
/// and a sink that wants one of them also carries a detailed timing for it.
const ESTABLISHED_TIMINGS: &[(u16, u16, u32)] = &[
    (720, 400, 70),
    (720, 400, 88),
    (640, 480, 60),
    (640, 480, 67),
    (640, 480, 72),
    (640, 480, 75),
    (800, 600, 56),
    (800, 600, 60),
    (800, 600, 72),
    (800, 600, 75),
    (832, 624, 75),
    (1024, 768, 87),
    (1024, 768, 60),
    (1024, 768, 70),
    (1024, 768, 75),
    (1280, 1024, 75),
    (1152, 870, 75),
];

fn established_timing_mode(bit: usize) -> Option<Mode> {
    let (hdisplay, vdisplay, refresh) = *ESTABLISHED_TIMINGS.get(bit)?;
    dmt::by_size(hdisplay, vdisplay, refresh, None).map(|entry| entry.mode)
}

/// The DMT code each bit of an established-timings-III bitmap names, in bit
/// order.  Forty-four bits are defined; the remaining four bits of the six-byte
/// bitmap are reserved.
const ESTABLISHED_III_DMT: [u8; 44] = [
    0x01, 0x02, 0x03, 0x07, 0x0e, 0x0c, 0x13, 0x15, // byte 0
    0x16, 0x17, 0x18, 0x19, 0x20, 0x21, 0x23, 0x25, // byte 1
    0x27, 0x2e, 0x2f, 0x30, 0x31, 0x29, 0x2a, 0x2b, // byte 2
    0x2c, 0x39, 0x3a, 0x3b, 0x3c, 0x33, 0x34, 0x35, // byte 3
    0x36, 0x37, 0x3e, 0x3f, 0x41, 0x42, 0x44, 0x45, // byte 4
    0x46, 0x47, 0x49, 0x4a, // byte 5
];

/// The mode a two-byte standard timing code names.
///
/// E-EDID 1.4 Appendix B: when the descriptor matches a DMT timing, that
/// timing's exact parameters must be used.  The code is the DMT standard
/// timing identifier, so the lookup is direct; a code with no DMT row is
/// generated with CVT instead, preferring the blanking the sink's CVT range
/// descriptor asks for and reduced blanking when the sink says nothing.
fn standard_timing_mode(edid: &Edid<'_>, code: [u8; 2]) -> Option<Mode> {
    if let Some(entry) = dmt::by_std_id(u16::from_be_bytes(code)) {
        return Some(entry.mode);
    }
    let (hdisplay, vdisplay, refresh_hz) = StandardTiming::named_parts(code)?;
    if let Some(entry) = dmt::by_size(hdisplay, vdisplay, u32::from(refresh_hz), None) {
        return Some(entry.mode);
    }
    let reduced_blanking = edid
        .range_limits()
        .and_then(|limits| match limits.kind {
            RangeLimitsKind::Cvt(cvt) => Some(cvt.reduced_blanking),
            _ => None,
        })
        .unwrap_or(true);
    cvt::generate(CvtRequest::new(
        hdisplay,
        vdisplay,
        u32::from(refresh_hz) * 1000,
        if reduced_blanking {
            CvtBlanking::ReducedV1
        } else {
            CvtBlanking::Standard
        },
    ))
}

/// How a caller wants the sink's own range limits treated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RangeCheck {
    /// Never consult the range limits descriptor.
    Ignore,
    /// Prefer a mode inside the sink's stated range, but fall back to one
    /// outside it rather than to the built-in timing, and report that the
    /// limits were relaxed.
    Prefer,
    /// Never select a mode the sink's stated range excludes.
    Require,
}

/// What the display engine and the link can carry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Constraints {
    /// Highest pixel clock the link can carry, in kHz.
    pub max_clock_khz: u32,
    pub max_hdisplay: Option<u16>,
    pub max_vdisplay: Option<u16>,
    /// Lowest acceptable refresh rate in millihertz; 0 accepts anything.
    pub min_refresh_millihz: u32,
    /// Whether interlaced modes may be selected.  Off by default: a modern
    /// panel is progressive, and driving an interlaced timing is a decision
    /// worth making explicitly.
    pub allow_interlace: bool,
    pub range_check: RangeCheck,
    /// The mode the firmware already programmed, when the caller knows it.
    pub firmware_mode: Option<Mode>,
}

impl Constraints {
    /// Everything a link and a sink can do: progressive only, range limits
    /// preferred but not enforced.
    pub const fn unlimited() -> Constraints {
        Constraints {
            max_clock_khz: u32::MAX,
            max_hdisplay: None,
            max_vdisplay: None,
            min_refresh_millihz: 0,
            allow_interlace: false,
            range_check: RangeCheck::Prefer,
            firmware_mode: None,
        }
    }

    /// True when nothing but the mode's own shape and the link's limits rule
    /// it out.  The sink's range limits are applied by [`select`], which can
    /// relax them.
    pub fn admits(&self, mode: &Mode) -> bool {
        mode.is_well_formed()
            && mode.clock_khz <= self.max_clock_khz
            && self.max_hdisplay.is_none_or(|max| mode.hdisplay <= max)
            && self.max_vdisplay.is_none_or(|max| mode.vdisplay <= max)
            && mode.refresh_millihz() >= self.min_refresh_millihz
            && (self.allow_interlace || !mode.is_interlaced())
    }
}

/// Why a mode was chosen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectionReason {
    /// The sink's own preferred timing.
    SinkPreferred,
    /// The timing the firmware already programmed, which the sink lists too.
    FirmwareModeRetained,
    EstablishedTiming,
    StandardTiming,
    DetailedTiming,
    CtaVic,
    /// The sink advertised nothing usable.  Always logged as a warning.
    BuiltinFallback { because: FallbackReason },
}

/// Why the built-in timing had to be used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FallbackReason {
    /// No EDID was supplied, or none of it could be parsed.
    NoEdid,
    /// The sink advertised no timing this kernel can program.
    NoUsableMode,
    /// Every advertised timing was excluded by the constraints.
    ConstraintsExcluded,
}

impl fmt::Display for SelectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectionReason::SinkPreferred => f.write_str("sink preferred timing"),
            SelectionReason::FirmwareModeRetained => f.write_str("firmware mode retained"),
            SelectionReason::EstablishedTiming => f.write_str("established timing"),
            SelectionReason::StandardTiming => f.write_str("standard timing"),
            SelectionReason::DetailedTiming => f.write_str("detailed timing descriptor"),
            SelectionReason::CtaVic => f.write_str("CTA-861 VIC"),
            SelectionReason::BuiltinFallback { because } => match because {
                FallbackReason::NoEdid => f.write_str("built-in fallback: no usable EDID"),
                FallbackReason::NoUsableMode => {
                    f.write_str("built-in fallback: the sink advertises no usable timing")
                }
                FallbackReason::ConstraintsExcluded => {
                    f.write_str("built-in fallback: the constraints exclude every timing")
                }
            },
        }
    }
}

/// The decision, with everything needed to log it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    pub mode: Mode,
    pub reason: SelectionReason,
    /// How many advertised modes were considered.
    pub considered: u8,
    /// How many were excluded by the constraints and the range limits.
    pub excluded: u8,
    /// True when the sink's own range limits had to be set aside to find a
    /// mode; the caller should log this.
    pub range_limits_relaxed: bool,
}

impl fmt::Display for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}, {} considered, {} excluded{})",
            self.mode,
            self.reason,
            self.considered,
            self.excluded,
            if self.range_limits_relaxed {
                ", range limits relaxed"
            } else {
                ""
            }
        )
    }
}

/// The mode to program when there is no EDID to read.
pub const fn fallback(because: FallbackReason) -> Selection {
    Selection {
        mode: FALLBACK_MODE,
        reason: SelectionReason::BuiltinFallback { because },
        considered: 0,
        excluded: 0,
        range_limits_relaxed: false,
    }
}

/// Chooses a mode from an enumeration.
///
/// See the module documentation for the preference order.  The result always
/// names a mode: when nothing the sink advertised can be programmed, the
/// built-in fallback is returned with a reason that says why, and the caller is
/// expected to log it.
pub fn select(edid: &Edid<'_>, candidates: &ModeList, constraints: &Constraints) -> Selection {
    let considered = candidates.len().min(usize::from(u8::MAX)) as u8;
    let limits = edid.range_limits().copied();
    let within_limits = |mode: &Mode| match (constraints.range_check, limits.as_ref()) {
        (RangeCheck::Ignore, _) | (_, None) => true,
        (RangeCheck::Prefer | RangeCheck::Require, Some(limits)) => limits.admits(mode),
    };

    let pick = |relax_limits: bool| -> Option<Selection> {
        let admissible = |candidate: &Candidate| {
            constraints.admits(&candidate.mode)
                && (relax_limits || within_limits(&candidate.mode))
        };
        let admitted = candidates
            .iter()
            .filter(|candidate| admissible(candidate))
            .count();
        let excluded = (candidates.len() - admitted).min(usize::from(u8::MAX)) as u8;
        for class in [
            ModeClass::Preferred,
            ModeClass::Established,
            ModeClass::Standard,
            ModeClass::DetailedTiming,
            ModeClass::CtaVic,
        ] {
            let best = candidates
                .iter()
                .filter(|candidate| candidate.class == class && admissible(candidate))
                .fold(None::<&Candidate>, |best, candidate| match best {
                    Some(current) if !better(candidate, current) => Some(current),
                    _ => Some(candidate),
                });
            let Some(best) = best else { continue };
            // The firmware mode wins inside whichever class it appears in: it
            // is a timing this panel is known to accept.
            if let Some(firmware) = constraints.firmware_mode
                && let Some(known) = candidates.iter().find(|candidate| {
                    candidate.class == class
                        && admissible(candidate)
                        && candidate.mode.same_timing(&firmware)
                })
            {
                return Some(Selection {
                    mode: known.mode,
                    reason: SelectionReason::FirmwareModeRetained,
                    considered,
                    excluded,
                    range_limits_relaxed: relax_limits,
                });
            }
            let reason = match class {
                ModeClass::Preferred => SelectionReason::SinkPreferred,
                ModeClass::Established => SelectionReason::EstablishedTiming,
                ModeClass::Standard => SelectionReason::StandardTiming,
                ModeClass::DetailedTiming => SelectionReason::DetailedTiming,
                ModeClass::CtaVic => SelectionReason::CtaVic,
            };
            return Some(Selection {
                mode: best.mode,
                reason,
                considered,
                excluded,
                range_limits_relaxed: relax_limits,
            });
        }
        None
    };

    if let Some(selection) = pick(false) {
        return selection;
    }
    // Relaxing the sink's own range limits is a decision, and the Selection
    // says so rather than hiding it.
    if constraints.range_check == RangeCheck::Prefer
        && let Some(selection) = pick(true)
    {
        return selection;
    }
    let link_admits_anything = candidates
        .iter()
        .any(|candidate| constraints.admits(&candidate.mode));
    let because = if !candidates.is_empty() && link_admits_anything {
        FallbackReason::ConstraintsExcluded
    } else {
        FallbackReason::NoUsableMode
    };
    Selection {
        mode: FALLBACK_MODE,
        reason: SelectionReason::BuiltinFallback { because },
        considered,
        excluded: considered,
        range_limits_relaxed: false,
    }
}

/// Total order over modes within a preference class.
fn better(candidate: &Candidate, current: &Candidate) -> bool {
    let (a, b) = (candidate.mode, current.mode);
    (
        a.active_area(),
        a.refresh_millihz(),
        core::cmp::Reverse(a.clock_khz),
        core::cmp::Reverse(a.htotal),
    ) > (
        b.active_area(),
        b.refresh_millihz(),
        core::cmp::Reverse(b.clock_khz),
        core::cmp::Reverse(b.htotal),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::modes::{
        edid::Edid,
        fixtures::{BaseBlockBuilder, CtaBlockBuilder, DetailedTimingSpec, RangeLimitsSpec, assemble},
    };
    use alloc::vec::Vec;

    /// 1920x1080 at 60 Hz, the shape a modern panel's preferred descriptor has.
    const PREFERRED: DetailedTimingSpec = DetailedTimingSpec::new(148_500, 1920, 1080);

    /// 1280x720 at 60 Hz, with exactly the numbers CTA-861 VIC 4 publishes.
    const VIC4_TIMING: DetailedTimingSpec = DetailedTimingSpec {
        clock_khz: 74_250,
        hdisplay: 1280,
        hblank: 370,
        hfront: 110,
        hsync: 40,
        vdisplay: 720,
        vblank: 30,
        vfront: 5,
        vsync: 5,
        hsync_positive: true,
        vsync_positive: true,
        interlaced: false,
        width_mm: 0,
        height_mm: 0,
        hborder: 0,
        vborder: 0,
    };

    fn panel() -> Vec<u8> {
        BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PREFERRED)
            .build()
            .to_vec()
    }

    fn choose(bytes: &[u8], constraints: &Constraints) -> Selection {
        let edid = Edid::parse(bytes).expect("the fixture must parse");
        let mut list = ModeList::new();
        collect_modes(&edid, &mut list);
        select(&edid, &list, constraints)
    }

    #[test]
    fn the_sink_preferred_timing_wins_over_everything_else() {
        // The sink advertises its preferred 1080p descriptor, an established
        // 640x480 bit, a 720p detailed timing and a CTA extension with VICs.
        let base = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PREFERRED)
            .detailed_timing(1, &VIC4_TIMING)
            .established_i_ii([0x20, 0x00, 0x00])
            .extension_count(1)
            .build();
        let cta = CtaBlockBuilder::new(3).vics(&[4, 16]).build();
        let bytes = assemble(base, &[cta]);
        let selection = choose(&bytes, &Constraints::unlimited());
        assert_eq!(selection.reason, SelectionReason::SinkPreferred);
        assert_eq!(selection.mode.hdisplay, 1920);
        assert_eq!(selection.mode.clock_khz, 148_500);
        assert!(selection.considered >= 4);
        assert_eq!(selection.excluded, 0);
    }

    #[test]
    fn a_firmware_mode_the_sink_lists_is_retained() {
        // What the firmware programmed: the same timing, from a different
        // authority.
        let firmware_mode = Mode {
            source: TimingSource::Firmware,
            ..PREFERRED_MODE
        };
        let constraints = Constraints {
            firmware_mode: Some(firmware_mode),
            ..Constraints::unlimited()
        };
        let selection = choose(&panel(), &constraints);
        assert_eq!(selection.reason, SelectionReason::FirmwareModeRetained);
        assert_eq!(selection.mode.hdisplay, 1920);
        // The retained timing keeps the provenance of the row that named it.
        assert_eq!(selection.mode.source, TimingSource::EdidDtd { index: 0 });
    }

    #[test]
    fn the_preferred_timing_outranks_a_firmware_mode_it_does_not_match() {
        // The firmware programmed 720p; the sink also advertises it, but its
        // own preferred timing is 1080p.  The preferred timing wins, and the
        // reason says so.
        let base = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PREFERRED)
            .detailed_timing(1, &VIC4_TIMING)
            .build();
        let constraints = Constraints {
            firmware_mode: Some(vic::by_vic(4).expect("VIC 4").mode),
            ..Constraints::unlimited()
        };
        let selection = choose(&base, &constraints);
        assert_eq!(selection.reason, SelectionReason::SinkPreferred);
        assert_eq!(selection.mode.hdisplay, 1920);
    }

    #[test]
    fn a_constraint_ceiling_promotes_the_next_class() {
        // The link cannot carry 1080p's 148.5 MHz; the sink's established
        // 1024x768@60 bit is the next best thing it advertises.
        let base = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PREFERRED)
            .established_i_ii([0x00, 0x08, 0x00])
            .build();
        let constraints = Constraints {
            max_clock_khz: 100_000,
            ..Constraints::unlimited()
        };
        let selection = choose(&base, &constraints);
        assert_eq!(selection.reason, SelectionReason::EstablishedTiming);
        assert_eq!(selection.mode.hdisplay, 1024);
        assert_eq!(selection.mode.vdisplay, 768);
        assert_eq!(selection.mode.clock_khz, 65_000);
        assert_eq!(selection.mode.source, TimingSource::Dmt(0x10));
        assert_eq!(selection.excluded, 1);
    }

    #[test]
    fn a_standard_timing_resolves_to_the_exact_dmt_row() {
        // A sink that advertises only the standard timing code 0x3140, which
        // is 640x480@60.  The blanking must be DMT's (160/45), not a formula's.
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .standard_timing_code(0, [0x31, 0x40])
            .build()
            .to_vec();
        let selection = choose(&bytes, &Constraints::unlimited());
        assert_eq!(selection.reason, SelectionReason::StandardTiming);
        assert_eq!(selection.mode.clock_khz, 25_175);
        assert_eq!(selection.mode.htotal, 800);
        assert_eq!(selection.mode.vtotal, 525);
        assert_eq!(selection.mode.hsync_start, 656);
        assert_eq!(selection.mode.hsync_end, 752);
        assert_eq!(selection.mode.source, TimingSource::Dmt(0x04));
    }

    #[test]
    fn a_standard_timing_with_no_dmt_row_is_generated_with_cvt() {
        // 1368x769 at 60 Hz is what the code 0x8CC0 stands for (1368 = 8 *
        // (0x8c + 31), 16:9, 60 Hz) and no DMT row has that size, so the
        // timing has to be computed.  Reduced blanking is the default when the
        // sink states no preference.
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .standard_timing_code(0, [0x8c, 0xc0])
            .build()
            .to_vec();
        let selection = choose(&bytes, &Constraints::unlimited());
        assert_eq!(selection.reason, SelectionReason::StandardTiming);
        assert_eq!(selection.mode.clock_khz, 72_500);
        assert_eq!(selection.mode.htotal, 1528);
        assert_eq!(selection.mode.vtotal, 791);
        assert_eq!(selection.mode.refresh_hz_rounded(), 60);
        assert_eq!(
            selection.mode.source,
            TimingSource::Cvt {
                reduced_blanking: true
            }
        );
    }

    #[test]
    fn the_sinks_cvt_range_descriptor_chooses_the_blanking() {
        // The same code, but the range descriptor says the sink does not do
        // reduced blanking, so the standard-blanking timing is generated.
        let spec = RangeLimitsSpec {
            cvt_flags: 0x00, // neither standard nor reduced blanking declared
            ..RangeLimitsSpec::cvt_panel()
        };
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .standard_timing_code(0, [0x8c, 0xc0])
            .range_limits(1, &spec)
            .build()
            .to_vec();
        let selection = choose(&bytes, &Constraints::unlimited());
        assert_eq!(selection.mode.clock_khz, 85_250);
        assert_eq!(selection.mode.htotal, 1784);
        assert_eq!(selection.mode.vtotal, 799);
        assert_eq!(
            selection.mode.source,
            TimingSource::Cvt {
                reduced_blanking: false
            }
        );
    }

    #[test]
    fn cta_vics_are_the_last_advertised_source() {
        // Nothing but video identification codes: 720p and 1080p.
        let base = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .extension_count(1)
            .build();
        let cta = CtaBlockBuilder::new(3).vics(&[4, 16]).build();
        let bytes = assemble(base, &[cta]);
        let selection = choose(&bytes, &Constraints::unlimited());
        assert_eq!(selection.reason, SelectionReason::CtaVic);
        // The largest area wins inside the class.
        assert_eq!(selection.mode.hdisplay, 1920);
        assert_eq!(selection.mode.source, TimingSource::CtaVic(16));
        assert_eq!(selection.mode.clock_khz, 148_500);
    }

    #[test]
    fn interlaced_modes_are_opt_in() {
        let base = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .extension_count(1)
            .build();
        let cta = CtaBlockBuilder::new(3).vics(&[5]).build();
        let bytes = assemble(base, &[cta]);
        let refused = choose(&bytes, &Constraints::unlimited());
        assert!(matches!(
            refused.reason,
            SelectionReason::BuiltinFallback {
                because: FallbackReason::NoUsableMode
            }
        ));
        assert_eq!(refused.mode, FALLBACK_MODE);
        let allowed = choose(
            &bytes,
            &Constraints {
                allow_interlace: true,
                ..Constraints::unlimited()
            },
        );
        assert_eq!(allowed.reason, SelectionReason::CtaVic);
        assert_eq!(allowed.mode.vtotal, 1125);
        assert!(allowed.mode.is_interlaced());
    }

    #[test]
    fn range_limits_are_preferred_and_their_relaxation_is_reported() {
        // The panel's own range caps the clock at 100 MHz, which excludes its
        // preferred 1080p timing.  Preferring the range would leave nothing, so
        // the limits are set aside and the selection says so.
        let spec = RangeLimitsSpec {
            max_clock_10mhz: 10, // 100 MHz
            ..RangeLimitsSpec::cvt_panel()
        };
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PREFERRED)
            .range_limits(1, &spec)
            .build()
            .to_vec();
        let selection = choose(&bytes, &Constraints::unlimited());
        assert_eq!(selection.reason, SelectionReason::SinkPreferred);
        assert!(selection.range_limits_relaxed);
        assert_eq!(selection.mode.hdisplay, 1920);

        // With the limits enforced there is no acceptable mode at all.
        let required = choose(
            &bytes,
            &Constraints {
                range_check: RangeCheck::Require,
                ..Constraints::unlimited()
            },
        );
        assert!(matches!(
            required.reason,
            SelectionReason::BuiltinFallback {
                because: FallbackReason::ConstraintsExcluded
            }
        ));
        assert!(!required.range_limits_relaxed);

        // Ignoring the range is not the same decision and is not reported as
        // a relaxation.
        let ignored = choose(
            &bytes,
            &Constraints {
                range_check: RangeCheck::Ignore,
                ..Constraints::unlimited()
            },
        );
        assert_eq!(ignored.reason, SelectionReason::SinkPreferred);
        assert!(!ignored.range_limits_relaxed);
    }

    #[test]
    fn a_sink_with_no_timings_falls_back_explicitly() {
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .build()
            .to_vec();
        let selection = choose(&bytes, &Constraints::unlimited());
        assert!(matches!(
            selection.reason,
            SelectionReason::BuiltinFallback {
                because: FallbackReason::NoUsableMode
            }
        ));
        assert_eq!(selection.mode, FALLBACK_MODE);
        assert!(selection.mode.is_well_formed());
        assert_eq!(selection.mode.clock_khz, 25_175);
        assert_eq!(selection.considered, 0);
    }

    #[test]
    fn constraints_that_exclude_everything_are_distinguished_from_no_modes() {
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PREFERRED)
            .build()
            .to_vec();
        let selection = choose(
            &bytes,
            &Constraints {
                max_hdisplay: Some(800),
                ..Constraints::unlimited()
            },
        );
        assert!(matches!(
            selection.reason,
            SelectionReason::BuiltinFallback {
                because: FallbackReason::ConstraintsExcluded
            }
        ));
        assert_eq!(selection.considered, 1);
        assert_eq!(selection.excluded, 1);
    }

    #[test]
    fn legacy_established_timing_bits_without_a_dmt_row_are_counted() {
        // 720x400@70 is bit 7 of the first established byte; DMT 1.13 does not
        // define it, so it must be counted as skipped rather than approximated.
        let base = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .established_i_ii([0x80, 0x00, 0x00])
            .detailed_timing(0, &PREFERRED)
            .build();
        let edid = Edid::parse(&base).expect("valid");
        let mut list = ModeList::new();
        let report = collect_modes(&edid, &mut list);
        assert_eq!(report.skipped_without_a_table_row, 1);
        assert!(
            !list
                .modes()
                .any(|mode| mode.hdisplay == 720 && mode.vdisplay == 400)
        );
        assert!(list.modes().any(|mode| mode.hdisplay == 1920));
    }

    #[test]
    fn selection_is_deterministic() {
        let base = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PREFERRED)
            .detailed_timing(1, &VIC4_TIMING)
            .established_i_ii([0x20, 0x00, 0x00])
            .extension_count(1)
            .build();
        let cta = CtaBlockBuilder::new(3).vics(&[4, 16, 31]).build();
        let bytes = assemble(base, &[cta]);
        let first = choose(&bytes, &Constraints::unlimited());
        for _ in 0..8 {
            assert_eq!(choose(&bytes, &Constraints::unlimited()), first);
        }
    }

    #[test]
    fn the_mode_list_dedupes_and_is_bounded() {
        let mut list = ModeList::new();
        assert!(list.is_empty());
        let candidate = Candidate {
            mode: PREFERRED_MODE,
            class: ModeClass::Preferred,
            native: true,
        };
        assert!(list.push(candidate));
        assert!(!list.push(candidate), "an identical timing is a duplicate");
        assert_eq!(list.len(), 1);
        assert!(list.contains_timing(&PREFERRED_MODE));
        // Fill the list, then show that it refuses to grow.
        for index in 0..MAX_MODES {
            let mode = Mode {
                htotal: PREFERRED_MODE.htotal + index as u16 + 1,
                ..PREFERRED_MODE
            };
            list.push(Candidate {
                mode,
                class: ModeClass::DetailedTiming,
                native: false,
            });
        }
        assert!(list.is_full());
        assert_eq!(list.len(), MAX_MODES);
        assert!(!list.push(Candidate {
            mode: Mode {
                htotal: 9999,
                ..PREFERRED_MODE
            },
            class: ModeClass::CtaVic,
            native: false,
        }));
        assert!(list.get(MAX_MODES).is_none());
        assert_eq!(list.iter().count(), MAX_MODES);
    }

    /// The 1920x1080@60 timing the fixtures encode, as a `Mode`.
    const PREFERRED_MODE: Mode = Mode::from_blanking(
        148_500,
        1920,
        280,
        88,
        44,
        1080,
        45,
        4,
        5,
        ModeFlags::NONE,
        TimingSource::EdidDtd { index: 0 },
    );
}
