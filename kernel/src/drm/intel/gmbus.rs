//! GMBUS: the I2C master that carries DDC, and therefore EDID.
//!
//! This is the first code in this kernel that produces a fact the firmware did
//! not give it.  Everything before it read registers the firmware had already
//! programmed, or configuration space, which is a standard; here the kernel
//! drives a bus of its own and a monitor on the other end answers.  That is why
//! the failure paths in this module are as long as the happy one.
//!
//! # What the hardware is
//!
//! GMBUS is Intel's I2C controller for the display's DDC channels.  It lives in
//! the south display window, six registers at `0xC5100`-`0xC5120` (reference
//! §9.1; `[I915]` `display/intel_gmbus_regs.h:29-79`).  A transaction is a
//! state machine over those registers: program the pin and the rate, program
//! one command word, then take four bytes at a time out of a data register
//! while the controller raises a ready bit.
//!
//! **Provenance is weaker here than anywhere else in this driver.**  Only
//! `GMBUS0` appears in any public Gen12 register volume; the reference says so
//! outright (§9.2's caveat, recorded again as item 2 of §13.1) after searching
//! the Tiger Lake, DG1 and Rocket Lake volumes.  The field layout of `GMBUS1`
//! through `GMBUS5` and every step of the protocol below therefore rest on
//! `drm/i915` alone, and every constant in this file cites the i915 symbol it
//! came from (`[I915]` is drm/i915 v6.12, the version the reference cites in
//! its §14.2).  Where the reference's prose and that source disagree, the
//! source wins and the disagreement is recorded here.
//!
//! # The protocol, and the one place the reference is out of date
//!
//! Reading an EDID block is one index cycle followed by one read: the
//! controller sends the EEPROM's register address to slave `0x50`, then a
//! repeated start and a read of the block.  i915 v6.12 programs both phases in
//! a *single* `GMBUS1` write -- `CYCLE_INDEX | CYCLE_WAIT`, with the index byte
//! in `SLAVE_INDEX[15:8]` (`[I915]` `display/intel_gmbus.c:596-611` and
//! `:451-452`, unchanged since at least v5.15).  Reference §9.3 describes the
//! older two-write form instead ("*sets* `gmbus1_index = GMBUS_CYCLE_INDEX |
//! (msgs[0].len << 16) | (addr << 1) | SW_RDY` *for the first message*").
//! This driver implements the single-write form, because that is the code path
//! a Gen12 part actually runs, and a test pins the exact command word.
//!
//! # Failure is the interesting part
//!
//! Reference §11.1 is a table of what goes wrong, and the two failure modes
//! worth naming are:
//!
//! * **NAK on every address.**  The reference's explanation, from §11 phase
//!   2.1, is that the AUX/DDC power well for that pin pair is not enabled --
//!   and when a power well is down, writes to its registers are dropped and
//!   reads return zero (§4.2).  A NAK is not a timeout, and this module does
//!   not report it as one: on `GMBUS2.SATOER` it reads the well's state bit
//!   back and returns [`GmbusError::AuxWellDown`], which names the well, or
//!   [`GmbusError::NoAck`], which says the well is up and the sink itself
//!   declined.  Nothing here enables a power well: that is the power
//!   workstream's register to write, and this module only reads it.
//! * **A bus left in a bad state.**  The recovery is the reference's (and
//!   i915's `intel_gmbus_reset`): clear `GMBUS0` and `GMBUS4`, wait for the bus
//!   to go idle, then toggle `GMBUS1.SW_CLR_INT` to reset the controller and
//!   clear the latched error (`[I915]` `intel_gmbus.c:680-708`).
//!
//! # What this module does not do
//!
//! * It does not enable the AUX/DDC power well, and it does not enable hotplug;
//!   [`super::hpd`] owns the latter.
//! * It does not take the GMBUS interrupt.  `SDE_GMBUS_ICP` (`SDEISR` bit 23,
//!   reference §10.5) reports a completed transaction, but taking an interrupt
//!   means wiring the display interrupt path, which this workstream is
//!   deliberately not doing; completion is polled, bounded and read back, and
//!   `GMBUS4` is left cleared so no interrupt is asked for.
//! * It does not bit-bang.  Reference §11.1 gives the GPIO procedure as the
//!   last resort when a reset does not clear a stuck bus; that needs the GPIO
//!   pair registers and is a separate piece of work.
//! * It does not parse EDID.  [`read_edid`] returns the bytes of one validated
//!   128-byte block; the parse belongs to `drm::modes`, which is not in this
//!   branch yet, so this module stops at bytes and says so rather than growing
//!   a second parser.
//!
//! # Not verified
//!
//! No line of this has run against a real display controller or a real monitor.
//! The protocol is tested against a model of the controller (the tests at the
//! bottom of this file drive the state machine through a fake register file),
//! and the register offsets and field masks are traced to the sources cited
//! above.  A test passing here says the state machine does what this file says
//! it does; it says nothing about what an Alder Lake-N part does.

use alloc::{format, string::String, vec::Vec};
use core::fmt;

use super::{
    hpd::Ddi,
    regs::{
        GMBUS0, GMBUS1, GMBUS2, GMBUS3, GMBUS4, GMBUS5, ICL_PWR_WELL_CTL_AUX2, Register,
        RegisterWindow,
    },
};

// ---------------------------------------------------------------------------
// Register fields
// ---------------------------------------------------------------------------

/// `GMBUS0[4:0]`: the pin index.  One-based, and zero means "no pin".
///
/// `[PRM]` "Pin Usage" and `[TGL12]` "GMBUS and GPIO" both describe the field
/// as `GMBUS0[4:0]`; reference §9.2 records the one-based mapping and warns
/// about it in bold because an earlier draft of that document had it wrong.
const GMBUS0_PIN_MASK: u32 = 0x1f;

/// `GMBUS0[9:8]`: the bus rate (`[I915]` `intel_gmbus_regs.h:32-35`).
const GMBUS0_RATE_SHIFT: u32 = 8;
const GMBUS0_RATE_MASK: u32 = 0x3 << GMBUS0_RATE_SHIFT;

/// `GMBUS0[6]`: byte-count override, for burst reads over 511 bytes.
///
/// Declared so that a stray value can be recognised, never set: EDID is read a
/// 128-byte block at a time (reference §9.3).
const GMBUS0_BYTE_CNT_OVERRIDE: u32 = 1 << 6;

/// `GMBUS1[31]`: software clear interrupt.  Toggling it resets the controller
/// and clears a latched bus error (`[I915]` `intel_gmbus_regs.h:41`).
const GMBUS1_SW_CLR_INT: u32 = 1 << 31;

/// `GMBUS1[30]`: software ready.  The bit that starts a programmed transfer.
const GMBUS1_SW_RDY: u32 = 1 << 30;

/// `GMBUS1[27:25]`: the cycle type (`[I915]` `intel_gmbus_regs.h:44-47`).
const GMBUS1_CYCLE_SHIFT: u32 = 25;
const GMBUS1_CYCLE_MASK: u32 = 0x7 << GMBUS1_CYCLE_SHIFT;
/// Let another cycle follow without a stop: the index cycle, and the read that
/// follows it.
const GMBUS1_CYCLE_WAIT: u32 = 1 << GMBUS1_CYCLE_SHIFT;
/// The no-stop index cycle: the byte in `SLAVE_INDEX` is sent to the slave as a
/// write, and the following cycle continues the transaction.
const GMBUS1_CYCLE_INDEX: u32 = 2 << GMBUS1_CYCLE_SHIFT;
/// Generate the stop condition.
const GMBUS1_CYCLE_STOP: u32 = 4 << GMBUS1_CYCLE_SHIFT;

