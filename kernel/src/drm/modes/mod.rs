//! Display modes: the timing representation and the published timing tables.
//!
//! [`mode`] is the timing shape a display engine is programmed with: pixel
//! clock, the four horizontal and four vertical edges, sync polarities, and
//! the interlace and doubled-clock flags.  Everything else in this module tree
//! produces [`Mode`] values from one of three published sources:
//!
//! * [`dmt`] - VESA Display Monitor Timing 1.13, the standard's own table;
//! * [`vic`] - CTA-861 video identification codes, which is how an HDMI sink
//!   names a format;
//! * [`cvt`] - the VESA Coordinated Video Timings formulas, including reduced
//!   blanking, for sinks that name a size and a rate but no timing.
//!
//! Each mode records where its numbers came from ([`TimingSource`]), because a
//! pixel clock that is off by one percent is a blank screen and a mode that was
//! computed must be distinguishable from one a monitor pinned.
//!
//! [`edid`] parses the bytes a monitor sends down a DDC channel - the base
//! block and its extension blocks - into the facts those tables need.  It is
//! allocation-free, borrows the caller's buffer, and reports a damaged block as
//! a typed error rather than accepting it silently.  The selection policy that
//! chooses among the resulting modes lives in the sibling module `select`.

mod cvt;
mod dmt;
mod edid;
mod mode;
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
pub use vic::{CTA_VIC_TIMINGS, MAX_VIC, VicTiming};
