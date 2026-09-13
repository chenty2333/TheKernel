//! Hotplug detect: turning it on, reading the live connect state, and the
//! polarity bit that explains a status which never changes.
//!
//! Hotplug is how a display driver learns that a monitor exists, and the
//! reference's advice (§9.4, quoting the PRM's "Interrupts and Hot Plug") is
//! the procedure this module implements:
//!
//! > *"To find if a receiver was connected before hotplug was enabled, enable
//! > hotplug in SHOTPLUG_CTL and then read the interrupt ISR to find the live
//! > connect state."*
//!
//! So: enable detection, then read the status register once, immediately.
//! Waiting for an interrupt is the wrong first move -- the monitor may have
//! been plugged in before the kernel started, in which case no edge will ever
//! arrive -- and taking an interrupt is not this workstream's work in any case.
//! [`enable_and_read`] is that whole procedure, and it reports the raw values
//! it saw alongside what it concluded, because on a machine with no serial port
//! the register dump *is* the diagnostic.
//!
//! # Registers
//!
//! | Register | Address | What it does |
//! |---|---|---|
//! | `SHOTPLUG_CTL_DDI` | `0xC4030` | per-DDI enable, output data and a latched detect field |
//! | `SDEISR` | `0xC4000` | the live connect state, one bit per pin |
//! | `SOUTH_CHICKEN1` | `0xC2000` | board hotplug inversion, one bit per DDI at [18:15] |
//! | `SHPD_FILTER_CNT` | `0xC4038` | the pulse filter, read for the report only |
//!
//! `SHOTPLUG_CTL_DDI` is four bits per DDI, indexed by `_HPD_PIN_DDI(hpd_pin) =
//! hpd_pin - HPD_PORT_A` -- which is the DDI's own number, 0 for A ([I915]
//! `i915_reg.h:2543`; `enum hpd_pin` in `display/intel_display_limits.h:110-129`
//! makes `HPD_PORT_A` the fourth enumerator because `HPD_TV` aliases
//! `HPD_NONE`, so the subtraction is what produces 0, not the enumerator's
//! value).  The live state is `SDE_DDI_HOTPLUG_ICP(hpd_pin) =
//! REG_BIT(16 + _HPD_PIN_DDI(hpd_pin))` ([I915] `i915_reg.h:3001`): DDI A is
//! `SDEISR` bit 16, DDI B bit 17, and so on.
//!
//! # The one place this module does less than the reference says
//!
//! Reference §9.5 says to "Program `HPD_LONG_DETECT` (2) unless you have a
//! reason to want both", describing the two-bit field per DDI in
//! `SHOTPLUG_CTL_DDI` whose values are no-detect, short, long and both.  i915
//! never writes that field: it writes only `HPD_ENABLE`, and *reads* the field
//! to classify a hotplug event as a long pulse (a real connect) or a short one
//! ([I915] `display/intel_hotplug_irq.c:243-254`, `:769-780` and `:564-572`,
//! where the whole register is read with a no-op read-modify-write and handed
//! to `icp_ddi_port_hotplug_long_detect`).  A field that is read to find out
//! what happened is a status latch, and writing to it would overwrite the
//! evidence.  This module therefore enables detection the way i915 does, and
//! reports the latched field instead of programming it -- see the design note
//! `docs/design/intel-gmbus.md`.
//!
//! # Not verified
//!
//! Nothing here has run against real hardware.  The bit positions come from
//! `[I915]` and from reference §9.5, and the tests below check the arithmetic
//! against those tables; whether an Alder Lake-N part latches a connect the way
//! this module assumes is exactly what the target machine is for.

use alloc::{format, string::String};
use core::fmt;

use super::{
    gmbus::Pin,
    regs::{Register, Registers, SDEISR, SHOTPLUG_CTL_DDI, SHPD_FILTER_CNT, SOUTH_CHICKEN1},
};

/// One DDI: the digital display interface a connector hangs off.
///
/// Only A through D are modelled, because that is what `SHOTPLUG_CTL_DDI` and
/// the `SDE_DDI_HOTPLUG_ICP` bits cover.  A Type-C port's hotplug lives in a
/// different pair of registers -- `SHOTPLUG_CTL_TC` (`0xC4034`) and
/// `SDE_TC_HOTPLUG_ICP` = `REG_BIT(24 + _HPD_PIN_TC(hpd_pin))` ([I915]
/// `i915_reg.h:2999`, `:3087`) -- and reaching a monitor on one needs the DKL
/// PHY, which reference §8.8 defers.  Sharing the DDI type between the two
/// would suggest this module drives both; it does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Ddi {
    A,
    B,
    C,
    D,
}

