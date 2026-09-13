//! Where phase 5's buffer-translation values come from: the PHY the firmware
//! already programmed.
//!
//! `crate::drm::intel::output` runs §8.5's voltage-swing sequence, and it
//! refuses to run at all without a [`SwingProgram`]: the *numbers* that
//! sequence writes are the one thing §8.5 marks as a `[GAP]`.  The table it
//! selects for an HDMI port, `icl_combo_phy_trans_hdmi`, was identified but not
//! extracted (§8.5, "Which table"; §13.1 item 12), and §8.5 records why that
//! matters: two Intel PRMs disagree about those values for the same nominal
//! level, because they are **board-tuned** rather than derivable from the PHY
//! IP.  Transcribing i915's table would be a decision about this board made
//! from another board's measurements, so this module does not do it.
//!
//! §13.4 gives the honest route: *"Boot the machine with a vendor driver (or
//! just the firmware's own GOP), let it modeset successfully, then dump the
//! display register window before anything clears it."*  This module is that
//! route, narrowed to the one register group phase 5 cannot do without.  On the
//! target machine the screen is the only console, a mode is already up when
//! this kernel starts, and the firmware that put it there programmed exactly
//! these registers to drive this board's connector at this board's swing.  A
//! read of them is a dump of a working configuration; a table copied out of
//! i915 is a guess about a board nobody measured.
//!
//! # What is read, and from where
//!
//! For a [`Ddi`] on a combo PHY (`A` ↔ PHY A, `B` ↔ PHY B; §8.1, §6.3), all
//! offsets of which `regs/port.rs` and `regs/mod.rs` hold declarations:
//!
//! | Register | Field | Why |
//! |---|---|---|
//! | `PORT_COMP_DW0` | `COMP_INIT[31]` | §8.3 steps 2 and 6: the PHY initialisation has run |
//! | `DDI_BUF_CTL` | `ENABLE[31]`, `IS_IDLE[7]`, `BUF_TRANS_SELECT[27:24]` | §8.6 steps 13-14: the port is enabled and driving, and the level it selected |
//! | `PORT_CL_DW10` | `PWR_DOWN_LN_MASK[7:4]` | §8.6 step 7: the lanes were powered before the buffer was enabled |
//! | `PORT_TX_DW2_LN0..3` | whole dword, **per lane** | §8.5 step 5's swing select |
//! | `PORT_TX_DW4_LN0..3` | whole dword, **per lane** | §8.5 step 5's cursor coefficient and step 2's per-lane loadgen select |
//! | `PORT_TX_DW5_LN0` | `TX_TRAINING_EN[31]`, and the word itself | §8.5 step 6: the write that commits the batch has happened |
//! | `PORT_TX_DW7_LN0..3` | whole dword, **per lane** | §8.5 step 5's N scalar |
//!
//! `PORT_TX_DW2`, `DW4` and `DW7` are read one lane at a time and the group
//! instance is never touched.  For `DW4` §8.5 step 2 says so in as many words
//! ("NOT group access -- each lane differs") and i915's own comment on the same
//! write is *"We cannot write to GRP. It would overwrite individual loadgen"*
//! (`[I915]` `display/intel_ddi.c:1160`); for `DW2` and `DW7` the sentence that
//! states the carve-out names `DW4` only, and the evidence is i915's sequence,
//! which writes both of them per lane in a loop over `ln = 0..3`
//! (`[I915]` `display/intel_ddi.c:1148-1157`, `:1171-1178`).  A read of a group
//! instance would collapse four values into one and the replay would drive every
//! lane from one lane's coefficients.  `DW5` is the one dword read from **lane
//! 0**: that is the copy i915 reads before writing the group instance
//! (`ICL_PORT_TX_DW5_LN(0, phy)`, `[I915]` `display/intel_ddi.c:1141`, `:1218`,
//! `:1226`), and `output::program` writes the group instance as it does
//! (`:1146`, `:1221`, `:1229`).  The group instances of `DW2` and `DW7` exist
//! and are declared in `regs/port.rs` -- §8.2's worked examples tabulate them --
//! but a DDI's swing does not land in them
//! (`[I915]` `display/intel_combo_phy_regs.h:96-105` gives the three address
//! forms: AUX `+0x380`, group `+0x680`, lane `0x880 + ln*0x100`).
//!
//! Every dword is read and later written whole.  That is deliberate: §8.5
//! enumerates the table's fields but not the registers' other bits, and i915
//! writes fields this module has no names for (`RCOMP_SCALAR(0x98)` in `DW2`,
//! `RTERM_SELECT(0x6)` and `TAP3_DISABLE` in `DW5`, the loadgen bit in `DW4`;
//! `[I915]` `display/intel_ddi.c:1141-1156`, `:1209-1211`).  A masked read
//! would drop them; a whole-dword read is the exact inverse of the whole-dword
//! write `output::program` performs, so the replay is bit-identical to what the
//! firmware left.
//!
//! # What has to be true before the values are believed
//!
//! A dump of a register is not evidence that the register holds a program.  A
//! port the firmware never brought up still answers reads -- with zero, with a
//! reset value, or with values from some earlier mode -- so [`read_firmware_swing`]
//! refuses unless every one of these holds, each as its own named
//! [`SwingSource`]:
//!
//! * `PORT_COMP_DW0.COMP_INIT` is set, so the PHY behind this port is in the
//!   state §8.3's initialisation leaves it (`PhyNotInitialised`).  §12.2 makes
//!   this the same read that decides whether a PHY instance is there at all,
//!   and `phy::init_one` treats it as "already initialised" for the same
//!   reason.
//! * `DDI_BUF_CTL.ENABLE` is set (`PortNotEnabled`) and `IS_IDLE` is clear
//!   (`PortStillIdle`).  §11 phase 5.7 calls `IS_IDLE` the single best "is my
//!   DDI alive" bit on the chip and phase 6.3 makes it the read-back that
//!   proves the port is scanning; a port that is idle has no working
//!   configuration to copy.
//! * At least one lane is powered (`LanesAllPoweredDown`): §8.6 step 7 powers
//!   the lanes *before* step 13 enables the buffer, so a buffer that reports
//!   itself enabled and not idle with every lane down is not a state that
//!   sequence produces.
//! * The swing words are not all-zeros or all-ones
//!   (`PhyNotResponding`): §12.2's absent-block test, asked of both per-lane
//!   dwords and of the lane-0 `DW5`.
//! * `PORT_TX_DW5.TX_TRAINING_EN` is set (`TrainingNotEnabled`): §8.5 step 6
//!   and `[I915]` `display/intel_ddi.c:1226-1229` make that bit the write that
//!   triggers the update, so with it clear the batch was never committed.
//!
//! Checks that would only describe *how* the firmware got there are
//! deliberately not refusals: `PORT_TX_DW5`'s scaling-mode field (i915 sets
//! `SCALING_MODE_SEL(0x2)`, §8.5 step 5 says `0b010`) and whether
//! `PWR_DOWN_LN_MASK` agrees lane-for-lane with `DDI_BUF_CTL`'s width.  Both
//! fields are replayed verbatim from the same read, so a disagreement would
//! change nothing about the values' honesty, while refusing on either could
//! block a board whose firmware used a different-but-working setting.
//!
//! # The two `PORT_TX_DW5` states
//!
//! [`SwingProgram`] carries two `DW5` dwords because §8.5's sequence writes two
//! states -- training disabled while the table values land (step 4), then
//! training enabled as the write that commits them (step 6) -- and the
//! firmware's register holds only the second.  Both go to the group instance;
//! this module reads **lane 0**, which is the copy i915 reads for the same two
//! writes (`[I915]` `display/intel_ddi.c:1218-1229`).  The first state is
//! **derived** here rather than left for a caller to invent: `TX_TRAINING_EN` is
//! a documented bit, bit 31 (`[I915]` `display/intel_combo_phy_regs.h:133`), and
//! the only difference between the state i915 leaves during the batch and the
//! state it leaves afterwards is that bit (`[I915]` `display/intel_ddi.c:1218-1229`:
//! read lane 0, clear, write group; program `DW2`/`DW4`/`DW7`; read lane 0, set,
//! write group).  So `dw5_training_disabled` is the lane-0 read with bit 31
//! cleared, and nothing else about it is guessed.  §8.5 gives the same two
//! states as steps 4 and 6.
//!
//! # What the caller does with it
//!
//! Phase 5's caller is the boot path in `modeset.rs`, which is another
//! workstream's and is not merged yet; this module is therefore not called from
//! anywhere yet.  It is `pub(crate)` so that caller can compose it as:
//!
//! ```text
//! match swing::read_firmware_swing(&window, ddi) {
//!     Ok(swing) => swing,
//!     Err(refusal) => {
//!         // `describe()` names the register, the value and the section.
//!         log!("intel: DDI {}: {} -- not mode-setting", ddi.name(), refusal);
//!         return;                       // do not attempt the modeset
//!     }
//! }
//! ```
//!
//! and then
//!
//! ```text
//! let request = output::OutputRequest::hdmi(ddi, mode, encoding).with_swing(swing);
//! let program = output::OutputProgram::plan(&request, platform_ref_khz)?;
//! output::program(&window, &program)?;
//! ```
//!
//! **When the read fails there is no fallback.**  The failure means this kernel
//! could not obtain this board's swing values, and the only alternatives are
//! i915's board-tuned table (a decision for the user, not a guess this code
//! makes) or a screen that stays dark.  It logs and does not program the
//! output.  A caller that caught the refusal and supplied invented numbers
//! would be doing exactly what §8.5's `[GAP]` and §13.1 item 12 exist to
//! prevent.
//!
//! # What has not been checked
//!
//! **Nothing in this module has run against real hardware.**  Every claim here
//! is a claim about the reference document, about i915 as cited, and about host
//! tests over `regs::mock::MockRegisters`.  No register has been read from a
//! Gen12 display engine, no firmware has been observed to leave the state this
//! module believes in, and the values it would return on the target machine are
//! unknown until it runs there.

