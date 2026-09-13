//! Display modes: EDID bytes in, a programmable timing out.
//!
//! This module is the part of a modeset that needs no hardware.  It answers
//! exactly one question for a display driver: *given what this monitor says
//! about itself, which timing do I program, and why that one?*
//!
//! # The interface
//!
//! A driver that has read an EDID block (through DDC, a firmware table, or a
//! debug endpoint) calls one function:
//!
//! ```
//! # use thekernel_kernel::drm::modes::{Constraints, plan_modeset};
//! # fn main() {
//! # let edid_bytes: &[u8] = &[];
//! let plan = plan_modeset(edid_bytes, &Constraints::unlimited());
//! // `plan.selection.mode` holds the pixel clock, the eight edge values, the
//! // sync polarities and the interlace flag to program.
//! # }
//! ```
//!
//! [`plan_modeset`] always returns a mode.  When the EDID is unreadable, or
//! advertises nothing this kernel can program, or everything it advertises is
//! excluded by the caller's constraints, the plan carries the built-in
//! fallback timing and a [`SelectionReason`] that says which of those
//! happened.  A driver on a machine whose only output is its screen must never
//! be left without a timing, but it must also never be left guessing: call
//! [`log_plan`] and the decision appears in the boot log.
//!
//! Drivers that want to make their own decision use the pieces directly:
//!
//! ```
//! # use thekernel_kernel::drm::modes::{Constraints, ModeList, collect_modes, select, Edid};
//! # fn main() -> Result<(), thekernel_kernel::drm::modes::EdidError> {
//! # let edid_bytes: &[u8] = &[];
//! let edid = Edid::parse(edid_bytes)?;
//! let mut modes = ModeList::new();
//! let report = collect_modes(&edid, &mut modes);
//! let choice = select(&edid, &modes, &Constraints::unlimited());
//! # let _ = (report, choice);
//! # Ok(())
//! # }
//! ```
//!
//! # What is where
//!
//! * [`edid`] parses the base block and the extension blocks.  It is
//!   allocation-free and borrows the caller's buffer.
//! * [`mode`] is the timing representation a display engine is programmed
//!   with.
//! * [`dmt`], [`vic`] and [`cvt`] produce modes from the three published
//!   sources: VESA DMT 1.13, CTA-861 video identification codes, and the VESA
//!   CVT formulas.
//! * [`select`] enumerates and chooses.  See its module documentation for the
//!   preference order.
//!
//! # Why nothing here allocates
//!
//! Enumeration happens during display bring-up, before it is known that the
//! heap is usable, and the input arrives over a cable.  The parser borrows the
//! caller's buffer, [`ModeList`] is a fixed array of [`MAX_MODES`] entries,
//! and every loop is bounded by the block structure rather than by a length
//! read out of the data.  The only cost is a documented truncation when a sink
//! advertises more modes than the list holds; [`plan_modeset`] reports it.

mod cvt;
mod dmt;
mod edid;
mod mode;
mod select;
mod vic;

#[cfg(test)]
mod fixtures;

pub use cvt::{CvtBlanking, CvtRequest, generate as generate_cvt};
pub use dmt::{DMT_TIMINGS, DmtTiming};
pub use edid::{
    AspectRatio, Chromaticity, ColorPoint, Cta861, CtaDetailedTimings, CvtAspectRatio,
    CvtRangeLimits, CvtRefreshSupport, CvtTimingCode, CvtTimingCodeBlock, DataBlock, DataBlockTag,
    Descriptor, DetailedTiming, DigitalInput, DigitalInterface, DisplayDescriptorTag, Edid,
    EdidError, EdidText, EdidWarnings, EstablishedTimings, Extension, Features, PnpId, RangeLimits,
    RangeLimitsKind, ScreenSize, SecondaryGtf, ShortVideoDescriptor, StandardTiming, StereoMode,
    SyncType, UnknownExtension, VideoInput,
};
pub use mode::{Mode, ModeFlags, TimingSource};
pub use select::{
    Candidate, CollectReport, Constraints, FALLBACK_MODE, FallbackReason, ModeClass, ModeList,
    RangeCheck, Selection, SelectionReason, collect_modes, fallback, select,
};
pub use vic::{CTA_VIC_TIMINGS, MAX_VIC, VicTiming};