/// `GMBUS1[23:16]`: the byte count.
///
/// i915 writes values up to `GEN9_GMBUS_BYTE_COUNT_MAX` (511) through this
/// field, which needs a ninth bit at 24 that the published field layout does
/// not name (`[I915]` `intel_gmbus_regs.h:48-50`).  This driver reads at most
/// [`MAX_TRANSFER`] bytes, which is below 256, so it never depends on that bit
/// being decoded -- one of the few places where staying inside a published
/// field costs nothing.
const GMBUS1_BYTE_COUNT_SHIFT: u32 = 16;
const GMBUS1_BYTE_COUNT_MASK: u32 = 0xff << GMBUS1_BYTE_COUNT_SHIFT;

/// `GMBUS1[15:8]`: the index byte of an index cycle.
const GMBUS1_SLAVE_INDEX_SHIFT: u32 = 8;
const GMBUS1_SLAVE_INDEX_MASK: u32 = 0xff << GMBUS1_SLAVE_INDEX_SHIFT;

/// `GMBUS1[7:1]`: the slave address, seven bits, left-aligned.
const GMBUS1_SLAVE_ADDR_SHIFT: u32 = 1;
const GMBUS1_SLAVE_ADDR_MASK: u32 = 0x7f << GMBUS1_SLAVE_ADDR_SHIFT;

/// `GMBUS1[0]`: direction.  One is a read, and DDC reads are all this uses.
const GMBUS1_SLAVE_READ: u32 = 1;

/// `GMBUS2` status bits (`[I915]` `intel_gmbus_regs.h:57-64`).
const GMBUS2_INUSE: u32 = 1 << 15;
const GMBUS2_HW_WAIT_PHASE: u32 = 1 << 14;
const GMBUS2_STALL_TIMEOUT: u32 = 1 << 13;
const GMBUS2_INT: u32 = 1 << 12;
const GMBUS2_HW_RDY: u32 = 1 << 11;
const GMBUS2_SATOER: u32 = 1 << 10;
const GMBUS2_ACTIVE: u32 = 1 << 9;

/// Two status bits that end a wait whatever was being waited for: the sink
/// declined the address, or a secondary held the clock too long.
const GMBUS2_TERMINAL: u32 = GMBUS2_SATOER | GMBUS2_STALL_TIMEOUT;

/// `GMBUS5[31]`: two-byte index mode (`[I915]` `intel_gmbus_regs.h:79`).
const GMBUS5_2BYTE_INDEX_EN: u32 = 1 << 31;

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// The DDC address of an EDID EEPROM: 7-bit `0x50`.
///
/// `[PRM]` "DDC" and every implementation agree, and the reference states it in
/// §9.3 and §11 phase 2.3.  The address is a property of DDC, not of a monitor.
pub(crate) const DDC_ADDRESS: u8 = 0x50;

/// The EEPROM byte address of the EDID base block.
pub(crate) const EDID_BASE_OFFSET: u8 = 0x00;

/// The EEPROM byte address of the first extension block.
///
/// An E-EDID block is 128 bytes, so the extension a CTA-861 capable sink
/// advertises sits at `0x80`.  Two blocks are reachable with the one-byte index
/// this driver sends; anything beyond them would need the two-byte index in
/// `GMBUS5`, which a base block plus one extension does not.
pub(crate) const EDID_EXTENSION_OFFSET: u8 = 0x80;

/// The most bytes one transaction reads.
///
/// A `GMBUS1` byte count is one byte, and an EDID block is 128 bytes, so one
/// block per transaction is both the natural unit and comfortably inside every
/// field and timeout involved.  A longer read would need the burst-read
/// override of reference §9.3, which this driver does not implement because
/// nothing in an EDID read needs it.
pub(crate) const MAX_TRANSFER: usize = 128;

/// How long one four-byte word may take to arrive.
///
/// i915 waits two microseconds in a spin and then up to 50 (milliseconds, by
/// the units `intel_wait_for_register` takes) for `GMBUS2.HW_RDY`
/// (`[I915]` `intel_gmbus.c:367-397`; reference §2.3 spells out the units).  At
/// 100 kHz a word takes about 430 microseconds, so 50 ms is over a hundred
/// times the working figure, which is the point: it is a bound on a hung
/// controller, not a performance target.
const READY_TIMEOUT_MICROS: u64 = 50_000;

/// How long the bus may take to go idle after a transaction.
///
/// `[I915]` `gmbus_wait_idle` waits 10 ms (`intel_gmbus.c:414`).
const IDLE_TIMEOUT_MICROS: u64 = 10_000;

/// How long a whole read may take, across every word of it.
///
/// i915 bounds each wait and leaves the total unbounded.  A driver reading an
/// EDID from the boot path cannot: a controller that answers each word just
/// inside `READY_TIMEOUT_MICROS` and then stops would hold the boot for 32
/// times that.  250 ms is about twenty times what a valid 128-byte read costs
/// at 100 kHz -- 128 bytes at nine bits each is 11.5 ms -- so a sink that is
/// merely slow is not mistaken for a hung bus.
const TRANSACTION_TIMEOUT_MICROS: u64 = 250_000;

/// How long to wait between polls of `GMBUS2`.
///
/// `[I915]` `gmbus_wait` spins for two microseconds before it starts sleeping
/// (`intel_gmbus.c:384-388`); this driver never sleeps, so two microseconds is
/// also its poll interval.
const POLL_INTERVAL_MICROS: u64 = 2;

/// How many times one pin is asked for its EDID before the answer is final.
///
/// Two: the reference's recoveries are both "once more" -- a NAK retried, and a
/// block that did not validate re-read at a lower rate (§11.1).  A third
/// attempt would only make a broken port slower to report.
const MAX_ATTEMPTS: u8 = 2;

// ---------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------

/// A GMBUS pin pair: the physical DDC channel an EDID is read from.
///
/// **The pin index is one-based, and zero is not a port.**  `GMBUS0[4:0] = 0`
/// disconnects the controller; DDI A is 1.  Reference §9.2 gives the mapping
/// and flags the off-by-one explicitly, `[I915]` agrees
/// (`GMBUS_PIN_1_BXT = 1`, `display/intel_gmbus.h:23-33`), and the values here
/// are that table's.
///
/// The table that applies to ADL-N is `gmbus_pins_icp` (`[I915]`
/// `display/intel_gmbus.c:113-124`), which the reference records as `[INF]`:
/// i915 selects it for any PCH at or above ICP, and ADL-N reports `PCH_ADP`
/// (item 6 of §13.2).  That table is why there is no DDI D pin here even though
/// DDI D has a hotplug index: `gmbus_pins_icp` has no entry at index 4.  Index
/// 4 is `GMBUS_PIN_4_CNP`, which exists in the CNP, DG1, DG2 and MTP tables and
/// not in the ICP one -- so reference §9.2's row "4 = DDI D" comes from a
/// different platform's table and must not be used on this part.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Pin {
    /// `GMBUS_PIN_1_BXT`, DDC for DDI A, on `GPIOB`.
    DdiA,
    /// `GMBUS_PIN_2_BXT`, DDC for DDI B, on `GPIOC`.
    DdiB,
    /// `GMBUS_PIN_3_BXT`, DDC for DDI C, on `GPIOD`.
    DdiC,
    /// `GMBUS_PIN_9_TC1_ICP`, DDC for Type-C port 1, on `GPIOJ`.
    Tc1,
    /// `GMBUS_PIN_10_TC2_ICP`, DDC for Type-C port 2, on `GPIOK`.
    Tc2,
    /// `GMBUS_PIN_11_TC3_ICP`, DDC for Type-C port 3, on `GPIOL`.
    Tc3,
    /// `GMBUS_PIN_12_TC4_ICP`, DDC for Type-C port 4, on `GPIOM`.
    Tc4,
}