use alloc::{format, string::String};
use core::fmt;

use super::{
    hpd::Ddi,
    output::SwingProgram,
    phy::COMP_INIT,
    regs::{self, Register, Registers, ddi, port},
};

/// `DDI_BUF_CTL[31]`, the port enable; §8.4, §8.6 step 13.
///
/// `[I915]` `i915_reg.h:3859` (`DDI_BUF_CTL_ENABLE (1 << 31)`).
const DDI_BUF_CTL_ENABLE: u32 = 1 << 31;

/// `DDI_BUF_CTL[27:24]`, the buffer-translation level index; §8.4.
///
/// `[I915]` `i915_reg.h:3861-3862` (`DDI_BUF_TRANS_SELECT(n) ((n) << 24)`,
/// `DDI_BUF_EMP_MASK (0xf << 24)`).
const DDI_BUF_TRANS_SELECT_SHIFT: u32 = 24;
const DDI_BUF_TRANS_SELECT_MASK: u32 = 0xf << DDI_BUF_TRANS_SELECT_SHIFT;

/// `DDI_BUF_CTL[7]`, idle while set; §8.4, §11 phases 5.7 and 6.3.
///
/// `[I915]` `i915_reg.h:3869` (`DDI_BUF_IS_IDLE (1 << 7)`).
const DDI_BUF_IS_IDLE: u32 = 1 << 7;