impl Ddi {
    /// Every DDI, in register order.
    pub(crate) const ALL: [Ddi; 4] = [Ddi::A, Ddi::B, Ddi::C, Ddi::D];

    /// The DDI's number, which is also its four-bit field index in
    /// `SHOTPLUG_CTL_DDI` (`_HPD_PIN_DDI`, [I915] `i915_reg.h:2543`).
    pub(crate) const fn index(self) -> u32 {
        match self {
            Self::A => 0,
            Self::B => 1,
            Self::C => 2,
            Self::D => 3,
        }
    }

    /// The name a person reads.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
        }
    }

    /// `HPD_ENABLE`: `0x8 << (idx*4)` ([I915] `i915_reg.h:3079`).
    pub(crate) const fn enable_bit(self) -> u32 {
        0x8 << (self.index() * 4)
    }

    /// The two-bit detect field: `0x3 << (idx*4)` ([I915] `i915_reg.h:3081`).
    ///
    /// Values are 0 no detect, 1 short, 2 long, 3 both (`i915_reg.h:3082-3085`).
    /// A long pulse is a real connect; a short one is a pulse.
    pub(crate) const fn detect_field(self) -> u32 {
        0x3 << (self.index() * 4)
    }

    /// `HPD_OUTPUT_DATA`: `0x4 << (idx*4)`.  Drives the pin.  Never written
    /// here: driving a hotplug pin is a test fixture's business, not a
    /// driver's.
    pub(crate) const fn output_data_bit(self) -> u32 {
        0x4 << (self.index() * 4)
    }

    /// `SDE_DDI_HOTPLUG_ICP`: the live connect state, `SDEISR` bit
    /// `16 + idx` ([I915] `i915_reg.h:3001`).
    pub(crate) const fn live_bit(self) -> u32 {
        1 << (16 + self.index())
    }

    /// `INVERT_DDIA_HPD`..`INVERT_DDID_HPD`: board inversion, `SOUTH_CHICKEN1`
    /// bit `15 + idx` ([I915] `i915_reg.h:3367-3370`).
    ///
    /// The reference records the register as `[GAP]` (item 10 of §13.1: "I did
    /// not identify what `0xC2000` is named in i915") and records the PRM's
    /// requirement as "bits [18:15] must be set to `1111b`" (§9.5).  Both
    /// halves are now sourced: `0xC2000` is `SOUTH_CHICKEN1`, and the field is
    /// one bit per DDI, so the PRM's `1111b` is "invert all four" written as a
    /// single value.  The bit-per-DDI form is what makes it possible to invert
    /// the one port a board actually level-shifts.
    pub(crate) const fn invert_bit(self) -> u32 {
        1 << (15 + self.index())
    }

    /// The GMBUS pin pair that carries this DDI's DDC channel, when there is
    /// one.
    ///
    /// DDI D has hotplug but no DDC pin this kernel can select: the ICP pin
    /// table stops at index 3 for DDI pins ([I915] `display/intel_gmbus.c:
    /// 113-124`; see [`Pin`]'s documentation).  Returning `None` there is the
    /// honest answer, and it is why hotplug and DDC are two types rather than
    /// one.
    pub(crate) const fn pin(self) -> Option<Pin> {
        match self {
            Self::A => Some(Pin::DdiA),
            Self::B => Some(Pin::DdiB),
            Self::C => Some(Pin::DdiC),
            Self::D => None,
        }
    }
}

impl fmt::Display for Ddi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DDI {}", self.name())
    }
}