impl Pin {
    /// The DDC pins a monitor's EDID is looked for on, in the order the
    /// reference recommends: DDI A, then DDI B, then DDI C.
    ///
    /// §11 phase 2.3: "a monitor's EDID will appear on exactly one of them and
    /// that identifies your physical port".  The Type-C pins are selectable and
    /// their DDC channels exist, but reaching a monitor on one of them means
    /// driving the DKL PHY, which reference §8.8 defers; scanning them here
    /// would report a bus that NAKs for a reason this workstream cannot fix.
    pub(crate) const DDC: [Pin; 3] = [Pin::DdiA, Pin::DdiB, Pin::DdiC];

    /// Every pin this driver can select, for a table-driven test.
    pub(crate) const ALL: [Pin; 7] = [
        Pin::DdiA,
        Pin::DdiB,
        Pin::DdiC,
        Pin::Tc1,
        Pin::Tc2,
        Pin::Tc3,
        Pin::Tc4,
    ];

    /// The value of `GMBUS0[4:0]` for this pin.  One-based: never zero.
    pub(crate) const fn index(self) -> u32 {
        match self {
            Self::DdiA => 1,
            Self::DdiB => 2,
            Self::DdiC => 3,
            Self::Tc1 => 9,
            Self::Tc2 => 10,
            Self::Tc3 => 11,
            Self::Tc4 => 12,
        }
    }

    /// The name `[I915]`'s pin table gives this channel, so that a log line and
    /// a source can be read side by side.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::DdiA => "dpa",
            Self::DdiB => "dpb",
            Self::DdiC => "dpc",
            Self::Tc1 => "tc1",
            Self::Tc2 => "tc2",
            Self::Tc3 => "tc3",
            Self::Tc4 => "tc4",
        }
    }

    /// The GPIO register name this pin pair is wired to (`[I915]`
    /// `display/intel_gmbus.c:113-124`).
    pub(crate) const fn gpio(self) -> &'static str {
        match self {
            Self::DdiA => "GPIOB",
            Self::DdiB => "GPIOC",
            Self::DdiC => "GPIOD",
            Self::Tc1 => "GPIOJ",
            Self::Tc2 => "GPIOK",
            Self::Tc3 => "GPIOL",
            Self::Tc4 => "GPIOM",
        }
    }

    /// The DDI this pin carries DDC for, when it is a DDI pin.
    pub(crate) const fn ddi(self) -> Option<Ddi> {
        match self {
            Self::DdiA => Some(Ddi::A),
            Self::DdiB => Some(Ddi::B),
            Self::DdiC => Some(Ddi::C),
            Self::Tc1 | Self::Tc2 | Self::Tc3 | Self::Tc4 => None,
        }
    }

    /// The AUX/DDC power well this pin pair's channel sits behind.
    ///
    /// Reference §11 phase 2.1 names `ICL_PWR_WELL_CTL_AUX2` and index 0 for
    /// `AUX_A` as what GMBUS/DCC on a pin pair needs; the indices come from
    /// `[I915]` `i915_reg.h:3675-3688`, where the XE_LPD (ADL) column gives
    /// `AUX_A` 0, `AUX_B` 1, `AUX_C` 2 and `AUX_TC1`..`AUX_TC4` 3 through 6.
    /// The reference's own `[GAP]` stands: which of `AUX_C` and above are
    /// physically present on a given ADL-N SKU is established by no source read
    /// here, so this mapping says where the state *would* be read, and a state
    /// bit that reads zero on a well the part does not have is reported as what
    /// it is -- a bit that reads zero -- rather than as a conclusion about
    /// silicon.
    pub(crate) const fn aux_well(self) -> AuxWell {
        match self {
            Self::DdiA => AuxWell {
                name: "AUX_A",
                index: 0,
            },
            Self::DdiB => AuxWell {
                name: "AUX_B",
                index: 1,
            },
            Self::DdiC => AuxWell {
                name: "AUX_C",
                index: 2,
            },
            Self::Tc1 => AuxWell {
                name: "AUX_USBC1",
                index: 3,
            },
            Self::Tc2 => AuxWell {
                name: "AUX_USBC2",
                index: 4,
            },
            Self::Tc3 => AuxWell {
                name: "AUX_USBC3",
                index: 5,
            },
            Self::Tc4 => AuxWell {
                name: "AUX_USBC4",
                index: 6,
            },
        }
    }
}

impl fmt::Display for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ddi = match self.ddi() {
            Some(ddi) => format!(", DDI {ddi}"),
            None => String::new(),
        };
        write!(f, "pin {} ({}{ddi})", self.index(), self.name())
    }
}

/// An AUX/DDC power well: the gate in front of a pin pair's DDC channel.
///
/// Only its identity and its state bit are modelled.  Requesting a well is the
/// power workstream's job; this value exists so that a failed transaction can
/// name the well that explains it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuxWell {
    name: &'static str,
    index: u32,
}

impl AuxWell {
    /// The name the reference and `[I915]` use.
    pub(crate) const fn name(self) -> &'static str {
        self.name
    }

    /// The well's index within its control register.
    pub(crate) const fn index(self) -> u32 {
        self.index
    }

    /// The bit that reports whether the well is on.
    ///
    /// `[I915]` `i915_reg.h:3630-3631` and reference §4.2:
    /// `STATE(i) = 0x1 << (i*2)`, `REQ(i) = 0x2 << (i*2)`.  Both are computed
    /// from the index; the test at the bottom of this file checks the twelve
    /// resulting values against the reference's own table, including the four
    /// request bits this module never writes.
    pub(crate) const fn state_bit(self) -> u32 {
        0x1 << (self.index * 2)
    }

    /// The bit a *requester* sets to ask for the well.  Never written here: it
    /// is the power workstream's bit, and it is computed so that the two halves
    /// of the layout stay together.
    pub(crate) const fn request_bit(self) -> u32 {
        0x2 << (self.index * 2)
    }
}

impl fmt::Display for AuxWell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (index {}, state bit {:#x})",
            self.name,
            self.index,
            self.state_bit()
        )
    }
}

/// The bus rate, in the `GMBUS0[9:8]` encoding.
///
/// The reference's recovery table puts a rate change among the things to try
/// when a read answers but does not validate, and names 100 kHz and 50 kHz
/// (§11.1).  This driver starts at 100 kHz -- the conservative end of what
/// every DDC sink supports -- and retries at 50 kHz.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Rate {
    /// `GMBUS_RATE_100KHZ`, the standard DDC rate.
    #[default]
    Khz100,
    /// `GMBUS_RATE_50KHZ`, the next step down.
    Khz50,
    /// `GMBUS_RATE_400KHZ`.
    Khz400,
    /// `GMBUS_RATE_1MHZ`.
    Mhz1,
}

impl Rate {
    /// The rate this driver uses first.
    pub(crate) const DEFAULT: Rate = Rate::Khz100;

    /// The `GMBUS0[9:8]` field value (`[I915]` `intel_gmbus_regs.h:32-35`).
    pub(crate) const fn field(self) -> u32 {
        match self {
            Self::Khz100 => 0,
            Self::Khz50 => 1,
            Self::Khz400 => 2,
            Self::Mhz1 => 3,
        }
    }

    /// The rate in kilohertz, for a log line.
    pub(crate) const fn khz(self) -> u32 {
        match self {
            Self::Khz100 => 100,
            Self::Khz50 => 50,
            Self::Khz400 => 400,
            Self::Mhz1 => 1000,
        }
    }

    /// The next rate to try when a read does not validate, or `None` at the
    /// bottom of the ladder.
    pub(crate) const fn slower(self) -> Option<Rate> {
        match self {
            Self::Khz400 | Self::Mhz1 | Self::Khz100 => Some(Rate::Khz50),
            Self::Khz50 => None,
        }
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} kHz", self.khz())
    }
}

