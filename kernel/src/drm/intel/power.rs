//! Display power: the fuse readback, the power wells, the DC states, the
//! display buffer slices, and the order they have to come up in.
//!
//! [`bring_up`] is the single entry point.  It performs reference §11 phases 0.3
//! and 1 in the order §4.9's `icl_display_core_init` uses, logs every step with
//! what it read back, and returns what it observed:
//!
//! ```text
//! 0.3  read SKL_DFSM, SFUSE_STRAP, SKL_DSSM, SKL_FUSE_STATUS and log them
//! 1.1  DC_STATE_EN <- DC disabled
//! 1.2  combo PHY init, PHY A first, every PHY present
//! 1.3  PW_1: Wa_16013190616, poll PG0, request, poll STATE, poll PG1
//!      and then re-read each PHY's COMP_INIT, which is what says whether the
//!      PHY step actually took (section 11 phase 1.2)
//! 1.4  CDCLK: keep the firmware's if it is usable, otherwise program the
//!      lowest value the platform's table allows
//! 1.4b PCH_RAWCLK_FREQ, from the SFUSE_STRAP strap
//! 1.5  DBUF slices: read the state, request only the slices that are off
//! 1.6  Wa_14011508470 in GEN11_CHICKEN_DCPR_2, and XELPD_DISPLAY_ERR_FATAL_MASK
//!      deliberately left unmasked
//! ```
//!
//! ## Why the order is the deliverable
//!
//! **A power well that is down drops writes and answers reads with zero.**
//! Reference §4.2 states this and §4.1 draws the dependency tree; it is why a
//! wrong sequence presents as "every register reads 0", which looks like a dead
//! device rather than a power bug.  So the sequence is not a preference: it is
//! the difference between a register read that means something and one that
//! does not, and every step below is ordered by that.
//!
//! On `XE_LPD` the wells are a **tree**, not the chain earlier generations used
//! (§4.2.1): `PG0` at the root, `PW_1` under it, then `PW_A` and `PW_2` as
//! siblings, and `PW_B`/`PW_C`/`PW_D` under `PW_2`.  They are enabled from the
//! root down and disabled from the leaves up.  `PW_1` is the one this phase
//! enables, because transcoder A, DDI A and DDI B are inside it (§4.3), which
//! is what makes pipe A plus DDI A the cheapest configuration to bring up.
//!
//! The well indices are per-project and this is the single most dangerous place
//! in the slice to generalise: Tiger Lake, Rocket Lake and ADL-P/N are all
//! Gen12 and all three map the same bits differently (§4.2.1).  The table here
//! is `XE_LPD`'s — `PW_1` = 0, `PW_2` = 1, `PW_A`..`PW_D` = 5..8 — from
//! `[I915]` `i915_reg.h:3650-3660` and `display/intel_display_power_map.c`.
//!
//! ## Which poll failures are fatal, and why not all of them
//!
//! Every poll here is bounded and every failure is reported; they differ in
//! whether the sequence can continue.
//!
//! * **`PW_1`'s `STATE` bit is fatal.**  It is the hardware's own statement
//!   that the well is up.  Continuing past it would read zeros for the rest of
//!   the bring-up and produce exactly the "dead device" confusion §4.2 warns
//!   about.
//! * **The `PG0` fuse is fatal.**  It is the root of the tree: nothing under it
//!   is powered if it is not distributed, and §11 phase 1.3 lists it as a
//!   precondition of the request.
//! * **A well's own `PG` fuse is recorded, not fatal.**  `[I915]`
//!   `gen9_wait_for_power_well_fuses` warns and continues, and for the wells
//!   above `PW_1` the fuse bit position is an `[INF]`: §4.4 derives `PG6`..`PG9`
//!   for indices 5..8 from `SKL_FUSE_PG_DIST_STATUS(pg) = 1 << (27 - pg)`, and
//!   §13.1 item 1 says the whole ADL-N power-well map has to be verified on
//!   hardware rather than inferred.  Refusing to continue on an inferred bit
//!   position would turn a documented unknown into a boot failure, while the
//!   `STATE` bit already answers the question that matters.
//! * **A DBUF slice's `POWER_STATE` is recorded**; the step is fatal only if
//!   *no* slice comes up.  Which slices a given SKU has is `[GAP]` (§4.7,
//!   §13.1 item 3) and i915's own position is "just power up at least 1 slice,
//!   we will figure out later which slices we have and what we need".  A DBUF
//!   with no slice at all is a display that underruns silently, which is fatal
//!   by any reading.
//! * **The PHY's `COMP_INIT` readback is recorded**, because it happens before
//!   `PW_1` exists and a zero here is expected (§11 phase 1.2 says the cause is
//!   the missing well).  [`bring_up`] re-reads it after `PW_1` and reports both.
//!
//! ## What is not here
//!
//! `icl_display_core_init` also initialises MBUS (step 6) and the memory
//! arbiter's `BW_BUDDY` registers (step 7).  Neither is in §11's phase 1 and
//! neither belongs to this slice; if the display underruns or the memory
//! arbiter misbehaves once a pipe is running, they are the first unported steps
//! to look at.  Noted in `docs/design/intel-power.md`.

use alloc::{format, string::String, vec::Vec};

use super::{
    clk,
    phy::{self, PhyState},
    regs::{self, Register, Registers},
};

// ---------------------------------------------------------------------------
// Bit positions, each cited to the reference section and the i915 line it was
// read from.  They live here rather than in `regs` because they are field
// layouts, which is what the sequences in this module are written against.
// ---------------------------------------------------------------------------

/// `SKL_DFSM[30]` — pipe A is fused off.  Reference §3.4.
pub(crate) const SKL_DFSM_PIPE_A_DISABLE: u32 = 1 << 30;
/// `SKL_DFSM[21]` — pipe B is fused off.  Reference §3.4.
pub(crate) const SKL_DFSM_PIPE_B_DISABLE: u32 = 1 << 21;
/// `SKL_DFSM[28]` — pipe C is fused off.  Reference §3.4.
pub(crate) const SKL_DFSM_PIPE_C_DISABLE: u32 = 1 << 28;
/// `SKL_DFSM[22]` — pipe D is fused off, Gen12 and later.  Reference §3.4.
pub(crate) const SKL_DFSM_PIPE_D_DISABLE: u32 = 1 << 22;
/// `SKL_DFSM[27]` — FBC is unavailable.  Reference §3.4.
pub(crate) const SKL_DFSM_DISPLAY_PM_DISABLE: u32 = 1 << 27;
/// `SKL_DFSM[25]` — HDCP is unavailable.  Reference §3.4.
pub(crate) const SKL_DFSM_DISPLAY_HDCP_DISABLE: u32 = 1 << 25;
/// `SKL_DFSM[7]` — DSC is unavailable.  Reference §3.4.
pub(crate) const SKL_DFSM_DISPLAY_DSC_DISABLE: u32 = 1 << 7;
/// `SKL_DFSM[23]` — the DMC is unavailable.  Reference §3.4.
pub(crate) const SKL_DFSM_DMC_DISABLE: u32 = 1 << 23;
/// `SFUSE_STRAP[13]` — the fuse strap is locked.  Reference §3.5.
pub(crate) const SFUSE_STRAP_FUSE_LOCK: u32 = 1 << 13;
/// `SFUSE_STRAP[7]` — the SKU declares itself headless.  Reference §3.5.
pub(crate) const SFUSE_STRAP_DISPLAY_DISABLED: u32 = 1 << 7;
/// `SFUSE_STRAP[6]` — the CRT port is disabled.  Reference §3.5.
pub(crate) const SFUSE_STRAP_CRT_DISABLED: u32 = 1 << 6;

/// `HSW_PWR_WELL_CTL_REQ(i)`: the request bit for well index `i`.
///
/// `[I915]` `i915_reg.h:3630`.
pub(crate) const fn well_request(index: u32) -> u32 {
    0x2 << (index * 2)
}

/// `HSW_PWR_WELL_CTL_STATE(i)`: the state bit for well index `i`.
///
/// `[I915]` `i915_reg.h:3631`.  This is the bit that answers "is the well on",
/// and it is the only one whose absence stops the sequence.
pub(crate) const fn well_state(index: u32) -> u32 {
    0x1 << (index * 2)
}

/// `SKL_FUSE_PG_DIST_STATUS(pg)`: whether power gate `pg` is distributed.
///
/// `[I915]` `i915_reg.h:3738`.  `SKL_PG0` = 0 and `SKL_PG1` = 1 are the two
/// this phase polls; the tooling for the higher wells uses the same macro,
/// which is why the index arithmetic lives in [`Well::pg`].
pub(crate) const fn fuse_pg_dist_status(pg: u8) -> u32 {
    1 << (27 - pg as u32)
}

/// `SKL_PG0`, the root power gate.  Reference §4.4.
pub(crate) const SKL_PG0: u8 = 0;
/// `SKL_PG1`, the gate `PW_1` corresponds to.  Reference §4.4.
pub(crate) const SKL_PG1: u8 = 1;

/// `GEN8_CHICKEN_DCPR_1[15]`, set before the `PW_1` request by
/// `Wa_16013190616`.  Reference §4.5 item 1; `[I915]` `i915_reg.h:2845`.
pub(crate) const DISABLE_FLR_SRC: u32 = 1 << 15;

/// The four bits `Wa_14011508470` sets in `GEN11_CHICKEN_DCPR_2`.
///
/// Reference §4.5 item 2 and §4.9 step 10; `[I915]` `i915_reg.h:2850-2853`.
pub(crate) const DCPR_CLEAR_MEMSTAT_DIS: u32 = 1 << 24;
pub(crate) const DCPR_SEND_RESP_IMM: u32 = 1 << 25;
pub(crate) const DCPR_MASK_LPMODE: u32 = 1 << 26;
pub(crate) const DCPR_MASK_MAXLATENCY_MEMUP_CLR: u32 = 1 << 27;