/// `PORT_CL_DW10[7:4]`, one bit per lane powered down; §8.2, §8.6 step 7.
///
/// `[I915]` `display/intel_combo_phy_regs.h:34` (`PWR_DOWN_LN_MASK` is
/// `REG_GENMASK(7, 4)`), with §8.6 step 7's three HDMI/DVI states at `:35-37`.
const PWR_DOWN_LN_MASK: u32 = 0xf << 4;

/// `PORT_TX_DW5[31]`, the bit whose clear-to-set transition commits the swing
/// values; §8.5 steps 4 and 6.
///
/// `[I915]` `display/intel_combo_phy_regs.h:133` (`TX_TRAINING_EN REG_BIT(31)`)
/// and `display/intel_ddi.c:1218-1229`, which clears it, programs the batch,
/// then sets it again.
const TX_TRAINING_EN: u32 = 1 << 31;

/// The registers one combo PHY port's swing values live in.
///
/// Every offset is the register table's, keyed by the DDI's combo PHY: §8.1's
/// port numbering puts DDI A on combo PHY A and DDI B on combo PHY B, and §6.3
/// pairs them with DPLL0 and DPLL1.  Three of the four dwords live in per-lane
/// instances (`PORT_TX_DW2`, `DW4` and `DW7`), so each is an array in lane
/// order; `PORT_TX_DW5` is read from lane 0, which is the copy i915 reads
/// before writing the group instance (`[I915]` `display/intel_ddi.c:1141`).
#[derive(Clone, Copy, Debug)]
struct SourceRegisters {
    /// `PORT_COMP_DW0`: `COMP_INIT` says §8.3's initialisation has run.
    comp_dw0: Register,
    /// `DDI_BUF_CTL`: enable, idle and the level index.
    ddi_buf_ctl: Register,
    /// `PORT_CL_DW10`: `PWR_DOWN_LN_MASK`.
    cl_dw10: Register,
    /// `PORT_TX_DW2`, one register per lane, in lane order.
    tx_dw2: [Register; 4],
    /// `PORT_TX_DW4`, one register per lane, in lane order.
    tx_dw4: [Register; 4],
    /// `PORT_TX_DW5`, lane 0.
    tx_dw5_lane0: Register,
    /// `PORT_TX_DW7`, one register per lane, in lane order.
    tx_dw7: [Register; 4],
}