// ---------------------------------------------------------------------------
// Failures
// ---------------------------------------------------------------------------

/// What the AUX/DDC power well said about itself when a transaction NAKed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuxWellReading {
    /// The state bit is set: the well is on, so the well is not the
    /// explanation.
    On,
    /// The state bit is clear.  Reads of a powered-down well return zero
    /// (reference §4.2), so this is either a well that is off or a well the
    /// part does not have; both mean the same thing to a caller.
    Off,
    /// The register could not be read at all -- it lies outside the mapped
    /// window -- so nothing can be said either way.
    Unreadable,
}

impl fmt::Display for AuxWellReading {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::On => "the power well's state bit reads 1 (on)",
            Self::Off => "the power well's state bit reads 0 (off, or absent)",
            Self::Unreadable => "the power well register could not be read",
        })
    }
}

/// Everything that can go wrong on the way to 128 bytes of EDID.
///
/// One variant per condition, because the caller's next move differs for each:
/// [`GmbusError::AuxWellDown`] is a message for the power workstream,
/// [`GmbusError::NoAck`] is a message about the cable or the port,
/// [`GmbusError::BusStuck`] wants a reset and a retry, and a checksum failure
/// wants a slower read.  A single `Err(())` would collapse all of that into "no
/// EDID", which is the answer that wastes a day on hardware.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GmbusError {
    /// A register the transaction needs is outside the mapped window, so no
    /// transaction was attempted.
    WindowTooSmall { register: &'static str },
    /// The register window refused a register it had an address for.  This is
    /// a bug in the register table, not a hardware condition.
    RegisterRefused { register: &'static str },
    /// `GMBUS2.INUSE` was set when the transaction began: something else is
    /// driving the controller.
    BusInUse { pin: Pin, status: u32 },
    /// `GMBUS2.HW_RDY` never set.  The controller accepted the command and then
    /// stopped talking.
    ReadyTimeout {
        pin: Pin,
        rate: Rate,
        status: u32,
        waited_micros: u64,
    },
    /// `GMBUS2.ACTIVE` never cleared, or `GMBUS2.STALL_TIMEOUT` appeared: the
    /// bus did not go idle, which is what a secondary holding the clock looks
    /// like (reference §11.1).
    BusStuck {
        pin: Pin,
        status: u32,
        waited_micros: u64,
    },
    /// `GMBUS2.SATOER` with the AUX/DDC power well for that pin pair reading
    /// back as off.  This is the condition reference §11 phase 2.1 and §11.1
    /// both name, and it is a *named* error rather than a timeout because the
    /// fix is a power-well enable, not a retry.
    AuxWellDown {
        pin: Pin,
        rate: Rate,
        address: u8,
        well: AuxWell,
    },
    /// `GMBUS2.SATOER` with the well up (or unreadable): the sink, or the
    /// absence of one, declined the address.
    NoAck {
        pin: Pin,
        rate: Rate,
        address: u8,
        well: AuxWellReading,
    },
    /// Every byte read came back `0xff`: the bus floated high, which is what a
    /// missing device with working pull-ups looks like (reference §11.1).  The
    /// data is not an EDID and is not reported as one.
    BusFloating { pin: Pin },
    /// The first eight bytes are not the EDID header.
    EdidHeader { pin: Pin, header: [u8; 8] },
    /// The 128 bytes do not sum to zero modulo 256.
    EdidChecksum { pin: Pin, sum: u8 },
}

impl GmbusError {
    /// Whether a retry on the same pin could plausibly do better.
    ///
    /// Reference §11.1's recovery is "reset the bus, then wait one full EDID
    /// transaction time and retry", so everything a reset could plausibly clear
    /// gets one more attempt: a NAK (which `[I915]` also retries once, because
    /// "passive adapters sometimes NAK the first probe", `intel_gmbus.c:
    /// 714-725`), a read that answered but did not validate (at a lower rate),
    /// a controller that stopped offering data, and a bus that did not go idle.
    /// The last of those is worth retrying precisely because it is
    /// diagnostic: if the reset does *not* clear it, the answer is "a reset does
    /// not fix this bus", which is what reference §11.1 says to conclude before
    /// reaching for bit-banging.
    ///
    /// Three things are not retried, because a second identical transaction
    /// cannot change them: a floating bus (there is no device), a bus already in
    /// use by someone else, and a register window that does not reach the
    /// registers at all.
    pub(crate) const fn worth_retrying(self) -> bool {
        matches!(
            self,
            Self::NoAck { .. }
                | Self::AuxWellDown { .. }
                | Self::EdidHeader { .. }
                | Self::EdidChecksum { .. }
                | Self::ReadyTimeout { .. }
                | Self::BusStuck { .. }
        )
    }

    /// Whether the failure says something about the power well rather than
    /// about the sink.
    pub(crate) const fn aux_well_is_down(self) -> bool {
        matches!(self, Self::AuxWellDown { .. })
    }

    /// One line for a log, complete enough to act on without the code open.
    ///
    /// The target machine's only console is the screen this driver is trying to
    /// light, so a message that says "GMBUS error" is a message nobody can act
    /// on.
    pub(crate) fn describe(self) -> String {
        match self {
            Self::WindowTooSmall { register } => format!(
                "the mapped register window does not reach {register}, so no DDC transaction was \
                 attempted"
            ),
            Self::RegisterRefused { register } => format!(
                "{register} is declared read-only or is not in the bus table, which is a bug in \
                 this kernel rather than a hardware condition"
            ),
            Self::BusInUse { pin, status } => format!(
                "GMBUS was already in use when the transaction on {pin} began (GMBUS2 = \
                 {status:#010x}: {})",
                describe_status(status)
            ),
            Self::ReadyTimeout {
                pin,
                rate,
                status,
                waited_micros,
            } => format!(
                "GMBUS2.HW_RDY never set on {pin} at {rate}: waited {waited_micros} us, last \
                 GMBUS2 = {status:#010x} ({})",
                describe_status(status)
            ),
            Self::BusStuck {
                pin,
                status,
                waited_micros,
            } => format!(
                "the bus did not go idle after the transaction on {pin}: waited {waited_micros} \
                 us, last GMBUS2 = {status:#010x} ({}); a secondary is holding the clock",
                describe_status(status)
            ),
            Self::AuxWellDown {
                pin,
                rate,
                address,
                well,
            } => format!(
                "every address NAKed on {pin} at {rate} (slave {address:#04x}) and {well} reads \
                 back as off: the AUX/DDC power well for this pin pair is not enabled, which is \
                 what GMBUS returning NAK on every address looks like"
            ),
            Self::NoAck {
                pin,
                rate,
                address,
                well,
            } => format!(
                "the sink NAKed slave {address:#04x} on {pin} at {rate}, and {well}: no device \
                 answered at that address"
            ),
            Self::BusFloating { pin } => format!(
                "every byte read on {pin} was 0xff: the bus floated high, so there is no device \
                 on this pin pair (or its pull-ups are missing)"
            ),
            Self::EdidHeader { pin, header } => format!(
                "the block read on {pin} does not start with the EDID header: {:02x} {:02x} \
                 {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}",
                header[0],
                header[1],
                header[2],
                header[3],
                header[4],
                header[5],
                header[6],
                header[7]
            ),
            Self::EdidChecksum { pin, sum } => format!(
                "the block read on {pin} sums to {sum:#04x} instead of 0 modulo 256, so its \
                 contents are not trustworthy"
            ),
        }
    }
}

impl fmt::Display for GmbusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// Name the set bits of a `GMBUS2` value, for a log line.
///
/// A raw status word in a log is a puzzle; the point of a diagnostic on a
/// machine with no serial port is that the person reading the screen does not
/// have to hold the register reference open at the same time.
fn describe_status(status: u32) -> String {
    let mut bits = Vec::new();
    for (mask, name) in [
        (GMBUS2_INUSE, "INUSE"),
        (GMBUS2_HW_WAIT_PHASE, "HW_WAIT_PHASE"),
        (GMBUS2_STALL_TIMEOUT, "STALL_TIMEOUT"),
        (GMBUS2_INT, "INT"),
        (GMBUS2_HW_RDY, "HW_RDY"),
        (GMBUS2_SATOER, "SATOER"),
        (GMBUS2_ACTIVE, "ACTIVE"),
    ] {
        if status & mask != 0 {
            bits.push(name);
        }
    }
    if bits.is_empty() {
        return String::from("no status bit set");
    }
    bits.join("|")
}

// ---------------------------------------------------------------------------
// The bytes
// ---------------------------------------------------------------------------

/// Every EDID block is 128 bytes: base block and extensions alike (VESA E-EDID;
/// reference §9.3 and §11 phase 2.3).
pub(crate) const EDID_BLOCK_LEN: usize = 128;

/// The eight bytes every EDID begins with.
const EDID_HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// Where the base block records how many extension blocks follow it.
const EDID_EXTENSION_COUNT_OFFSET: usize = 0x7e;

/// One EDID block: 128 bytes of a monitor's own description of itself.
///
/// The bytes are validated before this value exists -- header and checksum --
/// so holding one is a statement that the block is self-consistent.  It is not
/// a statement that the monitor is sane, that the timings in it are
/// programmable, or that this kernel understands them: those are `drm::modes`'
/// questions, and this type deliberately carries the bytes rather than an
/// opinion about them.  [`EdidBytes::as_slice`] is the shape that parser takes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EdidBytes {
    bytes: [u8; EDID_BLOCK_LEN],
}

