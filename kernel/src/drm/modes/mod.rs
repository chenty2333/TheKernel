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
//! The parser that reads these tables' inputs out of an EDID, and the
//! selection policy that chooses among them, live in the sibling modules
//! `edid` and `select`.

mod cvt;
mod dmt;
mod mode;
mod vic;

pub use cvt::{CvtBlanking, CvtRequest, generate as generate_cvt};
pub use dmt::{DMT_TIMINGS, DmtTiming};
pub use mode::{Mode, ModeFlags, TimingSource};
pub use vic::{CTA_VIC_TIMINGS, MAX_VIC, VicTiming};