/// The registers for a DDI's combo PHY, or `None` for a port this kernel does
/// not have a PHY mapping for.
///
/// §8.1: the combo-PHY ports are A and B; C and D are Type-C/DKL ports, whose
/// PHY is a different family in a different aperture (§8.8 defers it), and the
/// register table declares no `PORT_TX_*` registers for them.  `None` here is
/// what makes [`SwingSource::UnsupportedDdi`] reachable rather than a read of
/// some other port's registers.
const fn source_registers(ddi: Ddi) -> Option<SourceRegisters> {
    match ddi {
        Ddi::A => Some(SourceRegisters {
            comp_dw0: regs::COMBO_PHY_A.comp_dw0,
            ddi_buf_ctl: ddi::DDI_BUF_CTL_A,
            cl_dw10: port::PORT_CL_DW10_A,
            tx_dw2: [
                port::PORT_TX_DW2_LN0_A,
                port::PORT_TX_DW2_LN1_A,
                port::PORT_TX_DW2_LN2_A,
                port::PORT_TX_DW2_LN3_A,
            ],
            tx_dw4: [
                port::PORT_TX_DW4_LN0_A,
                port::PORT_TX_DW4_LN1_A,
                port::PORT_TX_DW4_LN2_A,
                port::PORT_TX_DW4_LN3_A,
            ],
            tx_dw5_lane0: port::PORT_TX_DW5_LN0_A,
            tx_dw7: [
                port::PORT_TX_DW7_LN0_A,
                port::PORT_TX_DW7_LN1_A,
                port::PORT_TX_DW7_LN2_A,
                port::PORT_TX_DW7_LN3_A,
            ],
        }),
        Ddi::B => Some(SourceRegisters {
            comp_dw0: regs::COMBO_PHY_B.comp_dw0,
            ddi_buf_ctl: ddi::DDI_BUF_CTL_B,
            cl_dw10: port::PORT_CL_DW10_B,
            tx_dw2: [
                port::PORT_TX_DW2_LN0_B,
                port::PORT_TX_DW2_LN1_B,
                port::PORT_TX_DW2_LN2_B,
                port::PORT_TX_DW2_LN3_B,
            ],
            tx_dw4: [
                port::PORT_TX_DW4_LN0_B,
                port::PORT_TX_DW4_LN1_B,
                port::PORT_TX_DW4_LN2_B,
                port::PORT_TX_DW4_LN3_B,
            ],
            tx_dw5_lane0: port::PORT_TX_DW5_LN0_B,
            tx_dw7: [
                port::PORT_TX_DW7_LN0_B,
                port::PORT_TX_DW7_LN1_B,
                port::PORT_TX_DW7_LN2_B,
                port::PORT_TX_DW7_LN3_B,
            ],
        }),
        Ddi::C | Ddi::D => None,
    }
}