impl EdidBytes {
    /// The block, for a parser that borrows it.
    pub(crate) const fn bytes(&self) -> &[u8; EDID_BLOCK_LEN] {
        &self.bytes
    }

    /// The block as a slice, which is the shape `drm::modes` takes.
    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// How many extension blocks the sink says follow this one.
    pub(crate) const fn extension_count(&self) -> u8 {
        self.bytes[EDID_EXTENSION_COUNT_OFFSET]
    }

    /// A line about what the block is *not*: the byte-level facts only.
    ///
    /// The identification fields are deliberately not decoded here.  A second
    /// decoder is exactly what this workstream was told not to write, and the
    /// mode layer owns that reading.
    pub(crate) fn describe(&self) -> String {
        let extensions = self.extension_count();
        format!(
            "128 bytes, checksum valid, {extensions} extension block{} declared",
            if extensions == 1 { "" } else { "s" }
        )
    }
}

/// A block that failed validation, before it is attributed to a pin.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EdidFault {
    Header([u8; 8]),
    Checksum(u8),
}

/// Check the two things that can be checked without understanding anything:
/// the header, and the checksum.
///
/// Reference §11 phase 2.3: "Checks: header `00 FF FF FF FF FF FF 00`; the EDID
/// checksum (sum of all 128 bytes is 0 mod 256)".  Both are checked before the
/// bytes are returned, because a wrong EDID gives wrong timings and the bug it
/// causes is indistinguishable from a modeset bug.
fn validate_edid(block: &[u8; EDID_BLOCK_LEN]) -> Result<(), EdidFault> {
    if block[..8] != EDID_HEADER {
        let mut header = [0u8; 8];
        header.copy_from_slice(&block[..8]);
        return Err(EdidFault::Header(header));
    }
    let sum = block.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    if sum != 0 {
        return Err(EdidFault::Checksum(sum));
    }
    Ok(())
}

/// Whether every byte of the block is `0xff`.
///
/// This is not a validation rule, it is a diagnostic one: a bus with no device
/// on it and working pull-ups reads as all ones, and the reference lists "GMBUS
/// returns 0xFF bytes" separately from "the header is wrong" because the two
/// have different causes (§11.1).  Reporting a floating bus as a bad header
/// would send a reader looking for a broken monitor instead of a missing one.
fn all_ones(block: &[u8; EDID_BLOCK_LEN]) -> bool {
    block.iter().all(|byte| *byte == 0xff)
}

// ---------------------------------------------------------------------------
// Where the protocol touches the machine
// ---------------------------------------------------------------------------

/// The six registers a GMBUS transaction is a state machine over.
///
/// This trait is the entire contact between the protocol and the machine.  Two
/// implementations exist: [`RegisterWindow`], which is volatile access to the
/// mapped aperture, and the test controller at the bottom of this file, which
/// is a device model layered on a real window over an ordinary buffer.  The
/// protocol therefore runs -- including its timeout, NAK, stuck-bus and
/// recovery paths, which no machine without a fault injector can be made to
/// produce -- on a host that has no graphics device at all.
pub(crate) trait BusRegisters {
    /// Read a register, or `None` when it is outside the accessible window.
    fn read(&mut self, register: Register) -> Option<u32>;

    /// Write a register, returning whether the write happened.  A register the
    /// table declares read-only, or one outside the window, is refused.
    fn write(&mut self, register: Register, value: u32) -> bool;
}

impl BusRegisters for RegisterWindow {
    fn read(&mut self, register: Register) -> Option<u32> {
        RegisterWindow::read(*self, register)
    }

    fn write(&mut self, register: Register, value: u32) -> bool {
        RegisterWindow::write(*self, register, value)
    }
}

/// What a poll loop needs from the outside world: a monotonic reading, and a
/// way to let a moment pass.
///
/// The real implementation reads the platform clock and spins; the test
/// implementation *is* the clock, which is how a 50 ms timeout is tested in
/// microseconds of wall time.  A timer whose `pause` does not advance
/// `now_micros` would spin forever, which is the one contract this trait has.
pub(crate) trait PollTimer {
    /// Microseconds from a monotonic source.
    fn now_micros(&self) -> u64;

    /// Let time pass, and give the processor something to do meanwhile.
    fn pause(&self);
}

/// The machine's own clock.
///
/// `pause` is a bounded busy wait rather than a sleep on purpose: the first
/// EDID read can happen before there is a scheduler to sleep on, and a driver
/// that cannot run from the boot path is a driver whose diagnostics arrive too
/// late to explain a boot.
pub(crate) struct MonotonicTimer;

impl PollTimer for MonotonicTimer {
    fn now_micros(&self) -> u64 {
        axhal::time::monotonic_time_nanos() / 1_000
    }

    fn pause(&self) {
        axhal::time::busy_wait(core::time::Duration::from_micros(POLL_INTERVAL_MICROS));
    }
}

/// Observations from the bus that are not failures.
///
/// These are the facts a bring-up wants that an error cannot carry: the state
/// the firmware left the controller in, and how many attempts a read took.  A
/// successful read with `stale_two_byte_index` set is a finding about the
/// firmware, and it is worth a line in a boot log rather than silence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct BusNotes {
    /// `GMBUS5` had two-byte index mode enabled when the first transaction
    /// began: the firmware left the controller in a mode this driver does not
    /// use, and the read only worked because it was cleared.
    pub(crate) stale_two_byte_index: bool,
    /// `GMBUS2.INUSE` was set before the first transaction: something else was
    /// driving the controller -- firmware, or a previous transfer of ours that
    /// did not finish.
    pub(crate) was_in_use: bool,
    /// The rate the last attempt ran at, which is not the default rate when the
    /// first attempt failed validation.
    pub(crate) last_rate: Rate,
    /// How many transactions were attempted on this pin.  One when the first
    /// attempt answered and validated.
    pub(crate) attempts: u8,
}