/// Everything `Wa_14011508470` asks for.
pub(crate) const WA_14011508470_BITS: u32 =
    DCPR_CLEAR_MEMSTAT_DIS | DCPR_SEND_RESP_IMM | DCPR_MASK_LPMODE | DCPR_MASK_MAXLATENCY_MEMUP_CLR;

/// `DBUF_CTL_S*[31]`, the request bit.  Reference §4.7.
pub(crate) const DBUF_POWER_REQUEST: u32 = 1 << 31;
/// `DBUF_CTL_S*[30]`, the state bit.  Reference §4.7.
pub(crate) const DBUF_POWER_STATE: u32 = 1 << 30;

/// The `DC_STATE_EN` field software owns, for display version 13.
///
/// `[I915]` `gen9_dc_mask` (`display/intel_display_power_well.c:684-700`) for
/// `DISPLAY_VER >= 12`: `UPTO_DC5 | UPTO_DC6 | DC9 | DC3CO`.  Everything else
/// in the register is either status or a hardware-communication bit that
/// software must not change — §12.1 says bits 9, 8 and 4 in particular — which
/// is why disabling DC states is a read-modify-write of this field rather than
/// a write of zero.
pub(crate) const DC_STATE_MASK: u32 = 0b1011 | (1 << 30);

/// `DC_STATE_DISABLE`: the field value that asks for no DC state at all.
pub(crate) const DC_STATE_DISABLE: u32 = 0;

/// How long to wait for a well's `STATE` bit.
///
/// Two sources give two figures: `[PRM]`'s "Initialize Sequence" step 3 says
/// 30 us for a 38.4 MHz reference and 45 us for a 24 MHz one, while `[I915]`
/// `hsw_wait_for_power_well_enable` uses `enable_timeout ?: 1` ms.  The
/// vendor driver's figure is used because a well that comes up late is not a
/// failure and a bound that is too tight would refuse hardware that works.
/// Reference §4.4.
pub(crate) const WELL_STATE_TIMEOUT_US: u32 = 1_000;

/// How long to wait for a power gate's fuse distribution bit.
///
/// `[PRM]` gives 20 us; `[I915]` `gen9_wait_for_power_well_fuses` passes its
/// generic 1 ms and only warns.  As above, the longer bound.  Reference §4.4.
pub(crate) const WELL_FUSE_TIMEOUT_US: u32 = 1_000;

/// How long to wait for a DBUF slice's `POWER_STATE` bit.
///
/// `[PRM]` "Initialize Sequence" step 5 gives 10 us, and `[I915]`
/// `gen9_dbuf_slice_set` delays 10 us and then reads the state once.  Polling
/// within the same budget is equivalent in effect and stricter about the
/// outcome, because it reports the value it actually saw.
pub(crate) const DBUF_STATE_TIMEOUT_US: u32 = 10;

/// How many times the `DC_STATE_EN` write may be retried.
///
/// `[I915]` `gen9_write_dc_state` rewrites up to 100 times, because the
/// hardware is documented to ignore a DC state change while it is still
/// restoring register state.  Reference §12.1.
pub(crate) const DC_STATE_ATTEMPTS: u32 = 100;

/// One power well: its name, where its request bit lives, its index, and which
/// power gate's fuse bit announces it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Well {
    pub(crate) name: &'static str,
    /// The *driver's* request register.  There are four requesters and hardware
    /// OR-s them, so the driver uses `HSW_PWR_WELL_CTL2` and leaves the BIOS,
    /// KVMR and debug registers alone.  Reference §4.2.
    pub(crate) register: Register,
    /// The well index within that register.
    pub(crate) index: u32,
    /// The power gate whose fuse bit is polled after the state bit, or `None`
    /// for a well with no fuses.  `[I915]` computes it as
    /// `idx - ICL_PW_CTL_IDX_PW_1 + SKL_PG1`, i.e. `idx + 1` in this table.
    pub(crate) pg: Option<u8>,
    pub(crate) timeout_us: u32,
}

impl Well {
    pub(crate) const fn request_mask(self) -> u32 {
        well_request(self.index)
    }

    pub(crate) const fn state_mask(self) -> u32 {
        well_state(self.index)
    }
}