/// The most modes one enumeration can hold.
///
/// A sink that advertises more than this (a television with a long CTA-861
/// video data block, for example) is truncated; [`CollectReport::overflowed`]
/// says so.  The list is filled in preference order, so a truncation can only
/// drop modes a selection would have ranked lower than the ones kept.
pub const MAX_MODES: usize = 64;

/// Everything a driver needs to program a mode, plus what it should log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModePlan {
    pub selection: Selection,
    pub report: CollectReport,
    pub warnings: EdidWarnings,
    /// Set when the strict parse failed.  When `strict` is false this is the
    /// error the lenient parse recovered from, and it must be logged.
    pub edid_error: Option<EdidError>,
    /// True when the EDID parsed strictly: no checksum was wrong, no declared
    /// extension block was missing and no descriptor had to be skipped.
    pub strict: bool,
}

impl ModePlan {
    /// True when the sink's own EDID produced the chosen mode.
    pub fn used_edid(&self) -> bool {
        !matches!(
            self.selection.reason,
            SelectionReason::BuiltinFallback { .. }
        )
    }
}

/// Parses an EDID, enumerates its modes, chooses one and logs the decision.
///
/// This is the function a display driver calls.  It never fails: see
/// [`ModePlan`] for what happened.
pub fn plan_modeset(edid_bytes: &[u8], constraints: &Constraints) -> ModePlan {
    let mut list = ModeList::new();
    let plan = match Edid::parse(edid_bytes) {
        Ok(edid) => {
            let report = collect_modes(&edid, &mut list);
            ModePlan {
                selection: select(&edid, &list, constraints),
                report,
                warnings: edid.warnings(),
                edid_error: None,
                strict: true,
            }
        }
        Err(strict_error) => match Edid::parse_lossy(edid_bytes) {
            Ok(edid) => {
                let report = collect_modes(&edid, &mut list);
                ModePlan {
                    selection: select(&edid, &list, constraints),
                    report,
                    warnings: edid.warnings(),
                    edid_error: Some(strict_error),
                    strict: false,
                }
            }
            Err(error) => ModePlan {
                selection: fallback(FallbackReason::NoEdid),
                report: CollectReport::default(),
                warnings: EdidWarnings::default(),
                edid_error: Some(error),
                strict: false,
            },
        },
    };
    log_plan(&plan);
    plan
}

/// Writes the decision to the kernel log.
///
/// Exactly one line describes the chosen mode; anything the EDID needed to
/// skip, and any use of the fallback timing, is a warning.  On a machine whose
/// only output is its screen, this line is how a modeset is diagnosed.
pub fn log_plan(plan: &ModePlan) {
    if let Some(error) = plan.edid_error {
        if plan.strict {
            warn!("drm: EDID rejected: {error}");
        } else {
            warn!("drm: EDID parsed leniently after: {error}");
        }
    }
    if !plan.warnings.is_clean() {
        warn!("drm: EDID warnings: {}", plan.warnings);
    }
    if plan.report.overflowed {
        warn!(
            "drm: more than {MAX_MODES} modes advertised; the list was truncated"
        );
    }
    if plan.report.skipped_without_a_table_row > 0 {
        info!(
            "drm: {} advertised timings have no table row and were skipped",
            plan.report.skipped_without_a_table_row
        );
    }
    if plan.report.skipped_unknown_vic > 0 {
        info!(
            "drm: {} video identification codes are not defined by CTA-861",
            plan.report.skipped_unknown_vic
        );
    }
    match plan.selection.reason {
        SelectionReason::BuiltinFallback { .. } => {
            warn!(
                "drm: no advertised display mode is usable; programming the built-in fallback {}",
                plan.selection
            );
        }
        _ => info!("drm: display mode chosen: {}", plan.selection),
    }
}