impl BusNotes {
    /// Whether anything here is worth a log line.
    pub(crate) const fn is_quiet(&self) -> bool {
        !self.stale_two_byte_index && !self.was_in_use && self.attempts <= 1
    }

    /// The notes as a log line, or `None` when there is nothing to say.
    pub(crate) fn describe(&self) -> Option<String> {
        if self.is_quiet() {
            return None;
        }
        let mut parts = Vec::new();
        if self.stale_two_byte_index {
            parts.push(String::from(
                "the firmware left GMBUS5 in two-byte index mode (cleared before the transaction)",
            ));
        }
        if self.was_in_use {
            parts.push(String::from("GMBUS2.INUSE was set before the transaction"));
        }
        if self.attempts > 1 {
            parts.push(format!(
                "{} attempts, the last at {}",
                self.attempts, self.last_rate
            ));
        }
        Some(parts.join("; "))
    }
}

// ---------------------------------------------------------------------------
// The transaction
// ---------------------------------------------------------------------------

/// What a wait ended as, with the last status read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Waited {
    status: u32,
    micros: u64,
    outcome: WaitOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    /// The condition was met.
    Met,
    /// `GMBUS2.SATOER`: the sink NAKed the address.
    NoAck,
    /// `GMBUS2.STALL_TIMEOUT`: a secondary held the clock too long.
    Stalled,
    /// The deadline passed first.
    TimedOut,
}

/// One GMBUS transaction's worth of state.
struct Bus<'a, R: BusRegisters, T: PollTimer> {
    registers: &'a mut R,
    timer: &'a T,
    pin: Pin,
    rate: Rate,
    notes: &'a mut BusNotes,
}

impl<R: BusRegisters, T: PollTimer> Bus<'_, R, T> {
    fn read(&mut self, register: Register) -> Result<u32, GmbusError> {
        self.registers
            .read(register)
            .ok_or(GmbusError::WindowTooSmall {
                register: register.name(),
            })
    }

    fn write(&mut self, register: Register, value: u32) -> Result<(), GmbusError> {
        if self.registers.write(register, value) {
            Ok(())
        } else {
            Err(GmbusError::RegisterRefused {
                register: register.name(),
            })
        }
    }

    /// Read a register back after writing it.
    ///
    /// MMIO writes are posted: the store retiring says nothing about the device
    /// having seen it, and this architecture offers no completion for a store.
    /// Reference §2.2 is explicit that a driver should do this "after a batch
    /// of writes ... before polling a status bit that the batch was supposed to
    /// affect", which is exactly the shape of selecting a pin and then waiting
    /// for data.
    fn posting_read(&mut self, register: Register) -> Result<u32, GmbusError> {
        self.read(register)
    }

    /// Put the controller back to a known state.
    ///
    /// Reference §11.1's recovery, which is `[I915]`'s `intel_gmbus_reset`
    /// (`display/intel_gmbus.c:209-213`): `GMBUS0 = 0` releases the pin,
    /// `GMBUS4 = 0` clears the interrupt mask so no completion is asked for.
    fn reset(&mut self) -> Result<(), GmbusError> {
        self.write(GMBUS0, 0)?;
        self.write(GMBUS4, 0)?;
        Ok(())
    }

    /// Clear a latched bus error.
    ///
    /// `[I915]`'s `clear_err` path (`intel_gmbus.c:680-708`): wait for the bus
    /// to go idle first -- clearing the NAK while the bus is still active
    /// leaves it active -- then toggle `SW_CLR_INT`, which resets the
    /// controller, and release the pin.
    fn clear_error(&mut self) {
        let _ = self.wait_idle();
        let _ = self.write(GMBUS1, GMBUS1_SW_CLR_INT);
        let _ = self.write(GMBUS1, 0);
        let _ = self.write(GMBUS0, 0);
    }

    /// Select the pin and the rate, and clear the two-byte index mode.
    ///
    /// `GMBUS5` is written for a reason worth stating.  It holds the 16-bit
    /// index for transfers that address more than 255 bytes into a slave
    /// (`[I915]` `intel_gmbus.c:596-612`, which sets it, and `:615`, which
    /// clears it after the transfer).  This driver never uses that mode, so a
    /// `GMBUS_2BYTE_INDEX_EN` left set by firmware would silently reinterpret
    /// the index phase of the first read -- a wrong-but-plausible EDID, which
    /// is the worst kind of failure in this module.  Clearing it costs one
    /// write, to a value (zero) that i915 itself writes on this hardware, and
    /// the value that was there is recorded in the notes either way, so the
    /// finding about the firmware is not lost.
    fn prepare(&mut self) -> Result<(), GmbusError> {
        let status = self.read(GMBUS2)?;
        if status & GMBUS2_INUSE != 0 {
            self.notes.was_in_use = true;
        }
        let index_mode = self.read(GMBUS5)?;
        if index_mode & GMBUS5_2BYTE_INDEX_EN != 0 {
            self.notes.stale_two_byte_index = true;
        }
        self.write(GMBUS5, 0)?;
        let select =
            (self.rate.field() << GMBUS0_RATE_SHIFT) | (self.pin.index() & GMBUS0_PIN_MASK);
        self.write(GMBUS0, select)?;
        let selected = self.posting_read(GMBUS0)?;
        debug_assert_eq!(
            selected & (GMBUS0_RATE_MASK | GMBUS0_PIN_MASK),
            select,
            "the pin and rate just programmed did not read back"
        );
        Ok(())
    }

    /// Poll `GMBUS2` until `mask` is set, a terminal condition appears, or the
    /// deadline passes.
    fn wait_for(&mut self, mask: u32, timeout_micros: u64) -> Result<Waited, GmbusError> {
        let start = self.timer.now_micros();
        let deadline = start.saturating_add(timeout_micros);
        let mut status = self.read(GMBUS2)?;
        loop {
            let outcome = if status & GMBUS2_SATOER != 0 {
                WaitOutcome::NoAck
            } else if status & GMBUS2_STALL_TIMEOUT != 0 {
                WaitOutcome::Stalled
            } else if status & mask != 0 {
                WaitOutcome::Met
            } else if self.timer.now_micros() >= deadline {
                WaitOutcome::TimedOut
            } else {
                self.timer.pause();
                status = self.read(GMBUS2)?;
                continue;
            };
            return Ok(Waited {
                status,
                micros: self.timer.now_micros().saturating_sub(start),
                outcome,
            });
        }
    }

    /// Wait for `GMBUS2.ACTIVE` to clear (`[I915]` `gmbus_wait_idle`,
    /// `intel_gmbus.c:399-420`).
    ///
    /// i915 also waits for `GMBUS2.HW_WAIT_PHASE` after the data.  This driver
    /// does not: waiting for the bus to go idle is the stronger condition, and
    /// adding a second way for a good read to be called bad is not worth it.
    fn wait_idle(&mut self) -> Result<Waited, GmbusError> {
        let start = self.timer.now_micros();
        let deadline = start.saturating_add(IDLE_TIMEOUT_MICROS);
        let mut status = self.read(GMBUS2)?;
        loop {
            if status & GMBUS2_ACTIVE == 0 {
                return Ok(Waited {
                    status,
                    micros: self.timer.now_micros().saturating_sub(start),
                    outcome: WaitOutcome::Met,
                });
            }
            if self.timer.now_micros() >= deadline {
                return Ok(Waited {
                    status,
                    micros: self.timer.now_micros().saturating_sub(start),
                    outcome: WaitOutcome::TimedOut,
                });
            }
            self.timer.pause();
            status = self.read(GMBUS2)?;
        }
    }

    /// Read the AUX/DDC power well's state bit for this pin pair.
    fn aux_well_state(&mut self) -> AuxWellReading {
        let well = self.pin.aux_well();
        match self.registers.read(ICL_PWR_WELL_CTL_AUX2) {
            Some(value) if value & well.state_bit() != 0 => AuxWellReading::On,
            Some(_) => AuxWellReading::Off,
            None => AuxWellReading::Unreadable,
        }
    }

    /// Turn a failed wait into the named error it actually is.
    ///
    /// This is where the module does the thing the reference asks for by name:
    /// on a NAK it reads the power well rather than reporting a bare NAK, so
    /// that the log says "the AUX/DDC power well for this pin pair is not
    /// enabled" when that is what happened.  The recovery is run first, so the
    /// controller is left usable whatever the caller does next.
    fn failure(&mut self, waited: Waited, address: u8) -> GmbusError {
        let reading = match waited.outcome {
            WaitOutcome::NoAck => Some(self.aux_well_state()),
            _ => None,
        };
        let pin = self.pin;
        let rate = self.rate;
        self.clear_error();
        match waited.outcome {
            WaitOutcome::NoAck => match reading.unwrap_or(AuxWellReading::Unreadable) {
                AuxWellReading::Off => GmbusError::AuxWellDown {
                    pin,
                    rate,
                    address,
                    well: pin.aux_well(),
                },
                well => GmbusError::NoAck {
                    pin,
                    rate,
                    address,
                    well,
                },
            },
            WaitOutcome::Stalled => GmbusError::BusStuck {
                pin,
                status: waited.status,
                waited_micros: waited.micros,
            },
            WaitOutcome::TimedOut | WaitOutcome::Met => GmbusError::ReadyTimeout {
                pin,
                rate,
                status: waited.status,
                waited_micros: waited.micros,
            },
        }
    }

    /// One index cycle followed by one read, into `out`.
    ///
    /// The command word is the form i915 v6.12 writes: the index byte in
    /// `SLAVE_INDEX`, `CYCLE_INDEX` and `CYCLE_WAIT` together in one write,
    /// then the byte count, the slave address, the read bit and `SW_RDY`
    /// (`[I915]` `intel_gmbus.c:451-452` with `gmbus1_index` built at
    /// `:596-611`).
    fn transfer(&mut self, address: u8, index: u8, out: &mut [u8]) -> Result<(), GmbusError> {
        if out.is_empty() || out.len() > MAX_TRANSFER {
            // Not reachable through this module's own callers, which ask for
            // one 128-byte block; a caller that asks for something else gets a
            // refusal rather than a silently truncated read.
            return Err(GmbusError::WindowTooSmall {
                register: "GMBUS3 (transfer length outside 1..=128)",
            });
        }
        let count = out.len() as u32;

        self.reset()?;
        self.prepare()?;

        let command = GMBUS1_CYCLE_INDEX
            | ((u32::from(index) << GMBUS1_SLAVE_INDEX_SHIFT) & GMBUS1_SLAVE_INDEX_MASK)
            | GMBUS1_CYCLE_WAIT
            | ((count << GMBUS1_BYTE_COUNT_SHIFT) & GMBUS1_BYTE_COUNT_MASK)
            | ((u32::from(address) << GMBUS1_SLAVE_ADDR_SHIFT) & GMBUS1_SLAVE_ADDR_MASK)
            | GMBUS1_SLAVE_READ
            | GMBUS1_SW_RDY;
        self.write(GMBUS1, command)?;

        let transaction_deadline = self
            .timer
            .now_micros()
            .saturating_add(TRANSACTION_TIMEOUT_MICROS);
        let mut written = 0;
        while written < out.len() {
            let budget = transaction_deadline.saturating_sub(self.timer.now_micros());
            if budget == 0 {
                let status = self.read(GMBUS2)?;
                self.clear_error();
                return Err(GmbusError::ReadyTimeout {
                    pin: self.pin,
                    rate: self.rate,
                    status,
                    waited_micros: TRANSACTION_TIMEOUT_MICROS,
                });
            }
            let waited = self.wait_for(GMBUS2_HW_RDY, budget.min(READY_TIMEOUT_MICROS))?;
            if waited.outcome != WaitOutcome::Met {
                return Err(self.failure(waited, address));
            }
            let word = self.read(GMBUS3)?;
            // Little-endian, byte 0 in bits 7:0 (reference §9.3 step 4).
            for shift in 0..4 {
                if written == out.len() {
                    break;
                }
                out[written] = (word >> (8 * shift)) as u8;
                written += 1;
            }
        }

        // The controller cannot generate a stop on the first cycle, so a
        // separate stop cycle is issued unconditionally ([I915] `intel_gmbus.c`
        // :660-664, whose comment says exactly that).
        self.write(GMBUS1, GMBUS1_CYCLE_STOP | GMBUS1_SW_RDY)?;
        let idle = self.wait_idle()?;
        self.write(GMBUS0, 0)?;
        if idle.status & GMBUS2_TERMINAL != 0 {
            return Err(self.failure(
                Waited {
                    outcome: WaitOutcome::NoAck,
                    ..idle
                },
                address,
            ));
        }
        if idle.outcome != WaitOutcome::Met {
            return Err(GmbusError::BusStuck {
                pin: self.pin,
                status: idle.status,
                waited_micros: idle.micros,
            });
        }
        Ok(())
    }
}