/// `PW_1`, the well this phase enables.
///
/// Index 0 in `HSW_PWR_WELL_CTL2`, request bit `0x2`, state bit `0x1`, and fuse
/// `PG1`.  Reference §4.2 and §11 phase 1.3; `[I915]` `i915_reg.h:3654` and
/// `display/intel_display_power_map.c:715-725`.
pub(crate) const PW_1: Well = Well {
    name: "PW_1",
    register: regs::HSW_PWR_WELL_CTL2,
    index: 0,
    pg: Some(SKL_PG1),
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// `PW_2`, the parent of `PW_B`..`PW_D` and of the south display's port set.
///
/// Not enabled by [`bring_up`]: it is needed for pipes B..D and for the
/// south display, which are later phases.  Its fuse index is the `[INF]` one
/// (`PG2` is documented, and the arithmetic gives `idx + 1`).  Reference §4.1
/// and §4.3.
pub(crate) const PW_2: Well = Well {
    name: "PW_2",
    register: regs::HSW_PWR_WELL_CTL2,
    index: 1,
    pg: Some(2),
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// `PW_A`..`PW_D`, one per pipe, each with a fuse poll.
///
/// Index 5..8 and `PG6`..`PG9`.  The reference marks the `PG6`..`PG9` *naming*
/// `[INF]` — the arithmetic follows from `[I915]`'s generic
/// `SKL_FUSE_PG_DIST_STATUS(pg)` — which is why a failure to see one of these
/// bits is recorded rather than fatal.  Reference §4.2 and §4.4.
pub(crate) const PW_A: Well = Well {
    name: "PW_A",
    register: regs::HSW_PWR_WELL_CTL2,
    index: 5,
    pg: Some(6),
    timeout_us: WELL_STATE_TIMEOUT_US,
};
/// `PW_B`, the well for pipe B.  See [`PW_A`].
pub(crate) const PW_B: Well = Well {
    name: "PW_B",
    register: regs::HSW_PWR_WELL_CTL2,
    index: 6,
    pg: Some(7),
    timeout_us: WELL_STATE_TIMEOUT_US,
};
/// `PW_C`, the well for pipe C.  See [`PW_A`].
pub(crate) const PW_C: Well = Well {
    name: "PW_C",
    register: regs::HSW_PWR_WELL_CTL2,
    index: 7,
    pg: Some(8),
    timeout_us: WELL_STATE_TIMEOUT_US,
};
/// `PW_D`, the well for pipe D.  See [`PW_A`].
pub(crate) const PW_D: Well = Well {
    name: "PW_D",
    register: regs::HSW_PWR_WELL_CTL2,
    index: 8,
    pg: Some(9),
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// `DDI_IO_A`, the IO well for DDIs A and B.
///
/// `[GAP]` §4.2: which of `DDI_IO_C`..`DDI_IO_E` and `AUX_C`..`AUX_E` exist on
/// ADL-N is not established, because `[I915]`'s table describes the maximum
/// `XE_LPD` configuration.  Only the ports whose names come from the same
/// `xe_lpd_display` port mask as PHY A and PHY B are declared here.
pub(crate) const DDI_IO_A: Well = Well {
    name: "DDI_IO_A",
    register: regs::ICL_PWR_WELL_CTL_DDI2,
    index: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// `DDI_IO_B`.  See [`DDI_IO_A`].
pub(crate) const DDI_IO_B: Well = Well {
    name: "DDI_IO_B",
    register: regs::ICL_PWR_WELL_CTL_DDI2,
    index: 1,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// `AUX_A`, the AUX channel power well for port A.
///
/// §11 phase 2.1: enabling this is a precondition of GMBUS or AUX working on
/// that pin pair, which is the sink workstream's step.  It is declared here
/// because the well machinery is generic over the request register, so that
/// step needs a name rather than a new mechanism.
pub(crate) const AUX_A: Well = Well {
    name: "AUX_A",
    register: regs::ICL_PWR_WELL_CTL_AUX2,
    index: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// `AUX_B`.  See [`AUX_A`].
pub(crate) const AUX_B: Well = Well {
    name: "AUX_B",
    register: regs::ICL_PWR_WELL_CTL_AUX2,
    index: 1,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// The four request registers, in the order the diagnostic line prints them.
///
/// i915 reports which requester is holding a well on when a disable does not
/// take, and §11 phase 1.3 tells a reader whose `STATE` never set to read all
/// four and compare.  Doing it for every well means the number is in the log
/// before anyone has to go looking.
pub(crate) const REQUEST_REGISTERS: [Register; 4] = [
    regs::HSW_PWR_WELL_CTL1,
    regs::HSW_PWR_WELL_CTL2,
    regs::HSW_PWR_WELL_CTL3,
    regs::HSW_PWR_WELL_CTL4,
];

/// The DBUF slice registers, in the order this driver enables them.
///
/// All four, because `XE_LPD`'s slice mask is `S1|S2|S3|S4` (`[I915]`
/// `XE_LPD_FEATURES`, `display/intel_display_device.c:1023-1024`) and because
/// §4.7's advice for a first bring-up is to sidestep the slice-renumbering
/// confusion by enabling all of them.  Whether a given SKU populates all four
/// is `[GAP]` §13.1 item 3, which is why a slice that never comes up is
/// recorded rather than fatal as long as one does.
pub(crate) const DBUF_SLICES: [Register; 4] = [
    regs::DBUF_CTL_S0,
    regs::DBUF_CTL_S1,
    regs::DBUF_CTL_S2,
    regs::DBUF_CTL_S3,
];

/// What the fuse and strap reads said, in the order §11 phase 0.3 asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FuseState {
    pub(crate) dfsm: u32,
    pub(crate) dssm: u32,
    pub(crate) sfuse_strap: u32,
    pub(crate) fuse_status: u32,
}

impl FuseState {
    /// The pipes that are not fused off, as bits 0..3 for pipes A..D.
    ///
    /// Bit positions from §3.4: A is `1 << 30`, C `1 << 28`, D `1 << 22`
    /// (Gen12+) and B `1 << 21`.
    pub(crate) fn pipe_mask(&self) -> u8 {
        let mut mask = 0b1111;
        for (bit, disable) in [
            (0, SKL_DFSM_PIPE_A_DISABLE),
            (1, SKL_DFSM_PIPE_B_DISABLE),
            (2, SKL_DFSM_PIPE_C_DISABLE),
            (3, SKL_DFSM_PIPE_D_DISABLE),
        ] {
            if self.dfsm & disable != 0 {
                mask &= !(1 << bit);
            }
        }
        mask
    }

    /// `SKL_DFSM`'s "this engine does not exist" bits, as `(name, disabled)`.
    pub(crate) fn absent_engines(&self) -> [(&'static str, bool); 4] {
        [
            ("DMC", self.dfsm & SKL_DFSM_DMC_DISABLE != 0),
            ("FBC", self.dfsm & SKL_DFSM_DISPLAY_PM_DISABLE != 0),
            ("DSC", self.dfsm & SKL_DFSM_DISPLAY_DSC_DISABLE != 0),
            ("HDCP", self.dfsm & SKL_DFSM_DISPLAY_HDCP_DISABLE != 0),
        ]
    }

    /// Whether the SKU's strap says the display is fused off entirely.
    pub(crate) fn display_disabled(&self) -> bool {
        self.sfuse_strap & SFUSE_STRAP_DISPLAY_DISABLED != 0
    }
}

/// What a fuse poll found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FusePoll {
    /// The distribution bit set within the budget.
    Distributed { readback: u32 },
    /// It did not.  `pg` is the power gate, so a log line can name the bit that
    /// should have been at `1 << (27 - pg)`.
    NotDistributed { pg: u8, readback: u32 },
}

impl FusePoll {
    pub(crate) fn distributed(self) -> bool {
        matches!(self, Self::Distributed { .. })
    }

    pub(crate) fn describe(self, what: &str) -> String {
        match self {
            Self::Distributed { readback } => {
                format!("{what} distributed (FUSE_STATUS {readback:#010x})")
            }
            Self::NotDistributed { pg, readback } => format!(
                "{what} NOT distributed within {WELL_FUSE_TIMEOUT_US} us: SKL_FUSE_STATUS reads \
                 {readback:#010x}, so bit {} (1 << (27 - {pg})) is clear",
                27 - pg as u32,
            ),
        }
    }
}

/// Which of the four requesters hold a well's request bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Requesters {
    pub(crate) bios: bool,
    pub(crate) driver: bool,
    pub(crate) kvmr: bool,
    pub(crate) debug: bool,
}

impl Requesters {
    pub(crate) fn describe(self) -> String {
        format!(
            "requesters: bios {} driver {} kvmr {} debug {}",
            u8::from(self.bios),
            u8::from(self.driver),
            u8::from(self.kvmr),
            u8::from(self.debug),
        )
    }
}

/// What enabling one well did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WellObservation {
    pub(crate) name: &'static str,
    pub(crate) index: u32,
    /// The request register before the request was added, which says whether
    /// the firmware had already asked for this well.
    pub(crate) control_before: u32,
    pub(crate) control_after: u32,
    /// Whether the state bit was already set before this driver asked.
    pub(crate) already_on: bool,
    /// The `PG0` poll, which happens only for `PW_1` — `[I915]`
    /// `hsw_power_well_enable` waits for `PG0` only when `pg == SKL_PG1`.
    pub(crate) pg0: Option<FusePoll>,
    pub(crate) pg: Option<FusePoll>,
    pub(crate) requesters: Requesters,
}

impl WellObservation {
    pub(crate) fn describe(&self) -> String {
        let mut text = format!(
            "power well {} (index {}, request {:#x}, state {:#x}): state {}, control {:#010x} -> \
             {:#010x}",
            self.name,
            self.index,
            well_request(self.index),
            well_state(self.index),
            if self.already_on {
                "was already on"
            } else {
                "came up"
            },
            self.control_before,
            self.control_after,
        );
        if let Some(pg0) = self.pg0 {
            text.push_str(&format!("; {}", pg0.describe("PG0")));
        }
        if let Some(pg) = self.pg {
            text.push_str(&format!("; {}", pg.describe("its power gate")));
        }
        text.push_str(&format!("; {}", self.requesters.describe()));
        text
    }
}

/// What disabling the DC states found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DcStateObservation {
    pub(crate) before: u32,
    pub(crate) after: u32,
    /// Whether the field was already clear.
    pub(crate) already_disabled: bool,
    /// How many writes it took.  Zero when the field was already clear.
    pub(crate) writes: u32,
}

impl DcStateObservation {
    pub(crate) fn describe(&self) -> String {
        format!(
            "DC states: {:#010x} -> {:#010x} (field {:#x} {}{})",
            self.before,
            self.after,
            self.after & DC_STATE_MASK,
            if self.already_disabled {
                "was already disabled"
            } else {
                "disabled"
            },
            match self.writes {
                0 => String::new(),
                1 => String::from(", one write"),
                n => format!(", {n} writes"),
            },
        )
    }
}

/// One DBUF slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DbufSliceState {
    pub(crate) register: Register,
    pub(crate) was_on: bool,
    pub(crate) requested: bool,
    pub(crate) on: bool,
    pub(crate) readback: u32,
}

/// The display buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DbufState {
    pub(crate) slices: Vec<DbufSliceState>,
}

impl DbufState {
    pub(crate) fn enabled(&self) -> usize {
        self.slices.iter().filter(|slice| slice.on).count()
    }

    pub(crate) fn describe(&self) -> String {
        let mut text = format!(
            "DBUF: {} of {} slices up (",
            self.enabled(),
            self.slices.len()
        );
        let parts: Vec<String> = self
            .slices
            .iter()
            .map(|slice| {
                format!(
                    "{:#07x} {}",
                    slice.register.offset(),
                    match (slice.was_on, slice.on) {
                        (true, _) => "already on",
                        (false, true) => "came up",
                        (false, false) => "DID NOT COME UP",
                    }
                )
            })
            .collect();
        text.push_str(&parts.join(", "));
        text.push(')');
        if self.enabled() < self.slices.len() {
            text.push_str(
                "; which slices a SKU populates is a documented gap (reference section 4.7), so \
                 this is reported rather than treated as a fault as long as one slice is up",
            );
        }
        text
    }
}

/// What the platform workarounds did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WorkaroundState {
    /// `GEN8_CHICKEN_DCPR_1` after `Wa_16013190616`, and whether its
    /// `DISABLE_FLR_SRC` bit is set.
    pub(crate) chicken_dcpr_1: u32,
    /// `GEN11_CHICKEN_DCPR_2` before and after `Wa_14011508470`.
    pub(crate) chicken_dcpr_2_before: u32,
    pub(crate) chicken_dcpr_2_after: u32,
    /// `XELPD_DISPLAY_ERR_FATAL_MASK`, read and deliberately not written.
    pub(crate) display_err_fatal_mask: u32,
}

impl WorkaroundState {
    pub(crate) fn describe(&self) -> String {
        format!(
            "workarounds: Wa_16013190616 DISABLE_FLR_SRC set (GEN8_CHICKEN_DCPR_1 {:#010x}); \
             Wa_14011508470 GEN11_CHICKEN_DCPR_2 {:#010x} -> {:#010x}; \
             XELPD_DISPLAY_ERR_FATAL_MASK reads {:#010x} and was deliberately left alone so fatal \
             display errors stay visible (i915 masks them with Wa_14011503030)",
            self.chicken_dcpr_1,
            self.chicken_dcpr_2_before,
            self.chicken_dcpr_2_after,
            self.display_err_fatal_mask,
        )
    }
}

/// Everything [`bring_up`] observed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PowerState {
    pub(crate) fuses: FuseState,
    pub(crate) dc_state: DcStateObservation,
    pub(crate) phys: Vec<PhyState>,
    /// Each PHY's `COMP_INIT` after `PW_1` came up, which is the read that says
    /// whether the PHY step took: §11 phase 1.2 says a `COMP_INIT` that does
    /// not stick means the PHY is not powered.
    pub(crate) phy_comp_init_after_pw1: Vec<(&'static str, bool)>,
    pub(crate) pw1: WellObservation,
    pub(crate) cdclk: clk::CdclkState,
    pub(crate) raw_clock: clk::RawClockState,
    pub(crate) dbuf: DbufState,
    pub(crate) workarounds: WorkaroundState,
}

impl PowerState {
    /// Whether every PHY came up, which is the last thing this phase can check
    /// without a pipe.
    pub(crate) fn phys_initialised(&self) -> bool {
        self.phy_comp_init_after_pw1
            .iter()
            .all(|(_, initialised)| *initialised)
    }

