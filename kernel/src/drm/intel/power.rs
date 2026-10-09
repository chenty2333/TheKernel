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
//! The HSW well handshake is delegated to the source-shaped
//! `tk-intel-display::power_well` translation. Its `STATE` and fuse timeouts
//! are reported and continue, matching i915's warning behavior; transport
//! errors remain errors. The DBUF and later PHY observations still belong to
//! this boot-sequence adapter.
//!
//! * **Power-well state and fuse timeouts are recorded, not fatal.** `[I915]`
//!   `hsw_wait_for_power_well_enable` and `gen9_wait_for_power_well_fuses`
//!   warn and continue. The PG6..PG9 positions for the ADL-N wells remain
//!   explicitly marked as an inference in the map; a timeout is observable
//!   without substituting a new driver policy for i915's behavior.
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

use intel_display::{
    dmc::DmcPlatform,
    power_domains::{PowerDomainIo, PowerDomainState},
    power_map::{
        PowerDomain, PowerWellGroup, PowerWellInstance, WellControl, WellOps, power_wells,
    },
    skl_watermark_full::{self, DisplayCaps as WatermarkDisplayCaps, WmLatencyIo},
};

use super::{
    clk,
    combo_phy_full::{
        self, ComboPhyDisplay, ComboPhyInstance, ComboPhyPlatform, DiagnosticLevel, VbtPortPresence,
    },
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

/// One power well: its name, where its request bit lives, its index, and which
/// power gate's fuse bit announces it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Well {
    pub(crate) name: &'static str,
    /// The *driver's* request register.  There are four requesters and hardware
    /// OR-s them, so the driver uses `HSW_PWR_WELL_CTL2` and leaves the BIOS,
    /// KVMR and debug registers alone.  Reference §4.2.
    pub(crate) register: Register,
    pub(crate) request_registers: WellRequestRegisters,
    /// The well index within that register.
    pub(crate) index: u32,
    pub(crate) irq_pipe_mask: u8,
    /// The power gate whose fuse bit is polled after the state bit, or `None`
    /// for a well with no fuses.  `[I915]` computes it as
    /// `idx - ICL_PW_CTL_IDX_PW_1 + SKL_PG1`, i.e. `idx + 1` in this table.
    pub(crate) pg: Option<u8>,
    pub(crate) timeout_us: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WellRequestRegisters {
    pub(crate) bios: Register,
    pub(crate) kvmr: Option<Register>,
    pub(crate) debug: Register,
}

const HSW_REQUESTS: WellRequestRegisters = WellRequestRegisters {
    bios: regs::HSW_PWR_WELL_CTL1,
    kvmr: Some(regs::HSW_PWR_WELL_CTL3),
    debug: regs::HSW_PWR_WELL_CTL4,
};
const AUX_REQUESTS: WellRequestRegisters = WellRequestRegisters {
    bios: regs::ICL_PWR_WELL_CTL_AUX1,
    kvmr: None,
    debug: regs::ICL_PWR_WELL_CTL_AUX4,
};
const DDI_REQUESTS: WellRequestRegisters = WellRequestRegisters {
    bios: regs::ICL_PWR_WELL_CTL_DDI1,
    kvmr: None,
    debug: regs::ICL_PWR_WELL_CTL_DDI4,
};

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
    request_registers: HSW_REQUESTS,
    index: 0,
    irq_pipe_mask: 0,
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
    request_registers: HSW_REQUESTS,
    index: 1,
    irq_pipe_mask: 0,
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
    request_registers: HSW_REQUESTS,
    index: 5,
    irq_pipe_mask: 0,
    pg: Some(6),
    timeout_us: WELL_STATE_TIMEOUT_US,
};
/// `PW_B`, the well for pipe B.  See [`PW_A`].
pub(crate) const PW_B: Well = Well {
    name: "PW_B",
    register: regs::HSW_PWR_WELL_CTL2,
    request_registers: HSW_REQUESTS,
    index: 6,
    irq_pipe_mask: 0,
    pg: Some(7),
    timeout_us: WELL_STATE_TIMEOUT_US,
};
/// `PW_C`, the well for pipe C.  See [`PW_A`].
pub(crate) const PW_C: Well = Well {
    name: "PW_C",
    register: regs::HSW_PWR_WELL_CTL2,
    request_registers: HSW_REQUESTS,
    index: 7,
    irq_pipe_mask: 0,
    pg: Some(8),
    timeout_us: WELL_STATE_TIMEOUT_US,
};
/// `PW_D`, the well for pipe D.  See [`PW_A`].
pub(crate) const PW_D: Well = Well {
    name: "PW_D",
    register: regs::HSW_PWR_WELL_CTL2,
    request_registers: HSW_REQUESTS,
    index: 8,
    irq_pipe_mask: 0,
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
    request_registers: DDI_REQUESTS,
    index: 0,
    irq_pipe_mask: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// `DDI_IO_B`.  See [`DDI_IO_A`].
pub(crate) const DDI_IO_B: Well = Well {
    name: "DDI_IO_B",
    register: regs::ICL_PWR_WELL_CTL_DDI2,
    request_registers: DDI_REQUESTS,
    index: 1,
    irq_pipe_mask: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// TGL/ADL TC1 DDI IO well for the Type-C PHY lane-power reference.
pub(crate) const DDI_IO_TC1: Well = Well {
    name: "DDI_IO_TC1",
    register: regs::ICL_PWR_WELL_CTL_DDI2,
    request_registers: DDI_REQUESTS,
    index: 3,
    irq_pipe_mask: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// TGL/ADL TC2 DDI IO well for the Type-C PHY lane-power reference.
pub(crate) const DDI_IO_TC2: Well = Well {
    name: "DDI_IO_TC2",
    register: regs::ICL_PWR_WELL_CTL_DDI2,
    request_registers: DDI_REQUESTS,
    index: 4,
    irq_pipe_mask: 0,
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
    request_registers: AUX_REQUESTS,
    index: 0,
    irq_pipe_mask: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// `AUX_B`.  See [`AUX_A`].
pub(crate) const AUX_B: Well = Well {
    name: "AUX_B",
    register: regs::ICL_PWR_WELL_CTL_AUX2,
    request_registers: AUX_REQUESTS,
    index: 1,
    irq_pipe_mask: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// TGL/ADL Type-C AUX1 (the source `AUX_CH_USBC1` / channel D well).
pub(crate) const AUX_TC1: Well = Well {
    name: "AUX_TC1",
    register: regs::ICL_PWR_WELL_CTL_AUX2,
    request_registers: AUX_REQUESTS,
    index: 3,
    irq_pipe_mask: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

/// TGL/ADL Type-C AUX2 (the source `AUX_CH_USBC2` / channel E well).
pub(crate) const AUX_TC2: Well = Well {
    name: "AUX_TC2",
    register: regs::ICL_PWR_WELL_CTL_AUX2,
    request_registers: AUX_REQUESTS,
    index: 4,
    irq_pipe_mask: 0,
    pg: None,
    timeout_us: WELL_STATE_TIMEOUT_US,
};

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
    Distributed,
    /// It did not.  `pg` is the power gate, so a log line can name the bit that
    /// should have been at `1 << (27 - pg)`.
    NotDistributed { pg: u8 },
}

impl FusePoll {
    pub(crate) fn distributed(self) -> bool {
        matches!(self, Self::Distributed)
    }

    pub(crate) fn describe(self, what: &str) -> String {
        match self {
            Self::Distributed => format!("{what} distributed"),
            Self::NotDistributed { pg } => format!(
                "{what} NOT distributed within {WELL_FUSE_TIMEOUT_US} us: bit {} (1 << (27 - \
                 {pg})) is clear",
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
    /// Whether the state bit was set when the handshake ended.
    ///
    /// [`enable_well`] returns an observation only once the state bit has set,
    /// so its own answer is always `true`.  The field exists because a caller
    /// that treats a well which never came up as a finding rather than a
    /// failure -- reference §11 phase 2.1's caller does -- records it in the
    /// same shape as one that came up, so that a report has one kind of line
    /// for a well rather than two.
    pub(crate) state_set: bool,
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
            if !self.state_set {
                "NEVER CAME UP"
            } else if self.already_on {
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
    pub(crate) state_stuck: bool,
}

impl DcStateObservation {
    pub(crate) fn describe(&self) -> String {
        format!(
            "DC states: {:#010x} -> {:#010x} (field {:#x} {}{}, {})",
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
            if self.state_stuck {
                "still not matched after the i915 retry loop"
            } else {
                "readback matched"
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
    pub(crate) platform: DmcPlatform,
    pub(crate) fuses: FuseState,
    pub(crate) dc_state: DcStateObservation,
    pub(crate) allowed_dc_mask: u32,
    pub(crate) target_dc_state: u32,
    pub(crate) phys: Vec<PhyState>,
    /// Each PHY's `COMP_INIT` after `PW_1` came up, which is the read that says
    /// whether the PHY step took: §11 phase 1.2 says a `COMP_INIT` that does
    /// not stick means the PHY is not powered.
    pub(crate) phy_comp_init_after_pw1: Vec<(&'static str, bool)>,
    pub(crate) pw1: WellObservation,
    /// Refcounted pipe-A domain and its map-backed wells.
    pub(crate) power_domains: PowerDomainState,
    pub(crate) cdclk: clk::CdclkState,
    /// Successful opt-in modeset CDCLK transition, if one was needed after
    /// the initial firmware/bring-up observation.
    pub(crate) runtime_cdclk_transition: Option<clk::CdclkTransitionReport>,
    /// PCODE acknowledged PREPARE when initial CDCLK state required a change.
    pub(crate) pcode_cdclk_prepared: bool,
    /// Voltage level accepted by PCODE after a newly programmed CDCLK.
    pub(crate) pcode_voltage_level: Option<u8>,
    pub(crate) wm_latencies: [u32; skl_watermark_full::WM_LEVELS],
    pub(crate) wm_num_levels: usize,
    pub(crate) sagv_block_time_us: u32,
    pub(crate) raw_clock: clk::RawClockState,
    pub(crate) dbuf: DbufState,
    pub(crate) workarounds: WorkaroundState,
}

impl PowerState {
    fn power_map(&self) -> &'static [PowerWellGroup] {
        power_wells(self.platform)
    }

    pub(crate) fn watermark_config(&self) -> super::pipe::WatermarkConfig {
        super::pipe::WatermarkConfig {
            display_ver: 13,
            latencies: self.wm_latencies,
            num_levels: self.wm_num_levels,
            sagv_block_time_us: self.sagv_block_time_us,
        }
    }

    /// Acquire one source-mapped domain and its dependent power wells.
    pub(crate) fn get_domain<R: Registers>(
        &mut self,
        regs: &R,
        domain: PowerDomain,
    ) -> Result<(), PowerError> {
        let platform = self.platform;
        let map = self.power_map();
        self.power_domains
            .get(map, domain, &mut MappedPowerWellIo { regs, platform })
            .map_err(|error| PowerError::PowerDomain(format!("{error:?}")))
    }

    /// Release one source-mapped domain after its final client reference.
    pub(crate) fn put_domain<R: Registers>(
        &mut self,
        regs: &R,
        domain: PowerDomain,
    ) -> Result<(), PowerError> {
        let platform = self.platform;
        let map = self.power_map();
        self.power_domains
            .put(map, domain, &mut MappedPowerWellIo { regs, platform })
            .map_err(|error| PowerError::PowerDomain(format!("{error:?}")))
    }

    pub(crate) fn get_domain_if_enabled<R: Registers>(
        &mut self,
        regs: &R,
        domain: PowerDomain,
        runtime_active: bool,
    ) -> Result<bool, PowerError> {
        let platform = self.platform;
        let map = self.power_map();
        self.power_domains
            .get_if_enabled(
                map,
                domain,
                runtime_active,
                &mut MappedPowerWellIo { regs, platform },
            )
            .map_err(|error| PowerError::PowerDomain(format!("{error:?}")))
    }

    pub(crate) fn is_domain_enabled<R: Registers>(
        &self,
        regs: &R,
        domain: PowerDomain,
        runtime_active: bool,
    ) -> Result<bool, PowerError> {
        self.power_domains
            .is_enabled(
                self.power_map(),
                domain,
                runtime_active,
                &MappedPowerWellIo {
                    regs,
                    platform: self.platform,
                },
            )
            .map_err(|error| PowerError::PowerDomain(format!("{error:?}")))
    }

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
        line(format!(
            "DC policy: allowed mask {:#04x}, target state {:#04x}",
            self.allowed_dc_mask, self.target_dc_state
        ));
        line(format!(
            "power-domain PIPE_A refcount {}",
            self.power_domains.domain_use_count(PowerDomain::PipeA)
        ));
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
        if let Some(transition) = self.runtime_cdclk_transition {
            line(format!(
                "runtime CDCLK: {:?}, now {} kHz; PCODE PREPARE and voltage update succeeded \
                 (unlock timeout {}, lock timeout {}, crawl ACK timeout {})",
                transition.method,
                transition.after.cdclk_khz,
                transition.unlock_timed_out,
                transition.lock_timed_out,
                transition.crawl_ack_timed_out,
            ));
        }
        line(format!(
            "PCode CDCLK: prepare acknowledged {}, voltage level {}",
            u8::from(self.pcode_cdclk_prepared),
            self.pcode_voltage_level
                .map_or_else(|| String::from("unchanged"), |level| format!("{level}")),
        ));
        line(format!(
            "i915 WM latency levels ({}): {:?} us",
            self.wm_num_levels,
            &self.wm_latencies[..self.wm_num_levels.min(self.wm_latencies.len())],
        ));
        line(format!("SAGV block time: {} us", self.sagv_block_time_us));
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
    Phy(phy::PhyError),
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
    Pcode(String),
    PowerDomain(String),
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
            Self::Phy(error) => error.describe(),
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
            Self::Pcode(error) => format!("PCode CDCLK handshake failed: {error}"),
            Self::PowerDomain(error) => format!("i915 power-domain reference failed: {error}"),
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

/// Which requesters hold a well's request bit.
fn requesters(regs: &impl Registers, well: Well) -> Result<Requesters, PowerError> {
    let mask = well.request_mask();
    Ok(Requesters {
        bios: read(regs, well.request_registers.bios)? & mask != 0,
        driver: read(regs, well.register)? & mask != 0,
        kvmr: well
            .request_registers
            .kvmr
            .map(|register| read(regs, register))
            .transpose()?
            .is_some_and(|value| value & mask != 0),
        debug: read(regs, well.request_registers.debug)? & mask != 0,
    })
}

/// PCode-only part of translated i915 WM-latency initialization. ADL-N is
/// display-13, so the display-14+ MTL latency register path is not admitted.
struct PcodeWmLatency<'a, R, T> {
    regs: &'a R,
    timer: &'a T,
}

impl<R: Registers, T: super::gmbus::PollTimer> WmLatencyIo for PcodeWmLatency<'_, R, T> {
    fn read_mtl_latency_reg(&self, _index: usize) -> u32 {
        unreachable!("display-13 uses the source SKL PCode latency path")
    }

    fn read_skl_latency_pcode(&self, index: u32) -> Result<u32, i32> {
        super::pcode::read_wm_latency(self.regs, self.timer, index).map_err(|error| match error {
            super::pcode::PcodeError::MailboxStatus(status) => status,
            _ => -5, // EIO: mailbox register/backend unavailable.
        })
    }
}

/// Read the source watermark latency profile without running the mutating
/// display bring-up phases. The legacy TC fastboot route already owns a live
/// firmware scanout and cannot construct a `PowerState` by quiescing it first.
pub(crate) fn read_source_watermark_config<R: Registers, T: super::gmbus::PollTimer>(
    regs: &R,
    timer: &T,
) -> Result<super::pipe::WatermarkConfig, String> {
    let mut latencies = [0u32; skl_watermark_full::WM_LEVELS];
    let display = WatermarkDisplayCaps {
        display_ver: 13,
        display_ver_fixed: 13,
        alderlake_p: true,
        sagv: true,
        sagv_wm: true,
        has_hw_sagv_wm: true,
        ..WatermarkDisplayCaps::default()
    };
    let num_levels = skl_watermark_full::skl_setup_wm_latency(
        &PcodeWmLatency { regs, timer },
        &display,
        &mut latencies,
    )
    .map_err(|error| format!("PCode watermark-latency read returned errno {error}"))?;
    let sagv_block_time_us = match super::pcode::read_sagv_block_time_us(regs, timer) {
        Ok(value) if value <= u16::MAX as u32 => value,
        Ok(value) => {
            axlog::warn!("intel-gpu: PCode SAGV block time {value}us exceeds i915's 16-bit limit");
            0
        }
        Err(error) => {
            axlog::debug!("intel-gpu: could not read PCode SAGV block time: {error:?}");
            0
        }
    };
    Ok(super::pipe::WatermarkConfig {
        display_ver: 13,
        latencies,
        num_levels,
        sagv_block_time_us,
    })
}

struct HswPowerWellAdapter<'a, R: Registers> {
    regs: &'a R,
}

impl<R: Registers> HswPowerWellAdapter<'_, R> {
    fn register(offset: u32) -> Option<Register> {
        [
            regs::HSW_PWR_WELL_CTL1,
            regs::HSW_PWR_WELL_CTL2,
            regs::HSW_PWR_WELL_CTL3,
            regs::HSW_PWR_WELL_CTL4,
            regs::ICL_PWR_WELL_CTL_DDI2,
            regs::ICL_PWR_WELL_CTL_DDI1,
            regs::ICL_PWR_WELL_CTL_DDI4,
            regs::ICL_PWR_WELL_CTL_AUX2,
            regs::ICL_PWR_WELL_CTL_AUX1,
            regs::ICL_PWR_WELL_CTL_AUX4,
            regs::SKL_FUSE_STATUS,
            regs::GEN8_CHICKEN_DCPR_1,
            regs::DC_STATE_EN,
        ]
        .into_iter()
        .find(|register| register.offset() == offset)
    }
}

impl<R: Registers> intel_display::RegisterIo for HswPowerWellAdapter<'_, R> {
    fn read32(&self, offset: u32) -> Result<u32, intel_display::Error> {
        let register = Self::register(offset).ok_or(intel_display::Error::Unavailable(offset))?;
        self.regs
            .read(register)
            .ok_or(intel_display::Error::Unavailable(offset))
    }

    fn write32(&self, offset: u32, value: u32) -> Result<(), intel_display::Error> {
        let register = Self::register(offset).ok_or(intel_display::Error::Unavailable(offset))?;
        if self.regs.write(register, value) {
            Ok(())
        } else {
            Err(intel_display::Error::Refused)
        }
    }
}

impl<R: Registers> intel_display::power_well::HswPowerWellIo for HswPowerWellAdapter<'_, R> {
    fn wait_set(
        &self,
        register: u32,
        mask: u32,
        timeout_ms: u16,
    ) -> Result<bool, intel_display::Error> {
        let typed = Self::register(register).ok_or(intel_display::Error::Unavailable(register))?;
        super::regs::poll(self.regs, typed, mask, mask, u32::from(timeout_ms) * 1_000)
            .ok_or(intel_display::Error::Unavailable(register))
    }

    fn wait_clear(
        &self,
        register: u32,
        mask: u32,
        timeout_ms: u16,
    ) -> Result<bool, intel_display::Error> {
        let typed = Self::register(register).ok_or(intel_display::Error::Unavailable(register))?;
        super::regs::poll(self.regs, typed, mask, 0, u32::from(timeout_ms) * 1_000)
            .ok_or(intel_display::Error::Unavailable(register))
    }

    fn post_enable(&self, irq_pipe_mask: u8) -> Result<(), intel_display::Error> {
        if irq_pipe_mask == 0 || !super::irq::online() {
            Ok(())
        } else {
            // The source resets/initializes every pipe's DE IRQ block here.
            // This owner currently supports Pipe A only at install time; do
            // not silently skip a live IRQ transition.
            Err(intel_display::Error::Refused)
        }
    }

    fn pre_disable(&self, irq_pipe_mask: u8) -> Result<(), intel_display::Error> {
        if irq_pipe_mask == 0 || !super::irq::online() {
            Ok(())
        } else {
            // The i915 path masks/acks the pipe source and synchronizes the
            // parent IRQ before the well drops. Keep this fail-closed until
            // the per-well IRQ teardown path is translated.
            Err(intel_display::Error::Refused)
        }
    }
}

/// Turn off the DC states.
///
/// §11 phase 1.1, and §4.10 for why it matters more than it looks: with DC
/// states disabled the display engine never hands power management to the DMC,
/// so a kernel with no DMC firmware is viable and the wells stay under the
/// driver's control.
///
/// The translated `gen9_dc_mask` preserves status and hardware-communication
/// fields; `gen9_write_dc_state` performs i915's initial write and up to 100
/// rewrites. A persistent mismatch is recorded in `state_stuck` rather than
/// converted into a new fatal policy.
pub(crate) fn disable_dc_states<R: Registers>(regs: &R) -> Result<DcStateObservation, PowerError> {
    struct BootDcStateObserver;
    impl intel_display::dc_state::DcStateObserver for BootDcStateObserver {
        fn notify_psr_dc5_dc6(&mut self) {}
        fn update_dc6_allowed_count(&mut self, _: bool) {}
    }

    let adapter = HswPowerWellAdapter { regs };
    let mut tracked_dc_state = DC_STATE_DISABLE;
    let state = intel_display::dc_state::gen9_set_dc_state(
        &adapter,
        regs::DC_STATE_EN.offset(),
        13,
        false,
        DC_STATE_DISABLE,
        true,
        DC_STATE_DISABLE,
        &mut tracked_dc_state,
        &mut BootDcStateObserver,
    )
    .map_err(|error| match error {
        intel_display::Error::Unavailable(offset) => PowerError::Unreadable {
            register: HswPowerWellAdapter::<R>::register(offset)
                .map(Register::name)
                .unwrap_or(regs::DC_STATE_EN.name()),
        },
        _ => PowerError::WriteRefused {
            register: regs::DC_STATE_EN.name(),
        },
    })?;
    let mask = intel_display::dc_state::gen9_dc_mask(13, false);
    let before = state.before.ok_or(PowerError::Unreadable {
        register: regs::DC_STATE_EN.name(),
    })?;
    Ok(DcStateObservation {
        before,
        after: state.readback,
        already_disabled: before & mask == DC_STATE_DISABLE,
        writes: 1 + u32::from(state.rewrites),
        state_stuck: state.readback & mask != DC_STATE_DISABLE,
    })
}

/// Enable one HSW-style power well using i915's request/fuse sequence.
// upstream: intel_display_power_well.c hsw_power_well_enable()
pub(crate) fn enable_well<R: Registers>(
    regs: &R,
    well: Well,
    platform: DmcPlatform,
) -> Result<WellObservation, PowerError> {
    // This observation belongs to the surrounding transaction, not the
    // translated HSW helper: it records who owned the request before handoff.
    let control_before = read(regs, well.register)?;
    let already_on = control_before & well.state_mask() != 0;
    let adapter = HswPowerWellAdapter { regs };
    let spec = intel_display::power_well::HswWellSpec {
        name: well.name,
        registers: intel_display::power_well::HswWellRegisters {
            bios: well.request_registers.bios.offset(),
            driver: well.register.offset(),
            kvmr: well.request_registers.kvmr.map(Register::offset),
            debug: well.request_registers.debug.offset(),
            fuse_status: regs::SKL_FUSE_STATUS.offset(),
            gen8_chicken_dcpr1: regs::GEN8_CHICKEN_DCPR_1.offset(),
        },
        index: well.index as u8,
        pg: well.pg,
        timeout_ms: well.timeout_us.div_ceil(1_000) as u16,
        has_fuses: well.pg.is_some(),
        alderlake_pw1_wa: matches!(platform, DmcPlatform::AlderLakeP | DmcPlatform::AlderLakeN)
            && well.pg == Some(SKL_PG1),
        irq_pipe_mask: well.irq_pipe_mask,
    };
    let enable =
        intel_display::power_well::hsw_power_well_enable(&adapter, spec).map_err(|error| {
            match error {
                intel_display::Error::Unavailable(offset) => PowerError::Unreadable {
                    register: HswPowerWellAdapter::<R>::register(offset)
                        .map(Register::name)
                        .unwrap_or(well.register.name()),
                },
                _ => PowerError::WriteRefused {
                    register: well.register.name(),
                },
            }
        })?;
    let pg0 = enable.pg0_distributed.map(|distributed| {
        if distributed {
            FusePoll::Distributed
        } else {
            FusePoll::NotDistributed { pg: SKL_PG0 }
        }
    });
    let pg = enable.pg_distributed.map(|distributed| {
        if distributed {
            FusePoll::Distributed
        } else {
            FusePoll::NotDistributed {
                pg: well.pg.unwrap_or(SKL_PG0),
            }
        }
    });

    Ok(WellObservation {
        name: well.name,
        index: well.index,
        control_before,
        control_after: read(regs, well.register)?,
        already_on,
        state_set: enable.state_set,
        pg0,
        pg,
        requesters: requesters(regs, well)?,
    })
}

/// Drop one HSW-style request after its final mapped domain reference.
// upstream: intel_display_power_well.c hsw_power_well_disable()
pub(crate) fn disable_well<R: Registers>(
    regs: &R,
    well: Well,
    platform: DmcPlatform,
) -> Result<(), PowerError> {
    let adapter = HswPowerWellAdapter { regs };
    let spec = intel_display::power_well::HswWellSpec {
        name: well.name,
        registers: intel_display::power_well::HswWellRegisters {
            bios: well.request_registers.bios.offset(),
            driver: well.register.offset(),
            kvmr: well.request_registers.kvmr.map(Register::offset),
            debug: well.request_registers.debug.offset(),
            fuse_status: regs::SKL_FUSE_STATUS.offset(),
            gen8_chicken_dcpr1: regs::GEN8_CHICKEN_DCPR_1.offset(),
        },
        index: well.index as u8,
        pg: well.pg,
        timeout_ms: well.timeout_us.div_ceil(1_000) as u16,
        has_fuses: well.pg.is_some(),
        alderlake_pw1_wa: matches!(platform, DmcPlatform::AlderLakeP | DmcPlatform::AlderLakeN)
            && well.pg == Some(SKL_PG1),
        irq_pipe_mask: well.irq_pipe_mask,
    };
    intel_display::power_well::hsw_power_well_disable(&adapter, spec)
        .map(|_| ())
        .map_err(|error| match error {
            intel_display::Error::Unavailable(offset) => PowerError::Unreadable {
                register: HswPowerWellAdapter::<R>::register(offset)
                    .map(Register::name)
                    .unwrap_or(well.register.name()),
            },
            _ => PowerError::WriteRefused {
                register: well.register.name(),
            },
        })
}

fn mapped_hsw_well(instance: PowerWellInstance) -> Option<Well> {
    let mut well = match instance.control? {
        WellControl::IclPw1 => Some(PW_1),
        WellControl::IclPw2 => Some(PW_2),
        WellControl::IclDdiA => Some(DDI_IO_A),
        WellControl::IclDdiB => Some(DDI_IO_B),
        WellControl::TglDdiTc1 => Some(DDI_IO_TC1),
        WellControl::TglDdiTc2 => Some(DDI_IO_TC2),
        WellControl::IclAuxA => Some(AUX_A),
        WellControl::IclAuxB => Some(AUX_B),
        WellControl::TglAuxTc1 => Some(AUX_TC1),
        WellControl::TglAuxTc2 => Some(AUX_TC2),
        WellControl::XelpdPwA => Some(PW_A),
        WellControl::XelpdPwB => Some(PW_B),
        WellControl::XelpdPwC => Some(PW_C),
        WellControl::XelpdPwD => Some(PW_D),
        _ => None,
    }?;
    well.irq_pipe_mask = instance.irq_pipe_mask;
    Some(well)
}

/// `icl_tc_phy_aux_power_well_enable()` warns if the Type-C PHY uC health
/// bit does not become ready within one millisecond. The source warning does
/// not fail AUX power acquisition, so this bounded check is diagnostic only.
fn warn_if_tc_aux_uc_unhealthy<R: Registers>(regs: &R, port: intel_display::dkl_phy::TcPort) {
    let register = match intel_display::dkl_phy::DklRegister::new(port, 0x236c) {
        Ok(register) => register,
        Err(_) => {
            axlog::warn!("intel power: unsupported TC AUX DKL uC-health register");
            return;
        }
    };
    let io = super::tc_modeset::dkl_io(regs);
    let timer = super::gmbus::MonotonicTimer;
    let start = super::gmbus::PollTimer::now_micros(&timer);
    for _ in 0..1_000 {
        if intel_display::dkl_phy::intel_dkl_phy_read(&io, register)
            .is_ok_and(|value| value & (1 << 15) != 0)
        {
            return;
        }
        if super::gmbus::PollTimer::now_micros(&timer).saturating_sub(start) >= 1_000 {
            break;
        }
        super::gmbus::PollTimer::pause(&timer);
    }
    axlog::warn!("intel power: TC AUX DKL uC health did not set within 1 ms");
}

struct MappedPowerWellIo<'a, R> {
    regs: &'a R,
    platform: DmcPlatform,
}

impl<R: Registers> PowerDomainIo for MappedPowerWellIo<'_, R> {
    fn sync_well(
        &mut self,
        group: PowerWellGroup,
        instance: PowerWellInstance,
        _: u32,
    ) -> Result<(), intel_display::Error> {
        if instance.always_on || group.ops == WellOps::AlwaysOn || group.ops == WellOps::DcOff {
            return Ok(());
        }
        if !matches!(group.ops, WellOps::Hsw | WellOps::Ddi | WellOps::Aux) {
            return Err(intel_display::Error::Refused);
        }
        let well = mapped_hsw_well(instance).ok_or(intel_display::Error::Refused)?;
        let adapter = HswPowerWellAdapter { regs: self.regs };
        intel_display::power_well::hsw_power_well_sync_hw(
            &adapter,
            intel_display::power_well::HswWellRegisters {
                bios: well.request_registers.bios.offset(),
                driver: well.register.offset(),
                kvmr: well.request_registers.kvmr.map(Register::offset),
                debug: well.request_registers.debug.offset(),
                fuse_status: regs::SKL_FUSE_STATUS.offset(),
                gen8_chicken_dcpr1: regs::GEN8_CHICKEN_DCPR_1.offset(),
            },
            well.index as u8,
        )
        .map(|_| ())
    }

    fn enable_well(
        &mut self,
        group: PowerWellGroup,
        instance: PowerWellInstance,
    ) -> Result<(), intel_display::Error> {
        if instance.always_on || group.ops == WellOps::AlwaysOn {
            return Ok(());
        }
        if !matches!(group.ops, WellOps::Hsw | WellOps::Ddi | WellOps::Aux) {
            return Err(intel_display::Error::Refused);
        }
        // Linux `icl_tc_phy_aux_power_well_enable()` clears TBT_IO in the
        // source-selected AUX D/E control before requesting the Type-C AUX
        // well. The TBT group has a separate owner and is not admitted here.
        let aux_ctl = match instance.control {
            Some(WellControl::TglAuxTc1) => Some(regs::aux::DP_AUX_CH_CTL_D),
            Some(WellControl::TglAuxTc2) => Some(regs::aux::DP_AUX_CH_CTL_E),
            _ => None,
        };
        if let Some(control) = aux_ctl {
            let current = self
                .regs
                .read(control)
                .ok_or(intel_display::Error::Unavailable(control.offset()))?;
            if !self.regs.write(control, current & !(1 << 11)) {
                return Err(intel_display::Error::Unavailable(control.offset()));
            }
        }
        let mut well = mapped_hsw_well(instance).ok_or(intel_display::Error::Refused)?;
        if let Some(timeout_ms) = group.enable_timeout_ms {
            well.timeout_us = u32::from(timeout_ms).saturating_mul(1_000);
        }
        enable_well(self.regs, well, self.platform)
            .map_err(|_| intel_display::Error::Unavailable(well.register.offset()))?;
        match instance.control {
            Some(WellControl::TglAuxTc1) => {
                warn_if_tc_aux_uc_unhealthy(self.regs, intel_display::dkl_phy::TcPort::Tc1)
            }
            Some(WellControl::TglAuxTc2) => {
                warn_if_tc_aux_uc_unhealthy(self.regs, intel_display::dkl_phy::TcPort::Tc2)
            }
            _ => {}
        }
        Ok(())
    }

    fn disable_well(
        &mut self,
        group: PowerWellGroup,
        instance: PowerWellInstance,
    ) -> Result<(), intel_display::Error> {
        if instance.always_on || group.ops == WellOps::AlwaysOn {
            return Ok(());
        }
        if !matches!(group.ops, WellOps::Hsw | WellOps::Ddi | WellOps::Aux) {
            return Err(intel_display::Error::Refused);
        }
        let well = mapped_hsw_well(instance).ok_or(intel_display::Error::Refused)?;
        disable_well(self.regs, well, self.platform)
            .map_err(|_| intel_display::Error::Unavailable(well.register.offset()))
    }

    fn well_is_enabled(
        &self,
        group: PowerWellGroup,
        instance: PowerWellInstance,
    ) -> Result<bool, intel_display::Error> {
        if instance.always_on || group.ops == WellOps::AlwaysOn {
            return Ok(true);
        }
        let well = mapped_hsw_well(instance).ok_or(intel_display::Error::Refused)?;
        let adapter = HswPowerWellAdapter { regs: self.regs };
        intel_display::power_well::hsw_power_well_enabled(
            &adapter,
            intel_display::power_well::HswWellSpec {
                name: well.name,
                registers: intel_display::power_well::HswWellRegisters {
                    bios: well.request_registers.bios.offset(),
                    driver: well.register.offset(),
                    kvmr: well.request_registers.kvmr.map(Register::offset),
                    debug: well.request_registers.debug.offset(),
                    fuse_status: regs::SKL_FUSE_STATUS.offset(),
                    gen8_chicken_dcpr1: regs::GEN8_CHICKEN_DCPR_1.offset(),
                },
                index: well.index as u8,
                pg: well.pg,
                timeout_ms: well.timeout_us.div_ceil(1_000) as u16,
                has_fuses: well.pg.is_some(),
                alderlake_pw1_wa: matches!(
                    self.platform,
                    DmcPlatform::AlderLakeP | DmcPlatform::AlderLakeN
                ) && well.pg == Some(SKL_PG1),
                irq_pipe_mask: well.irq_pipe_mask,
            },
            false,
        )
    }
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
    bring_up_inner(regs, None)
}
/// Boot rollback reuses an already verified active PHY; it must not recalibrate
/// unused PHYs or modify hidden analog state that a register image cannot undo.
pub(crate) fn bring_up_preserving_phys(
    regs: &impl Registers,
    phys: Vec<phy::PhyState>,
) -> Result<PowerState, PowerError> {
    bring_up_inner(regs, Some(phys))
}
fn bring_up_inner(
    regs: &impl Registers,
    preserved: Option<Vec<phy::PhyState>>,
) -> Result<PowerState, PowerError> {
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
    let phys = match preserved {
        Some(phys) => phys,
        None => {
            // Source-order combo PHY initialization. The `phy::init_all`
            // verification pass below retains the existing boot report, while
            // the source-function translation owns the init/uninit decisions.
            let instances: Vec<_> = regs::COMBO_PHYS
                .iter()
                .copied()
                .map(|registers| ComboPhyInstance {
                    phy: registers.port.as_bytes().first().copied().unwrap_or(b'A') - b'A',
                    registers,
                })
                .collect();
            let display = ComboPhyDisplay {
                platform: ComboPhyPlatform {
                    display_version: 13,
                    ..ComboPhyPlatform::default()
                },
                vbt_ports: VbtPortPresence::default(),
                phys: &instances,
            };
            let mut diagnostics = Vec::new();
            combo_phy_full::intel_combo_phy_init(regs, &display, &mut diagnostics).map_err(
                |error| match error {
                    combo_phy_full::ComboPhyIoError::Read(register) => PowerError::Unreadable {
                        register: register.name(),
                    },
                    combo_phy_full::ComboPhyIoError::Write(register) => PowerError::WriteRefused {
                        register: register.name(),
                    },
                },
            )?;
            for diagnostic in diagnostics {
                match diagnostic.level {
                    DiagnosticLevel::Debug => {
                        axlog::debug!("intel-combo-phy: {}", diagnostic.message)
                    }
                    DiagnosticLevel::Warning | DiagnosticLevel::MissingCase => {
                        axlog::warn!("intel-combo-phy: {}", diagnostic.message)
                    }
                    DiagnosticLevel::Error => {
                        axlog::error!("intel-combo-phy: {}", diagnostic.message)
                    }
                }
            }
            phy::init_all(regs)?
        }
    };

    // Phase 1.3.
    let pw1 = enable_well(regs, PW_1, DmcPlatform::AlderLakeN)?;
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
    let pcode_timer = super::gmbus::MonotonicTimer;
    let clock_before = clk::observe(regs).map_err(|error| {
        unwind(
            regs,
            we_requested,
            "the CDCLK readout",
            PowerError::Clock(error),
        )
    })?;
    let pcode_cdclk_prepared = !clock_before.usable();
    if pcode_cdclk_prepared {
        super::pcode::prepare_cdclk_change(regs, &pcode_timer).map_err(|error| {
            unwind(
                regs,
                we_requested,
                "the PCode CDCLK prepare request",
                PowerError::Pcode(format!("{error:?}")),
            )
        })?;
    }
    let cdclk = clk::bring_up(regs).map_err(|error| {
        unwind(
            regs,
            we_requested,
            "the CDCLK step",
            PowerError::Clock(error),
        )
    })?;
    let pcode_voltage_level = if let Some(programmed) = cdclk.programmed {
        let level = clk::source_voltage_level(programmed.entry.cdclk_khz).ok_or_else(|| {
            unwind(
                regs,
                we_requested,
                "the translated CDCLK voltage-level policy",
                PowerError::Clock(clk::ClockError::UnsupportedVoltageLevel {
                    cdclk_khz: programmed.entry.cdclk_khz,
                }),
            )
        })?;
        super::pcode::commit_cdclk_voltage(regs, &pcode_timer, programmed.entry.cdclk_khz)
            .map_err(|error| {
                unwind(
                    regs,
                    we_requested,
                    "the PCode CDCLK voltage update",
                    PowerError::Pcode(format!("{error:?}")),
                )
            })?;
        Some(level)
    } else {
        None
    };
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

    // Source `skl_wm_init` obtains the display-12/13 latency table from PCode
    // before plane watermark computation. Preserve the source's level
    // adjustment and sanitization instead of using a made-up latency profile.
    let wm = read_source_watermark_config(regs, &pcode_timer).map_err(|error| {
        unwind(
            regs,
            we_requested,
            "the PCode watermark-latency read",
            PowerError::Pcode(error),
        )
    })?;
    let wm_latencies = wm.latencies;
    let wm_num_levels = wm.num_levels;
    let sagv_block_time_us = wm.sagv_block_time_us;

    // Phase 1.6.
    let workarounds = apply_workarounds(regs)
        .map_err(|error| unwind(regs, we_requested, "the workaround step", error))?;

    // The opt-in pipe-A modeset needs PW_A after the display core and clocks
    // are live. Keep the reference in the returned state so the map count
    // remains paired with the hardware request for the lifetime of scanout.
    let platform = DmcPlatform::AlderLakeN;
    let power_map = power_wells(platform);
    let mut power_domains = PowerDomainState::new(power_map);
    power_domains
        .sync_domain(
            power_map,
            PowerDomain::PipeA,
            &mut MappedPowerWellIo { regs, platform },
        )
        .map_err(|error| {
            unwind(
                regs,
                we_requested,
                "PIPE_A power-well BIOS handoff",
                PowerError::PowerDomain(format!("{error:?}")),
            )
        })?;
    power_domains
        .get(
            power_map,
            PowerDomain::PipeA,
            &mut MappedPowerWellIo { regs, platform },
        )
        .map_err(|error| {
            unwind(
                regs,
                we_requested,
                "PIPE_A power-domain request",
                PowerError::PowerDomain(format!("{error:?}")),
            )
        })?;
    let allowed_dc_mask = intel_display::dc_state::get_allowed_dc_mask(
        intel_display::dc_state::DcStateCaps {
            display_version: 13,
            has_display: true,
            dg2: false,
            dg1: false,
            geminilake: false,
            broxton: false,
            disable_power_well: intel_display::dc_state::sanitize_disable_power_well_option(-1),
        },
        -1,
    );
    let target_dc_state = intel_display::dc_state::sanitize_target_dc_state(
        intel_display::dc_state::DC_STATE_EN_UPTO_DC6,
        allowed_dc_mask,
    );

    Ok(PowerState {
        platform,
        fuses,
        dc_state,
        allowed_dc_mask,
        target_dc_state,
        phys,
        phy_comp_init_after_pw1,
        pw1,
        power_domains,
        cdclk,
        runtime_cdclk_transition: None,
        pcode_cdclk_prepared,
        pcode_voltage_level,
        wm_latencies,
        wm_num_levels,
        sagv_block_time_us,
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
        regs.set(regs::ICL_PWR_WELL_CTL_DDI2, 0);
        regs.set(regs::aux::DP_AUX_CH_CTL_D, 1 << 11);
        regs.set(regs::aux::DP_AUX_CH_CTL_E, 1 << 11);
        // DC6 requested: a machine that needs the DC state turned off, which is
        // the case the step exists for.
        regs.set(regs::DC_STATE_EN, 0b10);
        for phy in regs::COMBO_PHYS {
            // 0.95 V dot-1, a row the reference table states.
            regs.set(phy.comp_dw3, (1 << 26) | (1 << 24));
            regs.set(phy.comp_dw0, 1);
        }
        // Every well's state bit follows its request bit, which is what a
        // working device does.  Two of the three wells share a register --
        // `PW_1` and `PW_A` are both in `HSW_PWR_WELL_CTL2` -- and the mock
        // file holds one write hook per *register*, so a hook per well would
        // mean the second install replaces the first and the test then asserts
        // against a device that models one well instead of two.  Each register
        // gets one hook that answers for every well in it.
        regs.derive(regs::HSW_PWR_WELL_CTL2, |written| {
            let mut value = written;
            for well in [PW_1, PW_A] {
                let (request, state) = (well.request_mask(), well.state_mask());
                if written & request != 0 {
                    value |= state;
                } else {
                    value &= !state;
                }
            }
            value
        });
        regs.derive(regs::ICL_PWR_WELL_CTL_AUX2, |written| {
            let mut value = written;
            for well in [AUX_A, AUX_B, AUX_TC1, AUX_TC2] {
                let (request, state) = (well.request_mask(), well.state_mask());
                if written & request != 0 {
                    value |= state;
                } else {
                    value &= !state;
                }
            }
            value
        });
        regs.derive(regs::ICL_PWR_WELL_CTL_DDI2, |written| {
            let mut value = written;
            for well in [DDI_IO_A, DDI_IO_B, DDI_IO_TC1, DDI_IO_TC2] {
                let (request, state) = (well.request_mask(), well.state_mask());
                if written & request != 0 {
                    value |= state;
                } else {
                    value &= !state;
                }
            }
            value
        });
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
        assert_eq!(
            state.allowed_dc_mask,
            intel_display::dc_state::gen9_dc_mask(13, false)
        );
        assert_eq!(
            state.target_dc_state,
            intel_display::dc_state::DC_STATE_EN_UPTO_DC6
        );

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

        // i915 still writes the selected field when it already reads clear.
        let regs = powered_machine();
        regs.set(regs::DC_STATE_EN, 1 << 4);
        let observation = disable_dc_states(&regs).unwrap();
        assert!(observation.already_disabled);
        assert_eq!(observation.writes, 1);
        assert_eq!(regs.writes().len(), 1);
    }

    #[test]
    fn a_dc_state_that_ignores_the_write_records_i915_retry_exhaustion() {
        let regs = powered_machine();
        regs.set(regs::DC_STATE_EN, 1);
        regs.derive(regs::DC_STATE_EN, |_| 1); // the write never lands
        let state = disable_dc_states(&regs).unwrap();
        assert!(state.state_stuck);
        assert_eq!(state.writes, 101); // initial write + 100 rewrites
        assert_eq!(state.after, 1);
    }

    #[test]
    fn hsw_state_timeout_is_recorded_and_requester_owners_remain_visible() {
        let regs = powered_machine();
        // The mock request lands but the power well never acknowledges it.
        regs.derive(regs::HSW_PWR_WELL_CTL2, |written| {
            written & !PW_1.state_mask()
        });
        regs.on_read(regs::HSW_PWR_WELL_CTL1, |stored| {
            stored | PW_1.request_mask()
        });

        let observation = enable_well(&regs, PW_1, DmcPlatform::AlderLakeN).unwrap();
        assert!(!observation.state_set);
        assert!(
            observation.requesters.bios,
            "the BIOS request must be reported"
        );
        assert!(observation.requesters.driver);
        assert_eq!(
            observation.control_after & PW_1.request_mask(),
            PW_1.request_mask()
        );
        let text = observation.describe();
        assert!(text.contains("well index 0"), "{text}");
        assert!(text.contains("NEVER CAME UP"), "{text}");
        assert!(text.contains("requesters: bios 1 driver 1"), "{text}");
    }

    #[test]
    fn fuse_timeouts_are_reported_but_do_not_short_circuit_hsw_well_enable() {
        let regs = powered_machine();
        regs.set(regs::SKL_FUSE_STATUS, 0);
        let observation = enable_well(&regs, PW_1, DmcPlatform::AlderLakeN).unwrap();
        assert_eq!(
            observation.pg0,
            Some(FusePoll::NotDistributed { pg: SKL_PG0 })
        );
        assert_eq!(
            observation.pg,
            Some(FusePoll::NotDistributed { pg: SKL_PG1 })
        );
        assert!(
            regs.writes()
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
    fn alder_lake_pw1_workaround_is_not_applied_to_other_platforms() {
        let regs = powered_machine();
        enable_well(&regs, PW_1, DmcPlatform::TigerLake).unwrap();
        assert!(
            !regs
                .writes()
                .iter()
                .any(|(name, _)| *name == regs::GEN8_CHICKEN_DCPR_1.name())
        );
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
        let observation = enable_well(&regs, PW_1, DmcPlatform::AlderLakeN).unwrap();
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
        assert_eq!(
            DDI_IO_A.request_registers.bios.name(),
            "ICL_PWR_WELL_CTL_DDI1"
        );
        assert_eq!(
            DDI_IO_A.request_registers.debug.name(),
            "ICL_PWR_WELL_CTL_DDI4"
        );
        assert_eq!(DDI_IO_A.request_registers.kvmr, None);
        assert_eq!(AUX_A.request_registers.bios.name(), "ICL_PWR_WELL_CTL_AUX1");
        assert_eq!(
            AUX_A.request_registers.debug.name(),
            "ICL_PWR_WELL_CTL_AUX4"
        );
        assert_eq!(AUX_A.request_registers.kvmr, None);
        assert_eq!(DDI_IO_A.request_mask(), 0x2);
        assert_eq!(AUX_B.state_mask(), 0x4);
        let adlp = power_wells(DmcPlatform::AlderLakeN);
        let pw_a = adlp
            .iter()
            .flat_map(|group| group.instances.iter())
            .find(|instance| instance.name == "PW_A")
            .copied()
            .unwrap();
        assert_eq!(pw_a.irq_pipe_mask, 1 << 0);
        assert_eq!(mapped_hsw_well(pw_a).unwrap().irq_pipe_mask, 1 << 0);
        for (name, expected) in [
            ("DDI_IO_A", DDI_IO_A),
            ("DDI_IO_B", DDI_IO_B),
            ("DDI_IO_TC1", DDI_IO_TC1),
            ("DDI_IO_TC2", DDI_IO_TC2),
            ("AUX_A", AUX_A),
            ("AUX_B", AUX_B),
            ("AUX_USBC1", AUX_TC1),
            ("AUX_USBC2", AUX_TC2),
        ] {
            let instance = adlp
                .iter()
                .flat_map(|group| group.instances.iter())
                .find(|instance| instance.name == name)
                .copied()
                .unwrap();
            assert_eq!(mapped_hsw_well(instance), Some(expected), "{name}");
        }
    }

    #[test]
    fn power_state_get_put_uses_map_driven_aux_domain_references() {
        let regs = powered_machine();
        let mut state = bring_up(&regs).unwrap();
        assert!(
            !state
                .is_domain_enabled(&regs, PowerDomain::AuxIoA, true)
                .unwrap()
        );
        assert!(
            !state
                .get_domain_if_enabled(&regs, PowerDomain::AuxIoA, true)
                .unwrap()
        );

        state.get_domain(&regs, PowerDomain::AuxIoA).unwrap();
        assert!(
            state
                .is_domain_enabled(&regs, PowerDomain::AuxIoA, true)
                .unwrap()
        );
        assert!(
            state
                .get_domain_if_enabled(&regs, PowerDomain::AuxIoA, true)
                .unwrap()
        );
        assert_eq!(state.power_domains.domain_use_count(PowerDomain::AuxIoA), 2);

        state.put_domain(&regs, PowerDomain::AuxIoA).unwrap();
        state.put_domain(&regs, PowerDomain::AuxIoA).unwrap();
        assert_eq!(state.power_domains.domain_use_count(PowerDomain::AuxIoA), 0);
        assert!(
            !state
                .is_domain_enabled(&regs, PowerDomain::AuxIoA, true)
                .unwrap()
        );
    }

    #[test]
    fn type_c_aux_domains_map_source_wells_and_clear_tbt_mode_before_request() {
        let regs = powered_machine();
        let mut state = bring_up(&regs).unwrap();

        state.get_domain(&regs, PowerDomain::AuxUsbc1).unwrap();
        assert_eq!(
            state.power_domains.domain_use_count(PowerDomain::AuxUsbc1),
            1
        );
        assert_eq!(
            regs.read(regs::aux::DP_AUX_CH_CTL_D).unwrap() & (1 << 11),
            0,
            "legacy TC AUX must clear TBT_IO before the source well request"
        );
        assert_ne!(
            regs.read(regs::ICL_PWR_WELL_CTL_AUX2).unwrap() & AUX_TC1.state_mask(),
            0
        );

        state.put_domain(&regs, PowerDomain::AuxUsbc1).unwrap();
        assert_eq!(
            state.power_domains.domain_use_count(PowerDomain::AuxUsbc1),
            0
        );
    }

    #[test]
    fn a_well_state_timeout_leaves_hsw_request_bit_for_domain_owner_cleanup() {
        let regs = powered_machine();
        regs.derive(regs::HSW_PWR_WELL_CTL2, |written| {
            written & !PW_1.state_mask()
        });
        let observation = enable_well(&regs, PW_1, DmcPlatform::AlderLakeN).unwrap();
        assert!(!observation.state_set);
        assert_eq!(
            regs.read(regs::HSW_PWR_WELL_CTL2).unwrap() & PW_1.request_mask(),
            PW_1.request_mask(),
            "i915 leaves the request to the power-domain refcount owner"
        );

        // A request bit that was already set is not this call's to withdraw.
        let regs = powered_machine();
        regs.set(regs::HSW_PWR_WELL_CTL2, PW_1.request_mask());
        regs.derive(regs::HSW_PWR_WELL_CTL2, |written| {
            written & !PW_1.state_mask()
        });
        let observation = enable_well(&regs, PW_1, DmcPlatform::AlderLakeN).unwrap();
        assert!(!observation.state_set);
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