/// Read one 128-byte block from the EEPROM at `address`, starting at EEPROM
/// byte address `offset`, with no retries.
///
/// This is the transport, with no opinion about what the bytes mean.
fn read_block_once<R: BusRegisters, T: PollTimer>(
    registers: &mut R,
    timer: &T,
    pin: Pin,
    rate: Rate,
    address: u8,
    offset: u8,
    notes: &mut BusNotes,
) -> Result<EdidBytes, GmbusError> {
    notes.last_rate = rate;
    let mut bytes = [0u8; EDID_BLOCK_LEN];
    let mut bus = Bus {
        registers,
        timer,
        pin,
        rate,
        notes,
    };
    bus.transfer(address, offset, &mut bytes)?;
    if all_ones(&bytes) {
        return Err(GmbusError::BusFloating { pin });
    }
    match validate_edid(&bytes) {
        Ok(()) => Ok(EdidBytes { bytes }),
        Err(EdidFault::Header(header)) => Err(GmbusError::EdidHeader { pin, header }),
        Err(EdidFault::Checksum(sum)) => Err(GmbusError::EdidChecksum { pin, sum }),
    }
}

/// Read one block, applying the recoveries the reference names.
///
/// Two attempts at most, and what changes between them is what the reference
/// says to change:
///
/// * **A NAK, once, at the same rate.**  `[I915]` retries the first message
///   once because "passive adapters sometimes NAK the first probe"
///   (`intel_gmbus.c:714-725`); a monitor that was not ready when the kernel
///   booted is the cheapest thing in this file to recover from.
/// * **A block that answered but did not validate, once, at a lower rate.**
///   Reference §11.1's row for a header that reads but a checksum that fails:
///   "Re-read; if persistent, lower the rate to 100 kHz or 50 kHz".  This
///   driver already starts at 100 kHz, so the second attempt is at 50 kHz.
/// * **A controller that stopped offering data, or a bus that did not go idle,
///   once, after the reset the recovery section prescribes.**  A second failure
///   of the same kind is the answer that says a reset does not fix this bus.
///
/// A floating bus, a bus already in use and a window that does not reach the
/// registers end here: retrying cannot change any of them.  See
/// [`GmbusError::worth_retrying`].
fn read_block<R: BusRegisters, T: PollTimer>(
    registers: &mut R,
    timer: &T,
    pin: Pin,
    address: u8,
    offset: u8,
    notes: &mut BusNotes,
) -> Result<EdidBytes, GmbusError> {
    let mut rate = Rate::DEFAULT;
    let mut attempt = 0u8;
    loop {
        attempt += 1;
        notes.attempts = attempt;
        match read_block_once(registers, timer, pin, rate, address, offset, notes) {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                let retry = if attempt < MAX_ATTEMPTS && error.worth_retrying() {
                    match error {
                        GmbusError::EdidHeader { .. } | GmbusError::EdidChecksum { .. } => {
                            rate.slower()
                        }
                        _ => Some(rate),
                    }
                } else {
                    None
                };
                match retry {
                    Some(next) => rate = next,
                    None => return Err(error),
                }
            }
        }
    }
}