    /// The bring-up log, one line per fact, as data.
    ///
    /// Built as a string rather than logged as it goes for the same reason the
    /// probe's report is: the sequence cannot be run on the target yet, so the
    /// text has to be something a host test can assert on.  [`Self::log`] puts
    /// the same text into the kernel log.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        let mut line = |text: String| {
            out.push_str("intel-gpu: power: ");
            out.push_str(&text);
            out.push('\n');
        };

        // Phase 0.3 first, because on a fused-off machine nothing downstream
        // fails with a diagnostic worth reading.
        line(format!(
            "fuses: SKL_DFSM {:#010x}, SFUSE_STRAP {:#010x}, SKL_DSSM {:#010x}, SKL_FUSE_STATUS \
             {:#010x}",
            self.fuses.dfsm, self.fuses.sfuse_strap, self.fuses.dssm, self.fuses.fuse_status,
        ));
        line(format!(
            "fuses: pipes present {} (A {} B {} C {} D {}); {}",
            describe_pipe_mask(self.fuses.pipe_mask()),
            u8::from(self.fuses.pipe_mask() & 1 != 0),
            u8::from(self.fuses.pipe_mask() & 2 != 0),
            u8::from(self.fuses.pipe_mask() & 4 != 0),
            u8::from(self.fuses.pipe_mask() & 8 != 0),
            self.fuses
                .absent_engines()
                .iter()
                .map(|(name, absent)| format!(
                    "{name} {}",
                    if *absent { "absent" } else { "present" }
                ))
                .collect::<Vec<_>>()
                .join(", "),
        ));
        line(format!(
            "fuses: SFUSE_STRAP fuse-lock {}, CRT {}, DDI-detect bits {:#06b} (a hint only: on \
             Gen12 port presence comes from the VBT, reference section 3.5)",
            u8::from(self.fuses.sfuse_strap & SFUSE_STRAP_FUSE_LOCK != 0),
            u8::from(self.fuses.sfuse_strap & SFUSE_STRAP_CRT_DISABLED != 0),
            self.fuses.sfuse_strap & 0xF,
        ));
        line(self.dc_state.describe());
        for phy in &self.phys {
            line(phy.describe());
        }
        line(self.pw1.describe());
        for (port, initialised) in &self.phy_comp_init_after_pw1 {
            line(format!(
                "combo PHY {port}: COMP_INIT after PW_1 is {}{}",
                u8::from(*initialised),
                if *initialised {
                    ""
                } else {
                    " -- section 11 phase 1.2: a PHY whose COMP_INIT does not stick is not \
                     powered, which after PW_1 is a real fault rather than an ordering artefact"
                },
            ));
        }
        line(self.cdclk.describe());
        line(self.raw_clock.describe());
        line(self.dbuf.describe());
        line(self.workarounds.describe());
        out
    }

    /// Emit the bring-up log, one line at a time.
    pub(crate) fn log(&self) {
        for text in self.render().lines() {
            axlog::info!("{text}");
        }
    }
}

/// The four pipes as a name, for a log line.
fn describe_pipe_mask(mask: u8) -> String {
    if mask == 0 {
        return String::from("none");
    }
    let names: Vec<&str> = ["A", "B", "C", "D"]
        .iter()
        .enumerate()
        .filter(|(bit, _)| mask & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect();
    names.join("+")
}

/// What can go wrong bringing the display's power up.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PowerError {
    Unreadable {
        register: &'static str,
    },
    WriteRefused {
        register: &'static str,
    },
    /// `SKL_DFSM` masks off every pipe, so there is nothing to bring up.
    DisplayFusedOff {
        dfsm: u32,
    },
    /// The SKU's strap declares the display fused off.  Section 3.6's pointer
    /// to the OpRegion's `PCON_HEADLESS_SKU` is the other half of this check;
    /// this driver does not read the OpRegion, so the strap is all it has.
    DisplayDisabledStrap {
        sfuse_strap: u32,
    },
    /// `DC_STATE_EN` would not take the disable.
    DcStateNotDisabled {
        wrote: u32,
        readback: u32,
        writes: u32,
    },
    Phy(phy::PhyError),
    /// `PW_1`'s precondition: `PG0` never reported its fuse as distributed.
    Pg0NeverDistributed {
        fuse_status: u32,
    },
    /// The well's `STATE` bit never set.  §11 phase 1.3 lists what this means.
    WellStateNeverSet {
        well: &'static str,
        index: u32,
        control: u32,
        requesters: Requesters,
        /// Whether the request bit this call added has been withdrawn.  It is
        /// false when the bit was already set before the call, because then it
        /// is not this call's to withdraw.
        rolled_back: bool,
    },
    /// A phase after `PW_1` came up failed.
    ///
    /// The well request this call added is withdrawn before returning, so a
    /// failed bring-up leaves the device as it was found rather than
    /// half-powered -- which matters because the next attempt, and any
    /// diagnosis, has to start from a state someone can describe.
    AfterPowerUp {
        step: &'static str,
        cause: String,
        unwound: bool,
    },
    Clock(clk::ClockError),
    /// No DBUF slice came up at all.
    DbufNeverPowered {
        readback: [u32; 4],
    },
    /// A write did not read back as written.  The bits in `wrote` are the ones
    /// the sequence set; `read` is what the device kept.
    ReadbackMismatch {
        register: &'static str,
        wrote: u32,
        read: u32,
    },
}

impl From<phy::PhyError> for PowerError {
    fn from(error: phy::PhyError) -> Self {
        Self::Phy(error)
    }
}

impl From<clk::ClockError> for PowerError {
    fn from(error: clk::ClockError) -> Self {
        Self::Clock(error)
    }
}

impl PowerError {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Unreadable { register } => {
                format!("{register} could not be read: it is outside the mapped register window")
            }
            Self::WriteRefused { register } => format!(
                "{register} refused the write: either it is not declared writable or it is \
                 outside the mapped register window"
            ),
            Self::DisplayFusedOff { dfsm } => format!(
                "SKL_DFSM reads {dfsm:#010x}, which fuses off every pipe: the display engine has \
                 nothing to drive and no power well will help.  Reference section 3.4"
            ),
            Self::DisplayDisabledStrap { sfuse_strap } => format!(
                "SFUSE_STRAP reads {sfuse_strap:#010x} with bit 7 set, so this SKU declares the \
                 display disabled.  Reference section 3.5; section 3.6 notes the OpRegion's \
                 PCON_HEADLESS_SKU bit as the other way a machine says this"
            ),
            Self::DcStateNotDisabled {
                wrote,
                readback,
                writes,
            } => format!(
                "DC_STATE_EN did not take {wrote:#010x} after {writes} writes; it reads \
                 {readback:#010x}.  Reference section 11 phase 1.1: with DC states enabled the \
                 hardware may power-gate what the driver is programming, intermittently"
            ),
            Self::Phy(error) => error.describe(),
            Self::Pg0NeverDistributed { fuse_status } => format!(
                "PG0's fuse distribution bit never set within {WELL_FUSE_TIMEOUT_US} us: \
                 SKL_FUSE_STATUS reads {fuse_status:#010x}, so bit 27 is clear.  PG0 is the root \
                 of the XE_LPD well tree (reference section 4.1), so nothing below it is powered \
                 and the PW_1 request would be dropped"
            ),
            Self::WellStateNeverSet {
                well,
                index,
                control,
                requesters,
                rolled_back,
            } => format!(
                "power well {well} never reported its state bit: {control:#010x} after requesting \
                 bit {request:#x} (well index {index}, state bit {state:#x}).  Reference section \
                 11 phase 1.3 lists the causes in order of likelihood: the well index is wrong; \
                 the fuse bit is wrong; or another requester is holding the well with a different \
                 bit pattern -- {}.  The BIOS, KVMR and debug request registers are in the log \
                 for exactly that comparison.  The request bit this call added was {}",
                requesters.describe(),
                if *rolled_back {
                    "withdrawn, so the well is left as it was found"
                } else {
                    "already set before this call, so it was left alone"
                },
                request = well_request(*index),
                state = well_state(*index),
            ),
            Self::AfterPowerUp {
                step,
                cause,
                unwound,
            } => format!(
                "{step} failed after PW_1 came up: {cause}.  {}",
                if *unwound {
                    "The well request this call added was withdrawn, so the display is left as it \
                     was found rather than half-powered"
                } else {
                    "The well request was already set before this call, so it was left alone; the \
                     display is powered but not brought up"
                },
            ),
            Self::Clock(error) => error.describe(),
            Self::DbufNeverPowered { readback } => format!(
                "no DBUF slice came up: the four slice registers read {readback:#010x?} after \
                 their request bits were set.  Reference section 11 phase 1.5: the failure this \
                 causes is not a failure to start but every frame underrunning \
                 (PIPE_FIFO_UNDERRUN_STATUS), which is far harder to attribute later"
            ),
            Self::ReadbackMismatch {
                register,
                wrote,
                read,
            } => format!("{register} did not keep the bits {wrote:#010x}: it reads {read:#010x}"),
        }
    }
}

/// Read a register the sequence cannot do without.
fn read(regs: &impl Registers, register: Register) -> Result<u32, PowerError> {
    regs.read(register).ok_or(PowerError::Unreadable {
        register: register.name(),
    })
}

/// Write a register, refusing to continue if the write did not happen.
fn write(regs: &impl Registers, register: Register, value: u32) -> Result<(), PowerError> {
    if regs.write(register, value) {
        Ok(())
    } else {
        Err(PowerError::WriteRefused {
            register: register.name(),
        })
    }
}

/// Read-modify-write, in the same `(clear, set)` convention the PHY module
/// documents: `(read & !clear) | set`.
fn rmw(regs: &impl Registers, register: Register, clear: u32, set: u32) -> Result<u32, PowerError> {
    let current = read(regs, register)?;
    write(regs, register, (current & !clear) | set)?;
    Ok(current)
}