/// Why a read of the firmware's swing values did not produce a
/// [`SwingProgram`].
///
/// The name is the one the caller composes into its own error: it is the
/// *source* of the swing values, and when the read fails it is the named reason
/// there is nothing to hand to `OutputRequest::with_swing`.  Every variant is a
/// thing a person on the machine can act on, in the style of `output::OutputError`
/// and `phy::PhyError`; `describe()` is the text, and every one of them names
/// the register, the value that was read and the section that explains it.
///
/// None of these is a reason to try harder: they are the evidence that the
/// premise -- "the firmware has this port up, and its registers hold the values
/// that work on this board" -- does not hold.  The alternative to a refusal is
/// a guessed table, which is what §8.5's `[GAP]` forbids.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SwingSource {
    /// The DDI is not a combo-PHY port, so there is no such PHY to read.
    UnsupportedDdi {
        /// The DDI that was asked for.
        ddi: Ddi,
    },
    /// A register could not be read at all.
    Unreadable {
        /// The DDI whose registers were being read.
        ddi: Ddi,
        /// The register's name.
        register: &'static str,
    },
    /// `PORT_COMP_DW0.COMP_INIT` is clear: §8.3's initialisation has not run.
    PhyNotInitialised {
        /// The DDI whose PHY it is.
        ddi: Ddi,
        /// The register's name, `PORT_COMP_DW0(A)` or `(B)`.
        register: &'static str,
        /// What it read.
        readback: u32,
    },
    /// `DDI_BUF_CTL.ENABLE` is clear: the firmware never enabled this port.
    PortNotEnabled {
        /// The DDI.
        ddi: Ddi,
        /// The register's name, `DDI_BUF_CTL(A)` or `(B)`.
        register: &'static str,
        /// What it read.
        readback: u32,
    },
    /// `DDI_BUF_CTL.IS_IDLE` is still set: the port is not driving.
    PortStillIdle {
        /// The DDI.
        ddi: Ddi,
        /// The register's name.
        register: &'static str,
        /// What it read.
        readback: u32,
    },
    /// `PORT_CL_DW10.PWR_DOWN_LN_MASK` has every lane powered down.
    LanesAllPoweredDown {
        /// The DDI.
        ddi: Ddi,
        /// The register's name.
        register: &'static str,
        /// What it read.
        readback: u32,
    },
    /// The swing words all read as an absent block.
    PhyNotResponding {
        /// The DDI.
        ddi: Ddi,
        /// `PORT_TX_DW2`'s read, one value per lane.
        dw2: [u32; 4],
        /// `PORT_TX_DW5`'s read, lane 0.
        dw5: u32,
        /// `PORT_TX_DW7`'s read, one value per lane.
        dw7: [u32; 4],
    },
    /// `PORT_TX_DW5.TX_TRAINING_EN` is clear: §8.5 step 6 never ran.
    TrainingNotEnabled {
        /// The DDI.
        ddi: Ddi,
        /// The register's name.
        register: &'static str,
        /// What it read.
        readback: u32,
    },
}

impl fmt::Display for SwingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

impl SwingSource {
    /// The refusal in words a person on the machine can act on.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::UnsupportedDdi { ddi } => format!(
                "DDI {} is not a combo-PHY port, so there is no PHY holding this board's \
                 buffer-translation values to read.  Reference section 8.1: the combo-PHY ports \
                 are A and B; C and D are Type-C/DKL ports, whose PHY is a different family in a \
                 different aperture (section 8.8 defers it), and the register table declares no \
                 PORT_TX registers for them.  Nothing was read",
                ddi.name()
            ),
            Self::Unreadable { ddi, register } => format!(
                "{register} for DDI {} could not be read, so the firmware's buffer-translation \
                 values cannot be recovered from it.  Reference section 2.2: a register that \
                 cannot be read is not a register that read zero, and section 12.2's absent-block \
                 test is the same distinction.  Nothing was believed",
                ddi.name()
            ),
            Self::PhyNotInitialised {
                ddi,
                register,
                readback,
            } => format!(
                "{register} reads {readback:#010x} and its COMP_INIT bit is clear, so DDI {}'s \
                 combo PHY has not been through section 8.3's initialisation.  A PHY that was \
                 never initialised has no firmware-programmed swing values to read -- only \
                 whatever a reset left in the registers.  Reference section 8.3 steps 2 and 6, \
                 section 12.2's presence and initialisation read, and phy.rs's init_one, which \
                 treats COMP_INIT set as 'already initialised'.  Nothing was believed",
                ddi.name()
            ),
            Self::PortNotEnabled {
                ddi,
                register,
                readback,
            } => format!(
                "{register} reads {readback:#010x} and its ENABLE bit (bit 31) is clear, so the \
                 firmware never enabled DDI {}'s buffer.  Whatever the PHY's TX registers hold is \
                 not a program for a live port -- it may be a reset value or a mode that was torn \
                 down.  Reference section 8.6 steps 13 and 14 and section 11 phase 5.7.  Nothing \
                 was believed",
                ddi.name()
            ),
            Self::PortStillIdle {
                ddi,
                register,
                readback,
            } => format!(
                "{register} reads {readback:#010x} and its IS_IDLE bit (bit 7) is still set, so \
                 DDI {} is not driving anything.  Section 11 phase 5.7 calls IS_IDLE the single \
                 best \"is my DDI alive\" bit on the chip -- it stays 1 when the DDI has no clock \
                 -- and phase 6.3 makes it the read-back that proves the port is scanning.  A \
                 port that is idle has no working configuration to copy.  Nothing was believed",
                ddi.name()
            ),
            Self::LanesAllPoweredDown {
                ddi,
                register,
                readback,
            } => format!(
                "{register} reads {readback:#010x} and its PWR_DOWN_LN_MASK (bits [7:4]) has \
                 every lane of DDI {} powered down, while DDI_BUF_CTL says the port is enabled \
                 and not idle.  Section 8.6 step 7 powers the lanes before step 13 enables the \
                 buffer, so a port in that state is not one this sequence left behind; one of the \
                 two registers is not saying what we think it is.  Nothing was believed",
                ddi.name()
            ),
            Self::PhyNotResponding { ddi, dw2, dw5, dw7 } => format!(
                "the swing registers of DDI {} all read as an absent block: PORT_TX_DW2 per lane \
                 {dw2:#010x?}, PORT_TX_DW5 (lane 0) {dw5:#010x}, PORT_TX_DW7 per lane \
                 {dw7:#010x?}.  Reference section 12.2: a block that is not there answers \
                 all-zero or all-ones, and the TX registers are the one thing this read cannot do \
                 without.  Nothing was believed",
                ddi.name()
            ),
            Self::TrainingNotEnabled {
                ddi,
                register,
                readback,
            } => format!(
                "{register} reads {readback:#010x} and its TX_TRAINING_EN bit (bit 31) is clear \
                 on DDI {}, so section 8.5's sequence never reached step 6 -- the write that \
                 commits the swing and de-emphasis values and triggers the update ([I915] \
                 display/intel_ddi.c:1226-1229).  Without it the register is either mid-sequence \
                 or was never programmed, and the training-disabled state this module derives \
                 from it would be a state the sequence never wrote.  Nothing was believed",
                ddi.name()
            ),
        }
    }
}