/// Read the 128-byte EDID base block from the monitor on `pin`, and validate it.
///
/// The result is either 128 bytes that passed the header and checksum checks, or
/// a named failure.  Nothing partial is ever returned: a block that failed
/// validation is not a block, and handing back the bytes would be handing
/// whoever debugs the next layer a display bug that is really a parse bug.
pub(crate) fn read_edid(regs: &RegisterWindow, pin: Pin) -> Result<EdidBytes, GmbusError> {
    read_edid_detailed(regs, pin).0
}

/// [`read_edid`], with the bus observations that are not failures.
///
/// The notes matter during bring-up: they carry the state the firmware left
/// the controller in and how many attempts a read took, neither of which an
/// error can say on a successful read.
pub(crate) fn read_edid_detailed(
    regs: &RegisterWindow,
    pin: Pin,
) -> (Result<EdidBytes, GmbusError>, BusNotes) {
    let mut notes = BusNotes::default();
    let mut registers = *regs;
    let result = read_edid_with(&mut registers, &MonotonicTimer, pin, &mut notes);
    (result, notes)
}

/// [`read_edid`] over any register file and any clock.
///
/// This is the seam the tests drive.  It is deliberately a register file and a
/// clock rather than MMIO and a sleep, because every path worth testing here --
/// a timeout, a NAK, a stuck bus, a corrupt block -- is one that a working
/// machine will not produce on request.
pub(crate) fn read_edid_with<R: BusRegisters, T: PollTimer>(
    registers: &mut R,
    timer: &T,
    pin: Pin,
    notes: &mut BusNotes,
) -> Result<EdidBytes, GmbusError> {
    read_block(registers, timer, pin, DDC_ADDRESS, EDID_BASE_OFFSET, notes)
}

/// Read the first extension block, when a sink declares one.
///
/// The base block records how many extension blocks follow it; the first of
/// them sits at EEPROM address `0x80` and is the CTA-861 block that carries an
/// HDMI sink's extra modes, which is where a 1080p60 mode on a modern monitor
/// usually lives.  A sink that declares more than one extension returns its
/// first and the caller can see the count in [`EdidBytes::extension_count`]:
/// blocks past the first two are beyond the one-byte index this driver sends,
/// and reading a block the addressing cannot reach is refused rather than
/// guessed at.
pub(crate) fn read_edid_extension(
    regs: &RegisterWindow,
    pin: Pin,
) -> Result<Option<EdidBytes>, GmbusError> {
    let mut registers = *regs;
    read_edid_extension_with(&mut registers, &MonotonicTimer, pin)
}

/// [`read_edid_extension`] over any register file and any clock.
pub(crate) fn read_edid_extension_with<R: BusRegisters, T: PollTimer>(
    registers: &mut R,
    timer: &T,
    pin: Pin,
) -> Result<Option<EdidBytes>, GmbusError> {
    let mut notes = BusNotes::default();
    let base = read_block(
        registers,
        timer,
        pin,
        DDC_ADDRESS,
        EDID_BASE_OFFSET,
        &mut notes,
    )?;
    if base.extension_count() == 0 {
        return Ok(None);
    }
    let extension = read_block(
        registers,
        timer,
        pin,
        DDC_ADDRESS,
        EDID_EXTENSION_OFFSET,
        &mut notes,
    )?;
    Ok(Some(extension))
}

// ---------------------------------------------------------------------------
// Looking for a monitor
// ---------------------------------------------------------------------------

/// What one pin answered when it was asked for an EDID.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PinOutcome {
    pub(crate) pin: Pin,
    pub(crate) result: Result<EdidBytes, GmbusError>,
    pub(crate) notes: BusNotes,
}

/// The result of asking every DDC pin for an EDID.
#[derive(Clone, Debug, Default)]
pub(crate) struct SinkProbe {
    pub(crate) outcomes: Vec<PinOutcome>,
}

impl SinkProbe {
    /// The pin a monitor answered on, if one did.
    pub(crate) fn found(&self) -> Option<Pin> {
        self.outcomes
            .iter()
            .find(|outcome| outcome.result.is_ok())
            .map(|outcome| outcome.pin)
    }

    /// The validated block from the pin that answered.
    pub(crate) fn edid(&self) -> Option<EdidBytes> {
        self.outcomes
            .iter()
            .find_map(|outcome| outcome.result.as_ref().ok().copied())
    }

    /// Say what happened, on the console.
    ///
    /// Reference §11 phase 2.3: the pin a monitor answers on is how the
    /// physical port is identified, and that is worth `info!`.  The pins that
    /// did not answer are logged too -- at `info!` when the reason is "nothing
    /// there", and at `warn!` when it is something a person has to act on, such
    /// as an AUX power well that is off.
    pub(crate) fn log(&self) {
        match (self.found(), self.edid()) {
            (Some(pin), Some(edid)) => {
                axlog::info!(
                    "intel-gmbus: a monitor answered on {pin}: {}",
                    edid.describe()
                );
            }
            _ => {
                axlog::info!(
                    "intel-gmbus: no monitor answered on any DDC pin this kernel can select"
                );
            }
        }
        for outcome in &self.outcomes {
            match &outcome.result {
                Ok(_) => {}
                Err(error) if error.aux_well_is_down() => {
                    axlog::warn!("intel-gmbus: {}", error.describe());
                }
                Err(error) => {
                    axlog::info!("intel-gmbus: {}", error.describe());
                }
            }
            if let Some(notes) = outcome.notes.describe() {
                axlog::info!("intel-gmbus: {} on {}", notes, outcome.pin);
            }
        }
    }

    /// The same thing as text, for a boot report or a debug file.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        match self.found() {
            Some(pin) => out.push_str(&format!("monitor on {pin}\n")),
            None => out.push_str("no monitor on any DDC pin this kernel can select\n"),
        }
        for outcome in &self.outcomes {
            match &outcome.result {
                Ok(edid) => out.push_str(&format!("  {}: {}\n", outcome.pin, edid.describe())),
                Err(error) => out.push_str(&format!("  {}: {}\n", outcome.pin, error.describe())),
            }
        }
        out
    }
}

/// Ask every DDC pin this kernel can select whether a monitor is on it.
///
/// The pins are tried in the order the reference gives (§11 phase 2.3), and
/// every pin is asked even after one answers, because "which pin answered" is
/// the answer to *which physical port the monitor is on* and a second monitor
/// on a second port is a fact worth having.  Nothing here enables a power well
/// or an interrupt: this reads the bus and reports, and the only writes it
/// makes are the transaction's own.
pub(crate) fn probe_sink(regs: &RegisterWindow) -> SinkProbe {
    let mut registers = *regs;
    probe_sink_with(&mut registers, &MonotonicTimer)
}

/// [`probe_sink`] over any register file and any clock.
pub(crate) fn probe_sink_with<R: BusRegisters, T: PollTimer>(
    registers: &mut R,
    timer: &T,
) -> SinkProbe {
    let mut outcomes = Vec::new();
    for pin in Pin::DDC {
        let mut notes = BusNotes::default();
        let result = read_edid_with(registers, timer, pin, &mut notes);
        outcomes.push(PinOutcome { pin, result, notes });
    }
    SinkProbe { outcomes }
}

#[cfg(test)]
mod tests;