/// Read the four fuse and strap registers §11 phase 0.3 asks for.
pub(crate) fn read_fuses(regs: &impl Registers) -> Result<FuseState, PowerError> {
    Ok(FuseState {
        dfsm: read(regs, regs::SKL_DFSM)?,
        sfuse_strap: read(regs, regs::SFUSE_STRAP)?,
        dssm: read(regs, regs::SKL_DSSM)?,
        fuse_status: read(regs, regs::SKL_FUSE_STATUS)?,
    })
}

/// Poll a power gate's distribution bit.
fn poll_fuse(regs: &impl Registers, pg: u8) -> Result<FusePoll, PowerError> {
    let mask = fuse_pg_dist_status(pg);
    let distributed = regs::poll(
        regs,
        regs::SKL_FUSE_STATUS,
        mask,
        mask,
        WELL_FUSE_TIMEOUT_US,
    )
    .ok_or(PowerError::Unreadable {
        register: regs::SKL_FUSE_STATUS.name(),
    })?;
    let readback = read(regs, regs::SKL_FUSE_STATUS)?;
    Ok(if distributed {
        FusePoll::Distributed { readback }
    } else {
        FusePoll::NotDistributed { pg, readback }
    })
}

/// Which requesters hold a well's request bit.
fn requesters(regs: &impl Registers, well: Well) -> Result<Requesters, PowerError> {
    let mask = well.request_mask();
    Ok(Requesters {
        bios: read(regs, REQUEST_REGISTERS[0])? & mask != 0,
        driver: read(regs, REQUEST_REGISTERS[1])? & mask != 0,
        kvmr: read(regs, REQUEST_REGISTERS[2])? & mask != 0,
        debug: read(regs, REQUEST_REGISTERS[3])? & mask != 0,
    })
}

/// Turn off the DC states.
///
/// §11 phase 1.1, and §4.10 for why it matters more than it looks: with DC
/// states disabled the display engine never hands power management to the DMC,
/// so a kernel with no DMC firmware is viable and the wells stay under the
/// driver's control.
///
/// The write is a read-modify-write of the field software owns, not a write of
/// zero: `DC_STATE_EN` also carries status and hardware-communication bits,
/// and §12.1 says software must not change bits 9, 8 and 4.  `[I915]`
/// `gen9_write_dc_state` retries up to 100 times, because the hardware is
/// documented to ignore a DC state change while it is restoring register state,
/// so a single write and a read-back would be a false failure waiting to
/// happen.
pub(crate) fn disable_dc_states(regs: &impl Registers) -> Result<DcStateObservation, PowerError> {
    let before = read(regs, regs::DC_STATE_EN)?;
    let already_disabled = before & DC_STATE_MASK == DC_STATE_DISABLE;
    let mut after = before;
    let mut writes = 0;
    let mut last_written = before;
    while after & DC_STATE_MASK != DC_STATE_DISABLE && writes < DC_STATE_ATTEMPTS {
        last_written = (after & !DC_STATE_MASK) | DC_STATE_DISABLE;
        write(regs, regs::DC_STATE_EN, last_written)?;
        writes += 1;
        after = read(regs, regs::DC_STATE_EN)?;
    }

    if after & DC_STATE_MASK != DC_STATE_DISABLE {
        return Err(PowerError::DcStateNotDisabled {
            wrote: last_written,
            readback: after,
            writes,
        });
    }
    Ok(DcStateObservation {
        before,
        after,
        already_disabled,
        writes,
    })
}

/// Enable one power well.
///
/// §4.4's handshake, with §4.5's ADL workaround in front of it:
///
/// ```text
/// if this well's power gate is PG1: GEN8_CHICKEN_DCPR_1 |= DISABLE_FLR_SRC
/// if this well's power gate is PG1: poll PG0's fuse distribution bit
/// HSW_PWR_WELL_CTL2 |= REQ(index)
/// poll HSW_PWR_WELL_CTL2.STATE(index)
/// poll this well's PG fuse distribution bit
/// ```
///
/// `Wa_16013190616` is `IS_ALDERLAKE_P` in i915, and ADL-N is a subplatform of
/// ADL-P, so it applies here.  `[I915]`
/// `display/intel_display_power_well.c:342-384`.
///
/// Only the `STATE` poll is fatal; see the module header for why the fuse polls
/// are recorded instead.
pub(crate) fn enable_well(
    regs: &impl Registers,
    well: Well,
) -> Result<WellObservation, PowerError> {
    // The workaround goes first, before the request write, which is the whole
    // point of it.  `PG0` is polled next, and only for a PG1 well, because
    // `[I915]` waits for PG0 only when the gate it is enabling is PG1.
    let mut pg0 = None;
    if well.pg == Some(SKL_PG1) {
        rmw(regs, regs::GEN8_CHICKEN_DCPR_1, 0, DISABLE_FLR_SRC)?;
        let polled = poll_fuse(regs, SKL_PG0)?;
        if let FusePoll::NotDistributed { readback, .. } = polled {
            return Err(PowerError::Pg0NeverDistributed {
                fuse_status: readback,
            });
        }
        pg0 = Some(polled);
    }

    let control_before = read(regs, well.register)?;
    let already_on = control_before & well.state_mask() != 0;
    // Whether *this call* is the one adding the request bit.  It is the
    // difference between a bit this call may withdraw and one that belongs to
    // whoever set it earlier.
    let we_request = control_before & well.request_mask() == 0;

    rmw(regs, well.register, 0, well.request_mask())?;

    // The write is posted.  The poll below is what establishes that the device
    // saw it, so no separate posting read is needed here; the state bit is the
    // acknowledgement.
    let state_set = regs::poll(
        regs,
        well.register,
        well.state_mask(),
        well.state_mask(),
        well.timeout_us,
    )
    .ok_or(PowerError::Unreadable {
        register: well.register.name(),
    })?;
    if !state_set {
        // The diagnostic describes the state at the moment of failure, so it is
        // read before the rollback: a report that said "nobody requested this
        // well" because the rollback had already run would be actively
        // misleading about the one thing section 11 phase 1.3 asks a reader to
        // compare.
        let control = read(regs, well.register)?;
        let requesters = requesters(regs, well)?;
        // Then withdraw the request this call added.  Leaving it set would make
        // the next attempt -- and anyone reading the register afterwards --
        // unable to tell whether the bit was there before, which is the
        // difference between a retry and a diagnosis.
        let rolled_back = we_request && rmw(regs, well.register, well.request_mask(), 0).is_ok();
        return Err(PowerError::WellStateNeverSet {
            well: well.name,
            index: well.index,
            control,
            requesters,
            rolled_back,
        });
    }

    // The well's own gate is polled after the state bit, for every well that
    // has fuses -- including PW_1, whose gate is PG1.  i915 polls *both* PG0
    // (before the request) and the well's own gate (after the state bit) for a
    // PG1 well, and only its own gate for the others.
    let pg = match well.pg {
        Some(pg) => Some(poll_fuse(regs, pg)?),
        None => None,
    };

    Ok(WellObservation {
        name: well.name,
        index: well.index,
        control_before,
        control_after: read(regs, well.register)?,
        already_on,
        pg0,
        pg,
        requesters: requesters(regs, well)?,
    })
}

/// Bring the display buffer's slices up.
///
/// §11 phase 1.5 and §4.7: read each slice's state, request only the ones that
/// are off, and poll `POWER_STATE`.  Reading first is the point: a firmware
/// that already powered the display buffer leaves these on, and a driver that
/// writes the request bit anyway cannot tell afterwards whether the slice came
/// up because of its request or was already there.
///
/// The poll is fatal only if no slice comes up at all.  Which slices a SKU
/// populates is `[GAP]` (§4.7, §13.1 item 3), and i915's own position is that
/// one slice is enough to start with; refusing to continue because the fourth
/// slice of a two-slice part did not answer would convert a documented unknown
/// into a boot failure.  Zero slices is different: the display would underrun
/// every frame.
pub(crate) fn enable_dbuf(regs: &impl Registers) -> Result<DbufState, PowerError> {
    let mut slices = Vec::with_capacity(DBUF_SLICES.len());
    for register in DBUF_SLICES {
        let before = read(regs, register)?;
        let was_on = before & DBUF_POWER_STATE != 0;
        let mut requested = false;
        if !was_on {
            rmw(regs, register, 0, DBUF_POWER_REQUEST)?;
            // A posting read before the poll: the request is posted, and a
            // state bit that is read before the request lands would be read as
            // clear for no reason.
            read(regs, register)?;
            let _ = regs::poll(
                regs,
                register,
                DBUF_POWER_STATE,
                DBUF_POWER_STATE,
                DBUF_STATE_TIMEOUT_US,
            );
            requested = true;
        }
        let readback = read(regs, register)?;
        slices.push(DbufSliceState {
            register,
            was_on,
            requested,
            on: readback & DBUF_POWER_STATE != 0,
            readback,
        });
    }

    let state = DbufState { slices };
    if state.enabled() == 0 {
        let mut readback = [0u32; 4];
        for (slot, slice) in readback.iter_mut().zip(&state.slices) {
            *slot = slice.readback;
        }
        return Err(PowerError::DbufNeverPowered { readback });
    }
    Ok(state)
}