/// Read a port's buffer-translation values back out of the PHY the firmware
/// programmed.
///
/// This is §13.4's route applied to §8.5's `[GAP]`: on the target machine the
/// firmware has already brought this port up (the screen is the only console),
/// so its `PORT_TX_DW2`/`DW4`/`DW5`/`DW7` hold the values that work on this
/// board, and phase 5 can replay them instead of inventing a table.
///
/// The returned [`SwingProgram`] is what `OutputRequest::with_swing` takes:
///
/// ```text
/// output::OutputRequest::hdmi(ddi, mode, encoding)
///     .with_swing(swing::read_firmware_swing(regs, ddi)?)
/// ```
///
/// `level` is `DDI_BUF_CTL`'s `BUF_TRANS_SELECT` field, which is the only
/// statement of the level index the chip carries; `source` names the DDI and
/// the level, so a log line says where the numbers came from.
///
/// On any failure the caller must log [`SwingSource::describe`] and **not**
/// program the output: a guessed table is exactly what the `[GAP]` forbids, and
/// the module documentation says so at length.
pub(crate) fn read_firmware_swing<R: Registers>(
    regs: &R,
    ddi: Ddi,
) -> Result<SwingProgram, SwingSource> {
    // §8.1: only combo PHY A and B have ports this kernel's register table can
    // address, so a Type-C port is refused before a single read.
    let Some(port) = source_registers(ddi) else {
        return Err(SwingSource::UnsupportedDdi { ddi });
    };

    // §8.3 steps 2 and 6: COMP_INIT is the PHY's own record that the
    // initialisation sequence ran, and §12.2 reads it to decide whether a PHY
    // exists and is initialised.  Without it the TX registers hold a reset or
    // stale state rather than a firmware program.
    let comp_dw0 = read(regs, ddi, port.comp_dw0)?;
    if comp_dw0 & COMP_INIT == 0 {
        return Err(SwingSource::PhyNotInitialised {
            ddi,
            register: port.comp_dw0.name(),
            readback: comp_dw0,
        });
    }

    // §8.6 steps 13 and 14, and §11 phases 5.7 and 6.3: the port is enabled
    // and no longer idle, which is the state the sequence leaves and the state
    // a working picture is scanned out of.
    let ddi_buf_ctl = read(regs, ddi, port.ddi_buf_ctl)?;
    if ddi_buf_ctl & DDI_BUF_CTL_ENABLE == 0 {
        return Err(SwingSource::PortNotEnabled {
            ddi,
            register: port.ddi_buf_ctl.name(),
            readback: ddi_buf_ctl,
        });
    }
    if ddi_buf_ctl & DDI_BUF_IS_IDLE != 0 {
        return Err(SwingSource::PortStillIdle {
            ddi,
            register: port.ddi_buf_ctl.name(),
            readback: ddi_buf_ctl,
        });
    }

    // §8.6 step 7 powers the lanes before step 13 enables the buffer.  This
    // checks the one state those two cannot both be in; the full lane-for-lane
    // agreement with DDI_BUF_CTL's width is deliberately not a refusal (see the
    // module documentation).
    let cl_dw10 = read(regs, ddi, port.cl_dw10)?;
    if cl_dw10 & PWR_DOWN_LN_MASK == PWR_DOWN_LN_MASK {
        return Err(SwingSource::LanesAllPoweredDown {
            ddi,
            register: port.cl_dw10.name(),
            readback: cl_dw10,
        });
    }

    // §8.5 step 5's swing values, read the way i915 writes them: `PORT_TX_DW2`,
    // `DW4` and `DW7` one lane at a time, `ln = 0..3`, and never through a group
    // instance (`[I915]` `display/intel_ddi.c:1148-1157`, `:1161-1169`,
    // `:1171-1178`).  For `DW4` §8.5 step 2 says group access must not be used
    // because each lane's loadgen select differs, and i915's comment on the same
    // write is that it "would overwrite individual loadgen"
    // (`[I915]` `display/intel_ddi.c:1160`).  A group read would collapse four
    // values into one, whichever lane the hardware answered with.
    let mut dw2 = [0u32; 4];
    for (lane, register) in port.tx_dw2.iter().enumerate() {
        dw2[lane] = read(regs, ddi, *register)?;
    }
    let mut dw4 = [0u32; 4];
    for (lane, register) in port.tx_dw4.iter().enumerate() {
        dw4[lane] = read(regs, ddi, *register)?;
    }
    // §8.5 steps 4 and 6 as i915 performs them: the value that is modified is
    // lane 0's, the write goes to the group instance
    // (`[I915]` `display/intel_ddi.c:1218-1229`).
    let dw5 = read(regs, ddi, port.tx_dw5_lane0)?;
    let mut dw7 = [0u32; 4];
    for (lane, register) in port.tx_dw7.iter().enumerate() {
        dw7[lane] = read(regs, ddi, *register)?;
    }

    // §12.2: a block that is not there answers all-zero or all-ones.  Every lane
    // of both per-lane dwords and lane 0's `DW5` degenerate at once is that, and
    // there is nothing to copy.
    if degenerate_all(&dw2) && degenerate(dw5) && degenerate_all(&dw7) {
        return Err(SwingSource::PhyNotResponding { ddi, dw2, dw5, dw7 });
    }

    // §8.5 step 6, and [I915] display/intel_ddi.c:1226-1229: the set of
    // TX_TRAINING_EN is the write that commits the batch.  It is also the bit
    // the training-disabled state is derived by clearing, so a read without it
    // is not a state this module can invert.
    if dw5 & TX_TRAINING_EN == 0 {
        return Err(SwingSource::TrainingNotEnabled {
            ddi,
            register: port.tx_dw5_lane0.name(),
            readback: dw5,
        });
    }

    // §8.4: the level is `BUF_TRANS_SELECT[27:24]`.  It is the index the
    // firmware's own enable write carried (section 8.6 step 13) and the field
    // phase 5 writes back, so the replay reproduces it.  See the module
    // documentation for what this field does and does not prove on Gen12.
    let level = ((ddi_buf_ctl & DDI_BUF_TRANS_SELECT_MASK) >> DDI_BUF_TRANS_SELECT_SHIFT) as u8;

    Ok(SwingProgram {
        level,
        dw2,
        dw4,
        dw5_training_disabled: dw5 & !TX_TRAINING_EN,
        dw5_training_enabled: dw5,
        dw7,
        source: read_back_source(ddi, level),
    })
}