/// Why hotplug detection could not be configured or read.
///
/// Two variants, because there are exactly two ways the register window can
/// refuse: it does not reach the register, or the register table says the
/// register may not be touched that way.  Both are bugs in this kernel rather
/// than hardware conditions, and both say which register so the bug is a
/// one-line fix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HpdError {
    /// The mapped window does not cover the register.
    WindowTooSmall { register: &'static str },
    /// The register window refused a write the table declares read-only.
    RegisterRefused { register: &'static str },
}

impl HpdError {
    /// One line for a log.
    pub(crate) fn describe(self) -> String {
        match self {
            Self::WindowTooSmall { register } => format!(
                "the mapped register window does not reach {register}, so hotplug detection was \
                 not touched"
            ),
            Self::RegisterRefused { register } => format!(
                "{register} is declared read-only, which is a bug in this kernel's register table"
            ),
        }
    }
}

impl fmt::Display for HpdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Everything one `enable_and_read` observed, values as well as conclusions.
///
/// The raw words are kept because the target machine cannot be interrogated
/// with a debugger: whatever this reports is what somebody has to reason from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HpdStatus {
    pub(crate) ddi: Ddi,
    /// `SHOTPLUG_CTL_DDI` as read *before* anything was written: the state the
    /// firmware left, including the detect field it latched.
    pub(crate) control_before: u32,
    /// `SHOTPLUG_CTL_DDI` as read back after the enable.
    pub(crate) control_after: u32,
    /// `SDEISR` as read once, immediately after the enable.
    pub(crate) interrupt_status: u32,
    /// `SOUTH_CHICKEN1` as read.
    pub(crate) south_chicken1: u32,
    /// `SHPD_FILTER_CNT` as read, when the window reaches it.
    pub(crate) filter: Option<u32>,
    /// Whether `HPD_ENABLE` reads back set for this DDI.
    pub(crate) enabled: bool,
    /// Whether the live connect state bit is set.
    pub(crate) connected: bool,
    /// Whether this DDI's board-inversion bit is set.
    pub(crate) polarity_inverted: bool,
}

impl HpdStatus {
    /// The latched detect field as the reference names its values.
    ///
    /// `[I915]` `i915_reg.h:3082-3085`: 0 no detect, 1 short, 2 long, 3 both.
    /// This was latched before this kernel touched the register, so it is a
    /// fact about the firmware's boot, not about this driver.
    pub(crate) const fn detect_field_name(&self) -> &'static str {
        match (self.control_before & self.ddi.detect_field()) >> (self.ddi.index() * 4) {
            0 => "no detect",
            1 => "short pulse",
            2 => "long pulse (a connect)",
            _ => "short and long",
        }
    }

    /// One line for a log, complete enough to act on.
    pub(crate) fn describe(&self) -> String {
        let live = match (self.connected, self.polarity_inverted) {
            (true, false) => "the live connect bit is set: a sink is connected",
            (false, false) => {
                "the live connect bit is clear: nothing is connected, or detection is not yet \
                 seeing it"
            }
            // With the board's level shifter inverting hotplug, a clear bit is
            // the connected state.  Both readings are stated rather than
            // picked, because this kernel has no way to tell which board it is
            // on: the reference records the PRM's inversion note and could not
            // confirm which boards it applies to (§9.5, and item 4 of §13.2).
            (true, true) => {
                "the live connect bit is set and this DDI's board-inversion bit is also set, so \
                 the state is inverted: the bit being set means nothing is connected"
            }
            (false, true) => {
                "the live connect bit is clear and this DDI's board-inversion bit is set, so the \
                 state is inverted: the bit being clear means a sink is connected"
            }
        };
        format!(
            "{}: HPD {} (SHOTPLUG_CTL_DDI {:#010x} -> {:#010x}, latched detect field: {}), {}; \
             SDEISR {:#010x}, SOUTH_CHICKEN1 {:#010x}{}",
            self.ddi,
            if self.enabled {
                "enabled"
            } else {
                "NOT enabled after the write"
            },
            self.control_before,
            self.control_after,
            self.detect_field_name(),
            live,
            self.interrupt_status,
            self.south_chicken1,
            match self.filter {
                Some(filter) => format!(", SHPD_FILTER_CNT {filter:#010x}"),
                None => String::new(),
            }
        )
    }

    /// Say it on the console.
    pub(crate) fn log(&self) {
        axlog::info!("intel-hpd: {}", self.describe());
    }
}