/// Apply the platform workarounds §11 phase 1.6 asks for.
///
/// `Wa_14011508470` (`GEN11_CHICKEN_DCPR_2`) is applied *after* CDCLK and DBUF,
/// which is where `[I915]` applies it in `icl_display_core_init`; the ordering
/// is why this is the last step rather than a step next to the well enable.
///
/// `XELPD_DISPLAY_ERR_FATAL_MASK` is read and **not written**.  `[I915]` writes
/// all-ones there (`Wa_14011503030`), masking every fatal display error; §4.9
/// step 11 notes that a first bring-up wants the opposite, and §13.2 marks that
/// as `[INF]`.  An error nobody can see is an error nobody fixes, so this
/// driver takes the `[INF]` and leaves the mask alone — but it reads the
/// register, so the log says what state it was left in rather than implying the
/// question was never asked.
///
/// `GEN8_CHICKEN_DCPR_1` is read here only for the log: the write belongs
/// before the `PW_1` request, in [`enable_well`].
pub(crate) fn apply_workarounds(regs: &impl Registers) -> Result<WorkaroundState, PowerError> {
    let chicken_dcpr_1 = read(regs, regs::GEN8_CHICKEN_DCPR_1)?;
    let chicken_dcpr_2_before = read(regs, regs::GEN11_CHICKEN_DCPR_2)?;
    rmw(regs, regs::GEN11_CHICKEN_DCPR_2, 0, WA_14011508470_BITS)?;
    let chicken_dcpr_2_after = read(regs, regs::GEN11_CHICKEN_DCPR_2)?;
    if chicken_dcpr_2_after & WA_14011508470_BITS != WA_14011508470_BITS {
        return Err(PowerError::ReadbackMismatch {
            register: regs::GEN11_CHICKEN_DCPR_2.name(),
            wrote: WA_14011508470_BITS,
            read: chicken_dcpr_2_after,
        });
    }
    let display_err_fatal_mask = read(regs, regs::XELPD_DISPLAY_ERR_FATAL_MASK)?;
    Ok(WorkaroundState {
        chicken_dcpr_1,
        chicken_dcpr_2_before,
        chicken_dcpr_2_after,
        display_err_fatal_mask,
    })
}

/// Re-read each combo PHY's `COMP_INIT`.
///
/// §11 phase 1.2's check, at the point where its answer means something: before
/// `PW_1` a `COMP_INIT` that did not stick is expected, and after it the same
/// reading means the PHY is not powered.
fn recheck_phys(regs: &impl Registers) -> Result<Vec<(&'static str, bool)>, PowerError> {
    let mut out = Vec::with_capacity(regs::COMBO_PHYS.len());
    for phy in regs::COMBO_PHYS {
        let value = read(regs, phy.comp_dw0)?;
        out.push((phy.port, value & phy::COMP_INIT != 0));
    }
    Ok(out)
}

/// Withdraw the `PW_1` request this call added, and describe why.
///
/// This is the rollback: a bring-up that fails after the well came up must not
/// leave the display powered but unprogrammed, because the next attempt and any
/// diagnosis have to start from a state someone can describe.  The request is
/// only withdrawn when *this call* added it -- `we_requested` is false when the
/// bit was already set, and then it belongs to whoever set it.
///
/// Nothing else is unwound.  The DBUF slice requests are deliberately left:
/// with `PW_1` down their state reads zero, `enable_dbuf` reads the state before
/// it requests anything, and so a retry re-requests exactly what it needs.  A
/// half-programmed PHY is likewise left alone, for the same reason and because
/// clearing `COMP_INIT` on a PHY whose reference values are half-written would
/// make a retry's verification pass fail for a reason this driver created.
fn unwind(
    regs: &impl Registers,
    we_requested: bool,
    step: &'static str,
    cause: PowerError,
) -> PowerError {
    let unwound = we_requested && rmw(regs, PW_1.register, PW_1.request_mask(), 0).is_ok();
    PowerError::AfterPowerUp {
        step,
        cause: cause.describe(),
        unwound,
    }
}