/// Read one register, naming the refusal if the access did not happen.
fn read(regs: &impl Registers, ddi: Ddi, register: Register) -> Result<u32, SwingSource> {
    regs.read(register).ok_or(SwingSource::Unreadable {
        ddi,
        register: register.name(),
    })
}

/// Whether a word looks like an absent block rather than a programmed register.
///
/// Reference §12.2: "A read of `0xFFFFFFFF` or `0x00000000` on **both** the read
/// and a re-read means **the PHY instance is absent**."  This read asks the
/// question of every swing word at once; a single zero or all-ones word among
/// sane neighbours is left alone, because these registers legitimately hold zero
/// in some fields.
fn degenerate(value: u32) -> bool {
    value == 0 || value == u32::MAX
}

/// The same question for a dword that has one instance per lane.
///
/// Four lanes of one dword are four instances; §12.2's absent-block answer is
/// all of them, not one of them.
fn degenerate_all(values: &[u32; 4]) -> bool {
    values.iter().all(|value| degenerate(*value))
}

/// The `source` strings a read-back program carries, one per DDI and level.
///
/// [`SwingProgram::source`] is a `&'static str` and both facts a caller wants in
/// it -- which DDI, which level -- are runtime values.  Formatting them would
/// need a `String`, and a `String` cannot reach a `&'static str` field without
/// leaking it, which is not something a boot path should do to write a log
/// line.  The levels are four bits wide, so the set is finite and is written
/// out.
const SOURCES_A: [&str; 16] = [
    "read back from the PHY the firmware programmed, DDI A level 0",
    "read back from the PHY the firmware programmed, DDI A level 1",
    "read back from the PHY the firmware programmed, DDI A level 2",
    "read back from the PHY the firmware programmed, DDI A level 3",
    "read back from the PHY the firmware programmed, DDI A level 4",
    "read back from the PHY the firmware programmed, DDI A level 5",
    "read back from the PHY the firmware programmed, DDI A level 6",
    "read back from the PHY the firmware programmed, DDI A level 7",
    "read back from the PHY the firmware programmed, DDI A level 8",
    "read back from the PHY the firmware programmed, DDI A level 9",
    "read back from the PHY the firmware programmed, DDI A level 10",
    "read back from the PHY the firmware programmed, DDI A level 11",
    "read back from the PHY the firmware programmed, DDI A level 12",
    "read back from the PHY the firmware programmed, DDI A level 13",
    "read back from the PHY the firmware programmed, DDI A level 14",
    "read back from the PHY the firmware programmed, DDI A level 15",
];