/// Enable hotplug detection for one DDI and read the live state once.
///
/// This is the PRM's procedure (reference §9.4) and the whole of this module's
/// useful behaviour:
///
/// 1. Read `SHOTPLUG_CTL_DDI` and keep it.  The per-DDI two-bit field latches
///    the last detected pulse type, so what is there before the first write is
///    a fact about the firmware's boot.
/// 2. Set only this DDI's `HPD_ENABLE` bit, leaving every other DDI's bits
///    exactly as they were.  A read-modify-write rather than a write, because
///    three other ports share the register.  Unlike the reference's §9.5
///    advice, the detect field is *not* programmed: i915 reads that field to
///    classify pulse length, so it is a status latch (see the module
///    documentation).
/// 3. Read the register back.  MMIO writes are posted and there is no
///    completion for a store, so a read-back is the only evidence the write
///    arrived (reference §2.2).
/// 4. Read `SDEISR` once, which is where the live connect state is.
/// 5. Read `SOUTH_CHICKEN1`, because "the bit never sets" is the symptom the
///    reference gives for missing board inversion (§11 phase 2.2), and a single
///    status line that omits the polarity bit sends the reader back for a
///    second look.
///
/// Interrupts are deliberately not enabled: `SDEIER` is not written, so this
/// only configures the detect logic and reads its result.
pub(crate) fn enable_and_read<R: Registers>(regs: &R, ddi: Ddi) -> Result<HpdStatus, HpdError> {
    let control_before = read(regs, SHOTPLUG_CTL_DDI)?;
    let enable = ddi.enable_bit();
    // `HPD_ENABLE` is the top bit of this DDI's four, so setting it leaves the
    // latched detect field and the output-data bit of this DDI -- and every bit
    // of the other three DDIs -- exactly as they were.  The read-modify-write
    // is not decoration: three other ports share this register.
    let control_after = control_before | enable;
    write(regs, SHOTPLUG_CTL_DDI, control_after)?;

    // The read-back is the ordering primitive, not a formality.
    let observed = read(regs, SHOTPLUG_CTL_DDI)?;
    let interrupt_status = read(regs, SDEISR)?;
    let south_chicken1 = read(regs, SOUTH_CHICKEN1)?;
    let filter = regs.read(SHPD_FILTER_CNT);

    Ok(HpdStatus {
        ddi,
        control_before,
        control_after: observed,
        interrupt_status,
        south_chicken1,
        filter,
        enabled: observed & enable != 0,
        connected: interrupt_status & ddi.live_bit() != 0,
        polarity_inverted: south_chicken1 & ddi.invert_bit() != 0,
    })
}

/// Read the live connect state of one DDI without changing anything.
///
/// The same read [`enable_and_read`] finishes with, for a caller that has
/// already enabled detection and wants to look again -- after a hotplug
/// interrupt was noticed, or later in a boot.
pub(crate) fn live_state<R: Registers>(regs: &R, ddi: Ddi) -> Result<bool, HpdError> {
    let status = read(regs, SDEISR)?;
    Ok(status & ddi.live_bit() != 0)
}

/// Whether this DDI's board level shifter inverts hotplug.
///
/// Reference §9.5: the PRM warns that the hotplug level shifter on a board can
/// invert the signal, so that connect reads as 0 and disconnect as 1, and says
/// `0xC2000[18:15]` must be `1111b` to account for it.  That register is
/// `SOUTH_CHICKEN1` and those bits are one per DDI ([I915] `i915_reg.h:
/// 3358-3370`).  It is the first thing to check when a status bit never
/// changes, which is why it is reported rather than acted on.
pub(crate) fn polarity_inverted<R: Registers>(regs: &R, ddi: Ddi) -> Result<bool, HpdError> {
    let chicken = read(regs, SOUTH_CHICKEN1)?;
    Ok(chicken & ddi.invert_bit() != 0)
}

/// Set or clear one DDI's board-inversion bit.
///
/// **Nothing calls this on its own.**  i915 applies the inversion only for DG1
/// boards (`dg1_hpd_invert`, [I915] `display/intel_hotplug_irq.c:883-904`, and
/// `dg1_hpd_irq_setup` at `:900`), which is the evidence behind the reference's
/// `[INF]` that the PRM's board-inversion note does not apply to ADL-N boards
/// (item 4 of §13.2).  So this is a caller's decision, taken with
/// [`polarity_inverted`] in hand: it is here so that the decision is one line,
/// not so that some later boot path makes it silently.
///
/// Returns `SOUTH_CHICKEN1` as it read back.
pub(crate) fn set_board_inversion<R: Registers>(
    regs: &R,
    ddi: Ddi,
    inverted: bool,
) -> Result<u32, HpdError> {
    let bit = ddi.invert_bit();
    let chicken = read(regs, SOUTH_CHICKEN1)?;
    let updated = if inverted {
        chicken | bit
    } else {
        chicken & !bit
    };
    write(regs, SOUTH_CHICKEN1, updated)?;
    read(regs, SOUTH_CHICKEN1)
}

fn read<R: Registers>(regs: &R, register: Register) -> Result<u32, HpdError> {
    regs.read(register).ok_or(HpdError::WindowTooSmall {
        register: register.name(),
    })
}

fn write<R: Registers>(regs: &R, register: Register, value: u32) -> Result<(), HpdError> {
    if regs.write(register, value) {
        Ok(())
    } else {
        Err(HpdError::RegisterRefused {
            register: register.name(),
        })
    }
}

#[cfg(test)]
mod tests;