/// Bring the display's power, clocks and PHYs up, in order.
///
/// This is the workstream's single entry point.  It is the whole of reference
/// §11 phases 0.3 and 1; everything after it — a pipe, a plane, a DDI, a PLL —
/// is a different slice and is not programmed here.
///
/// The caller is responsible for having identified the device first: this
/// function takes a mapped register window and assumes the device table has
/// already said the part is one this kernel has a model for.  That is the same
/// division [`super::probe`] makes, and it keeps the "may I interpret this
/// register at all" question in one place.
///
/// Every step is logged with its readback, and the log is returned as data as
/// well as emitted, because none of this can be run on the target yet and a
/// host test has to be able to read what it would have said.
pub(crate) fn bring_up(regs: &impl Registers) -> Result<PowerState, PowerError> {
    // Phase 0.3: the "am I allowed to do this" reads, before anything is
    // programmed.  On a machine whose display is fused off, every later step
    // fails with no diagnostic at all, so these two refusals exist to be the
    // diagnostic.
    let fuses = read_fuses(regs)?;
    if fuses.pipe_mask() == 0 {
        return Err(PowerError::DisplayFusedOff { dfsm: fuses.dfsm });
    }
    if fuses.display_disabled() {
        return Err(PowerError::DisplayDisabledStrap {
            sfuse_strap: fuses.sfuse_strap,
        });
    }

    // Phase 1.1.
    let dc_state = disable_dc_states(regs)?;

    // Phase 1.2: PHY A first, for every combo PHY present.  This runs before
    // PW_1, which is the order `icl_display_core_init` uses.
    let phys = phy::init_all(regs)?;

    // Phase 1.3.
    let pw1 = enable_well(regs, PW_1)?;
    // Every step below runs with the well up, so every failure below has to
    // put the well back.  `we_requested` is what makes that safe: a request bit
    // that was already set is not this call's to withdraw.
    let we_requested = pw1.control_before & well_request(PW_1.index) == 0;

    // The re-read §11 phase 1.2 asks for: COMP_INIT is written before PW_1
    // exists, so its not sticking there means nothing; after PW_1 it means the
    // PHY is not powered.
    let phy_comp_init_after_pw1 =
        recheck_phys(regs).map_err(|error| unwind(regs, we_requested, "the PHY re-read", error))?;

    // Phases 1.4 and 1.4b.  The raw clock is a clock and belongs with CDCLK;
    // it must also be right before any south display function is enabled, which
    // is not this phase, so doing it here satisfies that with margin.
    let cdclk = clk::bring_up(regs).map_err(|error| {
        unwind(
            regs,
            we_requested,
            "the CDCLK step",
            PowerError::Clock(error),
        )
    })?;
    let raw_clock = clk::bring_up_raw_clock(regs, fuses.sfuse_strap).map_err(|error| {
        unwind(
            regs,
            we_requested,
            "the raw clock step",
            PowerError::Clock(error),
        )
    })?;

    // Phase 1.5.
    let dbuf =
        enable_dbuf(regs).map_err(|error| unwind(regs, we_requested, "the DBUF step", error))?;

    // Phase 1.6.
    let workarounds = apply_workarounds(regs)
        .map_err(|error| unwind(regs, we_requested, "the workaround step", error))?;

    Ok(PowerState {
        fuses,
        dc_state,
        phys,
        phy_comp_init_after_pw1,
        pw1,
        cdclk,
        raw_clock,
        dbuf,
        workarounds,
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::drm::intel::regs::mock::MockRegisters;

    /// A machine shaped like the target: four pipes, a 24 MHz reference, a
    /// working firmware CDCLK, and a 24 MHz raw clock strap.
    fn powered_machine() -> MockRegisters {
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DFSM, 0);
        regs.set(regs::SKL_DSSM, 0); // 24 MHz
        regs.set(regs::SFUSE_STRAP, 1 << 8); // the 24 MHz raw clock strap
        regs.set(regs::SKL_FUSE_STATUS, u32::MAX); // every fuse distributed
        regs.set(regs::CDCLK_PLL_ENABLE, (1 << 31) | (1 << 30) | 22);
        regs.set(regs::CDCLK_CTL, (1 << 22) | (7 << 19) | 350);
        regs.set(regs::PCH_RAWCLK_FREQ, 24 << 16);
        // DC6 requested: a machine that needs the DC state turned off, which is
        // the case the step exists for.
        regs.set(regs::DC_STATE_EN, 0b10);
        for phy in regs::COMBO_PHYS {
            // 0.95 V dot-1, a row the reference table states.
            regs.set(phy.comp_dw3, (1 << 26) | (1 << 24));
            regs.set(phy.comp_dw0, 1);
        }
        // Every well's state bit follows its request bit, which is what a
        // working device does.
        for well in [PW_1, PW_A, AUX_A] {
            regs.derive(well.register, move |written| {
                let (request, state) = (well.request_mask(), well.state_mask());
                if written & request != 0 {
                    written | state
                } else {
                    written & !state
                }
            });
        }
        // So does every DBUF slice's.
        for slice in DBUF_SLICES {
            regs.derive(slice, |written| {
                if written & DBUF_POWER_REQUEST != 0 {
                    written | DBUF_POWER_STATE
                } else {
                    written & !DBUF_POWER_STATE
                }
            });
        }
        // And the CDCLK PLL locks when it is enabled.
        regs.derive(regs::CDCLK_PLL_ENABLE, |written| {
            if written & clk::PLL_ENABLE != 0 {
                written | clk::PLL_LOCK
            } else {
                written & !clk::PLL_LOCK
            }
        });
        regs
    }

    #[test]
    fn a_whole_bring_up_runs_in_the_documented_order() {
        let regs = powered_machine();
        let state = bring_up(&regs).unwrap();

        // Phase 0.3 read everything before anything was written.
        assert_eq!(state.fuses.pipe_mask(), 0b1111);
        assert!(
            state
                .fuses
                .absent_engines()
                .iter()
                .all(|(_, absent)| !absent)
        );
        assert!(!state.fuses.display_disabled());

        // Phase 1.1: DC states off, field cleared and everything else kept.
        assert!(!state.dc_state.already_disabled);
        assert_eq!(regs.read(regs::DC_STATE_EN).unwrap() & DC_STATE_MASK, 0);

        // Phase 1.2: both PHYs initialised, A first.
        assert_eq!(state.phys.len(), 2);
        assert!(state.phys.iter().all(|phy| phy.initialised()));
        assert!(state.phys[0].comp_source && !state.phys[1].comp_source);

        // Phase 1.3: PW_1 up, with both fuse polls recorded.
        assert_eq!(state.pw1.name, "PW_1");
        assert!(state.pw1.requesters.driver);
        assert!(state.pw1.pg0.unwrap().distributed());
        assert!(state.pw1.pg.unwrap().distributed());
        // The re-read after PW_1 succeeded, which is the point of doing it.
        assert!(state.phys_initialised());
        assert!(state.phy_comp_init_after_pw1.iter().all(|(_, up)| *up));

        // Phase 1.4: the firmware's CDCLK was kept, not reprogrammed.
        assert!(state.cdclk.kept_firmware);

        // Phase 1.5: every slice up.
        assert_eq!(state.dbuf.enabled(), 4);

        // Phase 1.6: the workaround bits are set and the error mask is not.
        assert_ne!(
            state.workarounds.chicken_dcpr_2_after & WA_14011508470_BITS,
            0
        );
        assert_eq!(
            state.workarounds.chicken_dcpr_2_after & WA_14011508470_BITS,
            WA_14011508470_BITS
        );
        assert!(
            !regs
                .writes()
                .iter()
                .any(|(name, _)| *name == "XELPD_DISPLAY_ERR_FATAL_MASK")
        );

        // The order, as the register writes actually happened.  The PHY step
        // must precede PW_1, PW_1 must precede the CDCLK work, and the DBUF
        // must come after CDCLK, which is the order `icl_display_core_init`
        // uses and the order the reference document gives.
        let writes: Vec<&str> = regs.writes().iter().map(|(name, _)| *name).collect();
        let position = |name: &str| {
            writes
                .iter()
                .position(|written| *written == name)
                .unwrap_or_else(|| panic!("{name} was never written: {writes:?}"))
        };
        assert!(
            position("PORT_COMP_DW0(A)") < position("PORT_COMP_DW0(B)"),
            "PHY A must be initialised before PHY B: {writes:?}"
        );
        assert!(
            position("PORT_CL_DW5(B)") < position("HSW_PWR_WELL_CTL2"),
            "the PHYs must be initialised before PW_1: {writes:?}"
        );
        assert!(
            position("HSW_PWR_WELL_CTL2") < position("DBUF_CTL_S0"),
            "PW_1 must be up before the DBUF: {writes:?}"
        );
        assert!(
            position("GEN11_CHICKEN_DCPR_2") > position("DBUF_CTL_S3"),
            "Wa_14011508470 comes after CDCLK and DBUF: {writes:?}"
        );
        assert!(
            position("GEN8_CHICKEN_DCPR_1") < position("HSW_PWR_WELL_CTL2"),
            "Wa_16013190616 comes before the PW_1 request: {writes:?}"
        );
    }

    #[test]
    fn the_log_says_what_happened_at_every_step() {
        let regs = powered_machine();
        let state = bring_up(&regs).unwrap();
        let text = state.render();
        for expected in [
            "fuses: SKL_DFSM",
            "pipes present A+B+C+D",
            "DC states:",
            "combo PHY A:",
            "combo PHY B:",
            "COMP_INIT after PW_1 is 1",
            "power well PW_1 (index 0, request 0x2, state 0x1)",
            "CDCLK: reference 24 MHz",
            "raw clock: SFUSE_STRAP[8]=1",
            "DBUF: 4 of 4 slices up",
            "workarounds: Wa_16013190616",
            "deliberately left alone",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        // Every line carries the prefix, so one search finds all of it.
        for line in text.lines() {
            assert!(line.starts_with("intel-gpu: power: "), "unlabelled: {line}");
        }
    }

    #[test]
    fn a_machine_whose_display_is_fused_off_is_refused_before_anything_is_written() {
        // The phase 0.3 decision.  Reference §3.4: if masking off the fused-off
        // pipes leaves an empty mask, the display is fused off entirely.
        let regs = powered_machine();
        regs.set(regs::SKL_DFSM, u32::MAX);
        regs.set(regs::SKL_FUSE_STATUS, u32::MAX);
        let error = bring_up(&regs).unwrap_err();
        assert!(matches!(error, PowerError::DisplayFusedOff { .. }));
        assert!(error.describe().contains("fuses off every pipe"));
        assert!(
            regs.writes().is_empty(),
            "nothing may be written to a fused-off display"
        );

        // One pipe missing is not a refusal, and the log names which.
        let regs = powered_machine();
        regs.set(regs::SKL_DFSM, SKL_DFSM_PIPE_B_DISABLE);
        let state = bring_up(&regs).unwrap();
        assert_eq!(state.fuses.pipe_mask(), 0b1101);
        assert!(state.render().contains("pipes present A+C+D"));

        // The headless strap is the other refusal.
        let regs = powered_machine();
        regs.set(regs::SFUSE_STRAP, SFUSE_STRAP_DISPLAY_DISABLED);
        let error = bring_up(&regs).unwrap_err();
        assert!(matches!(error, PowerError::DisplayDisabledStrap { .. }));
        assert!(regs.writes().is_empty());
    }

    #[test]
    fn dc_state_disable_keeps_the_bits_software_does_not_own() {
        // §12.1: bits 9, 8 and 4 of DC_STATE_EN are hardware-communication
        // only, and bit 29 is status.  A write of zero would clear them, which
        // is why this is a read-modify-write of the field.
        let regs = powered_machine();
        let hardware_bits = (1 << 9) | (1 << 8) | (1 << 4) | (1 << 29);
        regs.set(regs::DC_STATE_EN, hardware_bits | 0b10); // DC6 requested
        let observation = disable_dc_states(&regs).unwrap();
        assert!(!observation.already_disabled);
        let after = regs.read(regs::DC_STATE_EN).unwrap();
        assert_eq!(after & DC_STATE_MASK, 0, "the field is cleared");
        assert_eq!(
            after & hardware_bits,
            hardware_bits,
            "the other bits survive"
        );

        // A machine that already had DC disabled is not rewritten.
        let regs = powered_machine();
        regs.set(regs::DC_STATE_EN, 1 << 4);
        let observation = disable_dc_states(&regs).unwrap();
        assert!(observation.already_disabled);
        assert!(regs.writes().is_empty());
    }

    #[test]
    fn a_dc_state_that_will_not_take_the_write_is_an_error() {
        // i915 retries up to 100 times because the hardware ignores the write
        // while it restores state; when it never takes, that is a real failure
        // and the count is in the message.
        let regs = powered_machine();
        regs.set(regs::DC_STATE_EN, 1);
        regs.derive(regs::DC_STATE_EN, |_| 1); // the write never lands
        let error = disable_dc_states(&regs).unwrap_err();
        assert!(matches!(
            error,
            PowerError::DcStateNotDisabled { writes: 100, .. }
        ));
        let text = error.describe();
        assert!(text.contains("intermittently"), "{text}");
    }

    #[test]
    fn a_well_whose_state_never_sets_names_every_cause_it_can() {
        // §11 phase 1.3's documented failure, with the requester comparison the
        // reference tells a reader to make.
        let regs = powered_machine();
        // The mock's PNV request bit never produces a state bit for PW_1: set
        // the derive hook to drop it.
        regs.derive(regs::HSW_PWR_WELL_CTL2, |written| {
            written & !PW_1.state_mask()
        });
        // The BIOS request register is read, not written, so the mock has to
        // answer with the bit rather than remember a write.
        regs.on_read(regs::HSW_PWR_WELL_CTL1, |stored| {
            stored | PW_1.request_mask()
        });

        let error = bring_up(&regs).unwrap_err();
        match error {
            PowerError::WellStateNeverSet {
                well,
                index,
                requesters,
                ..
            } => {
                assert_eq!(well, "PW_1");
                assert_eq!(index, 0);
                assert!(requesters.bios, "the BIOS request must be reported");
                assert!(requesters.driver);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let text = error.describe();
        assert!(text.contains("well index 0"), "{text}");
        assert!(text.contains("requesting bit 0x2"), "{text}");
        assert!(text.contains("requesters: bios 1 driver 1"), "{text}");
    }

    #[test]
    fn a_pg0_that_is_not_distributed_stops_the_well_before_the_request() {
        // PG0 is the root of the tree, so a well requested under an
        // undistributed root would be dropped.  Reference §4.4 and §11 1.3.
        let regs = powered_machine();
        regs.set(regs::SKL_FUSE_STATUS, 0);
        let error = bring_up(&regs).unwrap_err();
        assert!(matches!(error, PowerError::Pg0NeverDistributed { .. }));
        assert!(error.describe().contains("bit 27"));
        // The request was never written, so the log cannot be misread as "the
        // well was asked for and did not come up".
        assert!(
            !regs
                .writes()
                .iter()
                .any(|(name, _)| *name == regs::HSW_PWR_WELL_CTL2.name())
        );

        // A well's own fuse not being distributed, by contrast, is recorded
        // rather than fatal -- the bit position for PG6..PG9 is an inference.
        let regs = powered_machine();
        regs.set(
            regs::SKL_FUSE_STATUS,
            fuse_pg_dist_status(SKL_PG0) | fuse_pg_dist_status(SKL_PG1),
        );
        let state = bring_up(&regs).unwrap();
        assert!(state.pw1.pg.unwrap().distributed());
    }

    #[test]
    fn the_dbuf_reads_before_it_requests_and_reports_a_partial_part() {
        // Reading first is what makes the log able to distinguish "this driver
        // brought it up" from "the firmware had it on".
        let regs = powered_machine();
        regs.set(regs::DBUF_CTL_S0, DBUF_POWER_STATE); // already on
        let state = enable_dbuf(&regs).unwrap();
        assert!(state.slices[0].was_on);
        assert!(
            !state.slices[0].requested,
            "an already-on slice is not requested"
        );
        assert!(state.slices[1..].iter().all(|slice| slice.requested));
        assert_eq!(state.enabled(), 4);

        // A slice that never comes up is reported but is not fatal while
        // another one is up: which slices a SKU has is a documented gap.
        let regs = powered_machine();
        regs.derive(regs::DBUF_CTL_S3, |written| written & !DBUF_POWER_STATE);
        let state = enable_dbuf(&regs).unwrap();
        assert_eq!(state.enabled(), 3);
        assert!(state.describe().contains("DID NOT COME UP"));
        assert!(state.describe().contains("documented gap"));

        // No slice at all is fatal: that display underruns every frame.
        let regs = powered_machine();
        for slice in DBUF_SLICES {
            regs.derive(slice, |written| written & !DBUF_POWER_STATE);
        }
        let error = enable_dbuf(&regs).unwrap_err();
        assert!(matches!(error, PowerError::DbufNeverPowered { .. }));
        assert!(error.describe().contains("underrunning"));
    }

    #[test]
    fn the_error_mask_is_read_and_left_alone() {
        let regs = powered_machine();
        regs.set(regs::XELPD_DISPLAY_ERR_FATAL_MASK, 0x0000_0001);
        let state = apply_workarounds(&regs).unwrap();
        assert_eq!(state.display_err_fatal_mask, 0x0000_0001);
        assert!(
            !regs
                .writes()
                .iter()
                .any(|(name, _)| *name == "XELPD_DISPLAY_ERR_FATAL_MASK")
        );
        assert!(state.describe().contains("left alone"));
        // The workaround bits are set, and the other bits are kept.
        assert_eq!(
            regs.read(regs::GEN11_CHICKEN_DCPR_2).unwrap() & WA_14011508470_BITS,
            WA_14011508470_BITS
        );
    }

    #[test]
    fn a_workaround_register_that_drops_the_bits_is_an_error() {
        let regs = powered_machine();
        regs.derive(regs::GEN11_CHICKEN_DCPR_2, |_| 0);
        let error = apply_workarounds(&regs).unwrap_err();
        assert!(
            matches!(error, PowerError::ReadbackMismatch { .. }),
            "{error:?}"
        );
        assert!(error.describe().contains("did not keep the bits"));
    }

    #[test]
    fn enabling_a_well_that_is_already_on_does_not_claim_credit() {
        let regs = powered_machine();
        regs.set(regs::HSW_PWR_WELL_CTL2, well_state(PW_1.index));
        let observation = enable_well(&regs, PW_1).unwrap();
        assert!(observation.already_on);
        assert!(observation.describe().contains("was already on"));
        assert!(observation.requesters.driver);
    }

    #[test]
    fn a_register_outside_the_window_is_an_error_rather_than_a_zero() {
        let regs = powered_machine();
        regs.hide(regs::SKL_DFSM);
        assert_eq!(
            bring_up(&regs).unwrap_err(),
            PowerError::Unreadable {
                register: "SKL_DFSM"
            }
        );

        let regs = powered_machine();
        regs.refuse(regs::DC_STATE_EN);
        let error = bring_up(&regs).unwrap_err();
        assert!(
            matches!(error, PowerError::WriteRefused { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_well_table_is_the_xe_lpd_one_and_not_tigers() {
        // §4.2.1 is the warning this test exists for: three Gen12 projects map
        // the same bits three different ways, and Tiger Lake's chain
        // (PG1 = index 0, PG2 = index 1, PG3 = 2, ...) is *not* ADL-N's tree.
        // On XE_LPD, index 2, 3 and 4 belong to no well at all.
        let indices: Vec<(u32, &str)> = [PW_1, PW_2, PW_A, PW_B, PW_C, PW_D]
            .iter()
            .map(|well| (well.index, well.name))
            .collect();
        assert_eq!(
            indices,
            vec![
                (0, "PW_1"),
                (1, "PW_2"),
                (5, "PW_A"),
                (6, "PW_B"),
                (7, "PW_C"),
                (8, "PW_D"),
            ]
        );
        // The request and state bits follow from the index, and the fuse bit
        // from the index plus one, which is `ICL_PW_CTL_IDX_TO_PG`.
        assert_eq!(PW_1.request_mask(), 0x2);
        assert_eq!(PW_1.state_mask(), 0x1);
        assert_eq!(PW_A.request_mask(), 0x800);
        assert_eq!(PW_A.state_mask(), 0x400);
        assert_eq!(PW_D.request_mask(), 0x2_0000);
        assert_eq!(PW_D.state_mask(), 0x1_0000);
        assert_eq!(fuse_pg_dist_status(SKL_PG0), 1 << 27);
        assert_eq!(fuse_pg_dist_status(SKL_PG1), 1 << 26);
        assert_eq!(
            fuse_pg_dist_status(6),
            1 << 21,
            "PW_A's fuse, per the inference"
        );
        assert_eq!(fuse_pg_dist_status(9), 1 << 18, "PW_D's fuse");
        // The DDI and AUX wells live in their own registers.
        assert_eq!(DDI_IO_A.register.name(), "ICL_PWR_WELL_CTL_DDI2");
        assert_eq!(AUX_A.register.name(), "ICL_PWR_WELL_CTL_AUX2");
        assert_eq!(DDI_IO_A.request_mask(), 0x2);
        assert_eq!(AUX_B.state_mask(), 0x4);
    }

    #[test]
    fn a_well_whose_state_never_sets_withdraws_the_request_it_added() {
        // Rollback, at the handshake: the request bit this call added must not
        // be left behind, or the next attempt cannot tell whether it was there
        // before.
        let regs = powered_machine();
        regs.derive(regs::HSW_PWR_WELL_CTL2, |written| {
            written & !PW_1.state_mask()
        });
        let error = enable_well(&regs, PW_1).unwrap_err();
        match error {
            PowerError::WellStateNeverSet { rolled_back, .. } => assert!(rolled_back),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            regs.read(regs::HSW_PWR_WELL_CTL2).unwrap() & PW_1.request_mask(),
            0,
            "the request bit must be gone"
        );
        assert!(
            error.describe().contains("withdrawn"),
            "{}",
            error.describe()
        );

        // A request bit that was already set is not this call's to withdraw.
        let regs = powered_machine();
        regs.set(regs::HSW_PWR_WELL_CTL2, PW_1.request_mask());
        regs.derive(regs::HSW_PWR_WELL_CTL2, |written| {
            written & !PW_1.state_mask()
        });
        let error = enable_well(&regs, PW_1).unwrap_err();
        match error {
            PowerError::WellStateNeverSet { rolled_back, .. } => assert!(!rolled_back),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            regs.read(regs::HSW_PWR_WELL_CTL2).unwrap() & PW_1.request_mask(),
            PW_1.request_mask(),
            "a bit this call did not set must survive"
        );
    }

    #[test]
    fn a_failure_after_the_well_came_up_withdraws_it() {
        // The whole-sequence rollback: a step after PW_1 that fails must leave
        // the display as it was found.  The failing step here is the DBUF, and
        // the well request must be gone afterwards.
        let regs = powered_machine();
        for slice in DBUF_SLICES {
            regs.derive(slice, |written| written & !DBUF_POWER_STATE);
        }
        let error = bring_up(&regs).unwrap_err();
        match &error {
            PowerError::AfterPowerUp { step, unwound, .. } => {
                assert_eq!(*step, "the DBUF step");
                assert!(unwound);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            regs.read(regs::HSW_PWR_WELL_CTL2).unwrap() & PW_1.request_mask(),
            0,
            "PW_1 must have been released"
        );
        let text = error.describe();
        assert!(text.contains("the DBUF step failed"), "{text}");
        assert!(text.contains("left as it was found"), "{text}");

        // A later failure, the CDCLK: same contract, and the original error's
        // own message is carried through rather than replaced.
        let regs = powered_machine();
        regs.set(regs::SKL_DSSM, 2 << 29); // 38.4 MHz
        regs.set(regs::CDCLK_PLL_ENABLE, 0); // nothing usable
        // A PLL that will not enable at all: the mock's earlier hook sets LOCK
        // when ENABLE is written, so this one has to take both away, or the
        // device would look like one that locks at a ratio it dropped.
        regs.derive(regs::CDCLK_PLL_ENABLE, |written| {
            written & !(clk::PLL_ENABLE | clk::PLL_LOCK)
        });
        let error = bring_up(&regs).unwrap_err();
        match &error {
            PowerError::AfterPowerUp { step, unwound, .. } => {
                assert_eq!(*step, "the CDCLK step");
                assert!(unwound);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        let text = error.describe();
        assert!(text.contains("never reported lock"), "{text}");
    }

    #[test]
    fn a_bring_up_that_finds_a_dead_cdclk_programs_one_and_says_so() {
        // The other half of the CDCLK path, through the whole sequence: a
        // machine whose firmware left no usable CDCLK gets the lowest value
        // the table allows for the reference the hardware reports.
        let regs = powered_machine();
        regs.set(regs::SKL_DSSM, 1 << 29); // 19.2 MHz, so a different column
        regs.set(regs::CDCLK_PLL_ENABLE, 0);
        regs.set(regs::CDCLK_CTL, 0);
        let state = bring_up(&regs).unwrap();
        assert!(!state.cdclk.kept_firmware);
        assert_eq!(state.cdclk.programmed.unwrap().entry.cdclk_khz, 172_800);
        let text = state.render();
        assert!(text.contains("172800 kHz"), "{text}");
        assert!(text.contains("19.2 MHz"), "{text}");
    }
}