/// Reads an EDID and returns the modes it advertises, without choosing.
///
/// Useful for a driver that wants the complete list (for a KMS mode list, for
/// example) rather than a single mode.
pub fn advertised_modes(
    edid_bytes: &[u8],
    list: &mut ModeList,
) -> Result<CollectReport, EdidError> {
    let edid = Edid::parse(edid_bytes)?;
    Ok(collect_modes(&edid, list))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::modes::fixtures::{BaseBlockBuilder, CtaBlockBuilder, DetailedTimingSpec, assemble};

    const PANEL: DetailedTimingSpec = DetailedTimingSpec::new(148_500, 1920, 1080);

    #[test]
    fn the_one_call_interface_produces_a_programmable_mode() {
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PANEL)
            .build();
        let plan = plan_modeset(&bytes, &Constraints::unlimited());
        assert!(plan.strict);
        assert!(plan.edid_error.is_none());
        assert!(plan.warnings.is_clean());
        assert!(plan.used_edid());
        assert_eq!(plan.selection.reason, SelectionReason::SinkPreferred);
        assert_eq!(plan.selection.mode.clock_khz, 148_500);
        assert_eq!(plan.selection.mode.htotal, 2200);
        assert!(plan.selection.mode.is_well_formed());
        assert_eq!(plan.report.added, 1);
    }

    #[test]
    fn a_damaged_edid_falls_back_and_says_why() {
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PANEL)
            .build_with_bad_checksum();
        let plan = plan_modeset(&bytes, &Constraints::unlimited());
        assert!(!plan.strict);
        assert!(matches!(
            plan.edid_error,
            Some(EdidError::BadBaseChecksum { .. })
        ));
        assert!(matches!(
            plan.selection.reason,
            SelectionReason::BuiltinFallback {
                because: FallbackReason::NoEdid
            }
        ));
        assert_eq!(plan.selection.mode, FALLBACK_MODE);
        assert!(plan.selection.mode.is_well_formed());
        assert!(!plan.used_edid());
    }

    #[test]
    fn a_lenient_recovery_still_chooses_a_mode_and_records_the_strict_error() {
        // The base block declares one extension block and the buffer has one,
        // but its checksum is wrong: strict fails, lenient keeps the base
        // block's preferred timing.
        let base = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PANEL)
            .extension_count(1)
            .build();
        let broken = CtaBlockBuilder::new(3).vics(&[4]).build_with_bad_checksum();
        let bytes = assemble(base, &[broken]);
        let plan = plan_modeset(&bytes, &Constraints::unlimited());
        assert!(!plan.strict);
        assert!(matches!(
            plan.edid_error,
            Some(EdidError::BadExtensionChecksum { block: 1, .. })
        ));
        assert_eq!(plan.warnings.bad_extension_checksums, 1);
        assert!(!plan.warnings.is_clean());
        assert_eq!(plan.selection.reason, SelectionReason::SinkPreferred);
        assert_eq!(plan.selection.mode.hdisplay, 1920);
    }

    #[test]
    fn the_advertised_mode_list_is_reported_without_choosing() {
        let bytes = BaseBlockBuilder::new()
            .manufacturer(b"ACR")
            .detailed_timing(0, &PANEL)
            .established_i_ii([0x20, 0x00, 0x00])
            .extension_count(1)
            .build();
        let cta = CtaBlockBuilder::new(3).vics(&[4, 16]).build();
        let bytes = assemble(bytes, &[cta]);
        let mut list = ModeList::new();
        let report = advertised_modes(&bytes, &mut list).expect("a valid EDID");
        assert!(report.added >= 3);
        assert!(!report.overflowed);
        // The preferred timing is first, and the VIC 16 duplicate of it was
        // deduplicated rather than listed twice.
        assert_eq!(list.get(0).expect("a first mode").class, ModeClass::Preferred);
        let duplicates = list
            .iter()
            .filter(|candidate| candidate.mode.hdisplay == 1920)
            .count();
        assert_eq!(duplicates, 1, "1080p is listed once");
        assert_eq!(report.duplicates, 1);
        assert!(list.modes().all(Mode::is_well_formed));
    }
}