/// The same strings for combo PHY B.
const SOURCES_B: [&str; 16] = [
    "read back from the PHY the firmware programmed, DDI B level 0",
    "read back from the PHY the firmware programmed, DDI B level 1",
    "read back from the PHY the firmware programmed, DDI B level 2",
    "read back from the PHY the firmware programmed, DDI B level 3",
    "read back from the PHY the firmware programmed, DDI B level 4",
    "read back from the PHY the firmware programmed, DDI B level 5",
    "read back from the PHY the firmware programmed, DDI B level 6",
    "read back from the PHY the firmware programmed, DDI B level 7",
    "read back from the PHY the firmware programmed, DDI B level 8",
    "read back from the PHY the firmware programmed, DDI B level 9",
    "read back from the PHY the firmware programmed, DDI B level 10",
    "read back from the PHY the firmware programmed, DDI B level 11",
    "read back from the PHY the firmware programmed, DDI B level 12",
    "read back from the PHY the firmware programmed, DDI B level 13",
    "read back from the PHY the firmware programmed, DDI B level 14",
    "read back from the PHY the firmware programmed, DDI B level 15",
];

/// The `source` string for a level read back from `ddi`.
///
/// `get` rather than indexing: the level comes from a four-bit mask and cannot
/// be out of range, but a panic on this path would be a kernel that dies
/// reading a register, and the two `None` arms say what happened instead.
fn read_back_source(ddi: Ddi, level: u8) -> &'static str {
    let table: &[&'static str; 16] = match ddi {
        Ddi::A => &SOURCES_A,
        Ddi::B => &SOURCES_B,
        // `read_firmware_swing` refuses C and D before this is reached; named
        // so that a future caller cannot turn that into a panic.
        Ddi::C | Ddi::D => {
            return "read back refused: the DDI is not a combo-PHY port";
        }
    };
    match table.get(usize::from(level)) {
        Some(source) => source,
        None => "read back from the PHY the firmware programmed, level out of range",
    }
}

#[cfg(test)]
mod tests;
