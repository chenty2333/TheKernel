//! The display clock: the CDCLK reference frequency, the CDCLK PLL and its
//! ratio table, and the south display's raw clock.
//!
//! Three facts decide everything here, and all three are read from the machine
//! rather than assumed:
//!
//! * **Which reference frequency the CDCLK ratios are relative to.**
//!   `SKL_DSSM[31:29]` says 24, 19.2 or 38.4 MHz.  The reference document
//!   (§4.6, §13.1 item 4) records a genuine disagreement between Intel's DG1
//!   PRM, which states 38.4 MHz as a fixed property of the part, and Linux
//!   `drm/i915`, which reads it out of `SKL_DSSM` — and it calls out an
//!   in-flight ADL-N report failing on exactly that assumption, with every
//!   ratio wrong by a factor of two.  §13.3 resolves it in favour of the
//!   readback.  So [`reference_clock`] decodes the register and this module
//!   never has a default.
//! * **Which CDCLK values are legal.**  `adlp_cdclk_table` in `[I915]`
//!   `display/intel_cdclk.c:1354-1373` is the ADL-P/N table — ADL-N is a
//!   subplatform of ADL-P and not at stepping A0/B0, so it takes the same
//!   path — and it is reproduced here as [`ADL_N_CDCLK_TABLE`].
//! * **Whether the firmware's CDCLK is already good.**  `bxt_cdclk_init_hw`
//!   sanitises and then *returns early* if the PLL is enabled and locked with
//!   a VCO the table knows, rather than reprogramming.  Reference §4.6
//!   ("Changing CDCLK") and §11 phase 1.4.  [`observe`] therefore reads
//!   `CDCLK_PLL_ENABLE` and `CDCLK_CTL` before anything is written, and
//!   [`CdclkObservation::usable`] decides whether the programming path is
//!   reached at all.
//!
//! The arithmetic is separated from the registers on purpose: [`cd2x_divider`],
//! [`cdclk_decimal`] and the table are pure functions with published values to
//! test against, and the code that touches `CDCLK_PLL_ENABLE` is short enough
//! to check against §4.6 by eye.
//!
//! ## What is not here
//!
//! * **The PCode "prepare for change" handshake.**  `[I915]` `bxt_set_cdclk`
//!   begins with `skl_pcode_request(SKL_PCODE_CDCLK_CONTROL,
//!   SKL_CDCLK_PREPARE_FOR_CHANGE, ...)` and ends by writing the voltage level.
//!   This kernel has no PCode mailbox, so neither is done.  The programming
//!   path below is deliberately the shorter one §11 phase 1.4 gives for a
//!   machine with no pipe running, and the omission is reported in
//!   `docs/design/intel-power.md` rather than hidden.  It is only reached when
//!   the firmware left no usable CDCLK, which on a machine whose firmware drove
//!   the screen should not happen.
//! * **CDCLK crawl.**  `XE_LPD` has `has_cdclk_crawl`, and `adlp_cdclk_pll_crawl`
//!   retunes a *running* PLL without disabling it.  It is not reachable here:
//!   `bxt_de_pll_readout` reports a VCO of zero unless the PLL is enabled *and*
//!   locked, so every state that reaches the programming path is one i915 also
//!   treats as a disable/enable (`[I915]` `display/intel_cdclk.c:2069-2135`).
//! * **The voltage-level table.**  `[GAP]` — §4.6 and §13.1 item 6: the table
//!   is Gen12-specific and was not verified against a PRM.  Since no PCode
//!   write happens, no voltage level is computed.

use alloc::{format, string::String};

use super::regs::{self, Registers};

/// The CDCLK PLL's reference frequency, as `SKL_DSSM[31:29]` states it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReferenceClock {
    /// `001b` — 19.2 MHz.
    Mhz19_2,
    /// `000b` — 24 MHz.
    Mhz24,
    /// `010b` — 38.4 MHz.
    Mhz38_4,
}

impl ReferenceClock {
    /// The frequency in kHz, which is how every comparison here is done:
    /// integer kilohertz, so a table row is an equality test rather than a
    /// floating-point tolerance.
    pub(crate) const fn khz(self) -> u32 {
        match self {
            Self::Mhz19_2 => 19_200,
            Self::Mhz24 => 24_000,
            Self::Mhz38_4 => 38_400,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Mhz19_2 => "19.2 MHz",
            Self::Mhz24 => "24 MHz",
            Self::Mhz38_4 => "38.4 MHz",
        }
    }

    /// The frequency the display runs at when the CDCLK PLL is off.
    ///
    /// `[I915]` `bxt_get_cdclk` (`display/intel_cdclk.c`): on display version 12
    /// and later the bypass clock is the reference divided by two.  It is worth
    /// naming because it is what a machine with no CDCLK PLL at all would give
    /// the display, and because a "CDCLK" of exactly this value is how a
    /// caller recognises that the PLL never came up.
    pub(crate) const fn bypass_khz(self) -> u32 {
        self.khz() / 2
    }
}

/// Decode `SKL_DSSM[31:29]` into the CDCLK reference frequency.
///
/// Returns the frequency and whether the encoding was one of the three the
/// documentation defines.  An unrecognised encoding is reported as 24 MHz,
/// which is what `[I915]` `icl_readout_refclk` does — it logs a missing case and
/// falls through to 24 MHz — but the `false` lets a caller say so in a log
/// instead of quietly using a frequency nobody stated.  Reference §4.6
/// ("The reference clock") and §12.1; `[I915]` `i915_reg.h:2880-2884` and
/// `display/intel_cdclk.c:1585-1604`.
pub(crate) fn reference_clock(dssm: u32) -> (ReferenceClock, bool) {
    match dssm >> 29 & 0b111 {
        0b000 => (ReferenceClock::Mhz24, true),
        0b001 => (ReferenceClock::Mhz19_2, true),
        0b010 => (ReferenceClock::Mhz38_4, true),
        // The three remaining encodings are not defined by any source read for
        // this driver.  i915's fallthrough to 24 MHz is reproduced because it
        // is the value a caller has to use to make progress, and the flag is
        // what keeps that from being an unstated assumption.
        _ => (ReferenceClock::Mhz24, false),
    }
}

/// One row of the ADL-N CDCLK table: a legal CDCLK, its PLL ratio, and the
/// reference frequency that ratio is relative to.
///
/// The VCO is `ratio * reference`; the CD2X divider is then `vco / cdclk`, which
/// must come out as one of 2, 3, 4 or 8 (`[I915]` `bxt_cdclk_cd2x_div_sel`,
/// `display/intel_cdclk.c`: `cdclk = vco / 2 / div{1,1.5,2,4}`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CdclkEntry {
    pub(crate) reference_khz: u32,
    pub(crate) cdclk_khz: u32,
    pub(crate) ratio: u32,
}

impl CdclkEntry {
    pub(crate) const fn reference(self) -> ReferenceClock {
        match self.reference_khz {
            19_200 => ReferenceClock::Mhz19_2,
            24_000 => ReferenceClock::Mhz24,
            _ => ReferenceClock::Mhz38_4,
        }
    }

    /// The PLL output frequency, `ratio * reference`.
    pub(crate) const fn vco_khz(self) -> u32 {
        self.ratio * self.reference_khz
    }
}

/// The CDCLK values ADL-N may use, per reference frequency.
///
/// `[I915]` `adlp_cdclk_table`, `display/intel_cdclk.c:1354-1373`, which the
/// reference document reproduces as §4.6's "CDCLK ratios for ADL-N".  The rows
/// are grouped by reference frequency and ascending within a group, which is
/// the order `bxt_calc_cdclk` relies on when it takes "the first row whose
/// reference matches and whose CDCLK is at least the requested minimum".
///
/// Note that the 24 MHz column's lowest entry is 176.000 MHz, not the 180 MHz
/// an older platform's table gives: §13.3 records that `adlp_cdclk_table`
/// replaced `180.000/ratio 15` with `176.000/ratio 22`, and that i915's
/// platform-specific table is the one to trust.
pub(crate) const ADL_N_CDCLK_TABLE: &[CdclkEntry] = &[
    CdclkEntry {
        reference_khz: 19_200,
        cdclk_khz: 172_800,
        ratio: 27,
    },
    CdclkEntry {
        reference_khz: 19_200,
        cdclk_khz: 192_000,
        ratio: 20,
    },
    CdclkEntry {
        reference_khz: 19_200,
        cdclk_khz: 307_200,
        ratio: 32,
    },
    CdclkEntry {
        reference_khz: 19_200,
        cdclk_khz: 556_800,
        ratio: 58,
    },
    CdclkEntry {
        reference_khz: 19_200,
        cdclk_khz: 652_800,
        ratio: 68,
    },
    CdclkEntry {
        reference_khz: 24_000,
        cdclk_khz: 176_000,
        ratio: 22,
    },
    CdclkEntry {
        reference_khz: 24_000,
        cdclk_khz: 192_000,
        ratio: 16,
    },
    CdclkEntry {
        reference_khz: 24_000,
        cdclk_khz: 312_000,
        ratio: 26,
    },
    CdclkEntry {
        reference_khz: 24_000,
        cdclk_khz: 552_000,
        ratio: 46,
    },
    CdclkEntry {
        reference_khz: 24_000,
        cdclk_khz: 648_000,
        ratio: 54,
    },
    CdclkEntry {
        reference_khz: 38_400,
        cdclk_khz: 179_200,
        ratio: 14,
    },
    CdclkEntry {
        reference_khz: 38_400,
        cdclk_khz: 192_000,
        ratio: 10,
    },
    CdclkEntry {
        reference_khz: 38_400,
        cdclk_khz: 307_200,
        ratio: 16,
    },
    CdclkEntry {
        reference_khz: 38_400,
        cdclk_khz: 556_800,
        ratio: 29,
    },
    CdclkEntry {
        reference_khz: 38_400,
        cdclk_khz: 652_800,
        ratio: 34,
    },
];

/// The lowest CDCLK this reference frequency may run at.
///
/// `[I915]` `bxt_cdclk_init_hw` programs `bxt_calc_cdclk(dev_priv, 0)` — the
/// first row for the reference frequency — when it finds no usable CDCLK, so
/// this is what the programming path asks for.
pub(crate) fn lowest_cdclk(clock: ReferenceClock) -> Option<CdclkEntry> {
    ADL_N_CDCLK_TABLE
        .iter()
        .filter(|entry| entry.reference_khz == clock.khz())
        .min_by_key(|entry| entry.cdclk_khz)
        .copied()
}

/// The table row that states this exact combination, if there is one.
///
/// This is the test `bxt_sanitize_cdclk` makes, in two steps: that the CDCLK is
/// a legal value for the platform, and that the VCO matches the one that CDCLK
/// should have.  Requiring the row to match on all three of reference, CDCLK and
/// ratio makes both checks at once, and it cannot accept a CDCLK whose ratio
/// belongs to a different reference frequency — which is the shape of the
/// 38.4-versus-19.2 failure §4.6 warns about.
pub(crate) fn entry_for(clock: ReferenceClock, cdclk_khz: u32, ratio: u32) -> Option<CdclkEntry> {
    ADL_N_CDCLK_TABLE.iter().copied().find(|entry| {
        entry.reference_khz == clock.khz() && entry.cdclk_khz == cdclk_khz && entry.ratio == ratio
    })
}

/// The CD2X divider, as `CDCLK_CTL[23:22]` encodes it.
///
/// `[I915]` `bxt_cdclk_cd2x_div_sel` (`display/intel_cdclk.c`) and
/// `i915_reg.h:4069-4073`; reference §4.6's `CD2X_DIV_SEL` table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Cd2xDivider {
    /// `00b` — `cdclk = vco / 2`.
    Div1,
    /// `01b` — `cdclk = vco / 3`.
    Div1_5,
    /// `10b` — `cdclk = vco / 4`.
    Div2,
    /// `11b` — `cdclk = vco / 8`.
    Div4,
}

impl Cd2xDivider {
    /// The divisor the hardware applies to the VCO, which is twice the field's
    /// name: the field names the CD2X divider, the pipe sees `vco / 2 / div`.
    pub(crate) const fn divisor(self) -> u32 {
        match self {
            Self::Div1 => 2,
            Self::Div1_5 => 3,
            Self::Div2 => 4,
            Self::Div4 => 8,
        }
    }

    /// The value to place in `CDCLK_CTL[23:22]`.
    pub(crate) const fn field(self) -> u32 {
        match self {
            Self::Div1 => 0,
            Self::Div1_5 => 1,
            Self::Div2 => 2,
            Self::Div4 => 3,
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Div1 => "divide by 1",
            Self::Div1_5 => "divide by 1.5",
            Self::Div2 => "divide by 2",
            Self::Div4 => "divide by 4",
        }
    }

    /// Decode `CDCLK_CTL[23:22]`.
    ///
    /// All four two-bit encodings are named, so this is total: the field cannot
    /// hold a value that means nothing.  The two that matter for ADL-N's lowest
    /// entries are `Div1_5` and `Div1`; §4.6 records that the TGL and DG1 PRMs
    /// list only `00b` and `10b` while i915 names all four, and marks ADL-N's
    /// having all four as `[INF]`.  This driver therefore treats `01b` and `11b`
    /// as legal but reports the divider it decoded, so a machine that turned out
    /// to disagree would be visible in the bring-up log rather than silent.
    pub(crate) const fn decode(field: u32) -> Self {
        match field & 0b11 {
            0 => Self::Div1,
            1 => Self::Div1_5,
            2 => Self::Div2,
            _ => Self::Div4,
        }
    }
}

/// The divider that turns `vco_khz` into `cdclk_khz`, if the two are exactly
/// related by one of the four legal dividers.
///
/// Exactness is required rather than a nearest fit.  `[I915]` rounds
/// (`DIV_ROUND_CLOSEST`), but every row of the ADL-N table divides exactly, so a
/// remainder here means the caller's arithmetic is wrong rather than that the
/// hardware will round — and silently rounding a wrong VCO into a legal divider
/// is how a CDCLK ends up at a frequency nobody asked for.
pub(crate) fn cd2x_divider(vco_khz: u32, cdclk_khz: u32) -> Option<Cd2xDivider> {
    if cdclk_khz == 0 || !vco_khz.is_multiple_of(cdclk_khz) {
        return None;
    }
    match vco_khz / cdclk_khz {
        2 => Some(Cd2xDivider::Div1),
        3 => Some(Cd2xDivider::Div1_5),
        4 => Some(Cd2xDivider::Div2),
        8 => Some(Cd2xDivider::Div4),
        _ => None,
    }
}

/// `CDCLK_CTL[10:0]`, the CDCLK frequency in the format the hardware states it.
///
/// `[I915]` `skl_cdclk_decimal` (`display/intel_cdclk.c:1024-1027`):
/// `DIV_ROUND_CLOSEST(cdclk_khz - 1000, 500)`, i.e. U10.1 of
/// `round_to_0.5MHz(cdclk) - 1 MHz`, which is what the PRM states and what the
/// reference document checks the two against each other for at 307.2 and
/// 172.8 MHz (§4.6).  Integer arithmetic throughout: `(cdclk - 1000 + 250) / 500`
/// is `DIV_ROUND_CLOSEST` for the positive values this is ever called with.
pub(crate) const fn cdclk_decimal(cdclk_khz: u32) -> u32 {
    (cdclk_khz + 250 - 1_000) / 500
}

/// `CDCLK_CTL[23:22]`'s pipe field for pipes A..D.
///
/// `[I915]` `TGL_CDCLK_CD2X_PIPE(pipe) = pipe << 20` for display version 12 and
/// later, so A is 0, B is bit 20, C is bit 21 and D is both; §4.6 records that
/// the TGL PRM's table and this expression agree bit for bit.
pub(crate) const fn cd2x_pipe(pipe: u8) -> u32 {
    (pipe as u32) << 20
}

/// `CDCLK_CTL[21:19] = 111b`: the divider is not synchronised to any pipe.
///
/// `[I915]` `TGL_CDCLK_CD2X_PIPE_NONE` (`i915_reg.h:4080`), which is what
/// `bxt_set_cdclk` writes when it is called with `INVALID_PIPE` — the case
/// `bxt_cdclk_init_hw` is in, because no pipe is running when CDCLK is
/// initialised.  §11 phase 1.4 shows `CD2X_PIPE(A)` instead, and
/// `bxt_sanitize_cdclk` explicitly ignores the field when it decides whether a
/// firmware-programmed value is acceptable, so the two are interchangeable to
/// the hardware's own sanitiser; this driver writes "none" because that is what
/// the state actually is.
pub(crate) const CD2X_PIPE_NONE: u32 = 7 << 19;

/// The `CDCLK_CTL` value that selects this table row.
///
/// `[I915]` `bxt_cdclk_ctl` assembles exactly these three fields and nothing
/// else — the register's other fields (`CDCLK_FREQ_SEL`, `MDCLK_SOURCE_SEL`,
/// `SSA_PRECHARGE_ENABLE`) are not part of a Gen12 CDCLK selection — so a
/// full-register write of this value is what the vendor driver does rather than
/// an omission of fields that should have been preserved.
pub(crate) fn cdclk_ctl_value(entry: CdclkEntry, divider: Cd2xDivider, pipe: u32) -> u32 {
    (divider.field() << 22) | pipe | cdclk_decimal(entry.cdclk_khz)
}

/// What the CDCLK registers said before anything was written.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CdclkObservation {
    /// `SKL_DSSM[31:29]`, decoded.
    pub(crate) reference: ReferenceClock,
    /// Whether that encoding was one the documentation defines.
    pub(crate) reference_recognised: bool,
    /// The raw `CDCLK_PLL_ENABLE` value.
    pub(crate) pll_register: u32,
    pub(crate) pll_enabled: bool,
    pub(crate) pll_locked: bool,
    /// `CDCLK_PLL_ENABLE[7:0]`.
    pub(crate) ratio: u32,
    /// `ratio * reference`, or zero when the PLL is not enabled and locked.
    pub(crate) vco_khz: u32,
    /// The CD2X divider decoded from `CDCLK_CTL[23:22]`.
    pub(crate) divider: Cd2xDivider,
    /// The frequency the display is running at: the VCO over the divider, or
    /// the bypass clock when the PLL is not up.
    pub(crate) cdclk_khz: u32,
    /// The raw `CDCLK_CTL` value.
    pub(crate) ctl: u32,
    /// `CDCLK_CTL[10:0]`.
    pub(crate) decimal_field: u32,
    /// `CDCLK_CTL[21:19]`.
    pub(crate) pipe_field: u32,
    /// The table row this CDCLK matches on reference, frequency and ratio.
    pub(crate) entry: Option<CdclkEntry>,
}

impl CdclkObservation {
    /// Whether this CDCLK can be kept.
    ///
    /// Three things have to hold, and each is a way the firmware could have left
    /// something the display cannot use: the PLL is enabled, it has locked, and
    /// the reference/frequency/ratio triple is a row of the platform's table.
    /// `[I915]` `bxt_sanitize_cdclk` makes the same three decisions and
    /// reprograms if any of them fails.
    pub(crate) fn usable(&self) -> bool {
        self.pll_enabled && self.pll_locked && self.entry.is_some()
    }

    /// The `CDCLK_CTL` decimal field this frequency should have had.
    pub(crate) fn decimal_expected(&self) -> Option<u32> {
        self.entry.map(|entry| cdclk_decimal(entry.cdclk_khz))
    }

    /// A line for the bring-up log.
    pub(crate) fn describe(&self) -> String {
        let mut text = format!(
            "CDCLK: reference {} (DSSM[31:29] {:#05b}{}), PLL {}, ratio {}, VCO {} kHz, CD2X {}, \
             CDCLK {} kHz, decimal {:#05x}",
            self.reference.name(),
            self.pll_register >> 29 & 0b111,
            if self.reference_recognised {
                ""
            } else {
                ", NOT a documented encoding: 24 MHz assumed as i915 does, every ratio is suspect"
            },
            if self.pll_enabled {
                "enabled"
            } else {
                "disabled"
            },
            self.ratio,
            self.vco_khz,
            self.divider.name(),
            self.cdclk_khz,
            self.decimal_field,
        );
        if self.entry.is_none() && self.pll_enabled {
            text.push_str(&format!(
                "; no ADL-N table row is {}/{}/ratio {}",
                self.reference.name(),
                self.cdclk_khz,
                self.ratio
            ));
        }
        if let Some(expected) = self.decimal_expected()
            && expected != self.decimal_field
        {
            text.push_str(&format!(
                "; decimal field says {:#05x} where the table implies {expected:#05x} (left \
                 alone: the frequency is what matters and rewriting the field would be a CDCLK \
                 change)",
                self.decimal_field,
            ));
        }
        text
    }
}

/// Read `SKL_DSSM`, `CDCLK_PLL_ENABLE` and `CDCLK_CTL` and work out what the
/// display clock is doing.
///
/// `[I915]` `bxt_de_pll_readout` plus `bxt_get_cdclk`: the reference comes from
/// `DSSM`, the ratio from the PLL enable register, the VCO is `ratio *
/// reference` — but only when the PLL is enabled *and* locked, because the ratio
/// of an unlocked PLL says nothing — and the divider comes from `CDCLK_CTL`.
pub(crate) fn observe(regs: &impl Registers) -> Result<CdclkObservation, ClockError> {
    let dssm = read(regs, regs::SKL_DSSM)?;
    let pll_register = read(regs, regs::CDCLK_PLL_ENABLE)?;
    let ctl = read(regs, regs::CDCLK_CTL)?;

    let (reference, reference_recognised) = reference_clock(dssm);
    let pll_enabled = pll_register >> 31 & 1 == 1;
    let pll_locked = pll_register >> 30 & 1 == 1;
    let ratio = pll_register & 0xFF;
    let vco_khz = if pll_enabled && pll_locked {
        ratio * reference.khz()
    } else {
        0
    };
    let divider = Cd2xDivider::decode(ctl >> 22 & 0b11);
    let cdclk_khz = if vco_khz == 0 {
        reference.bypass_khz()
    } else {
        vco_khz / divider.divisor()
    };
    let entry = entry_for(reference, cdclk_khz, ratio);

    Ok(CdclkObservation {
        reference,
        reference_recognised,
        pll_register,
        pll_enabled,
        pll_locked,
        ratio,
        vco_khz,
        divider,
        cdclk_khz,
        ctl,
        decimal_field: ctl & 0x7FF,
        pipe_field: ctl >> 19 & 0b111,
        entry,
    })
}

/// Program the CDCLK PLL and `CDCLK_CTL` for a table row.
///
/// This is the fallback: it is only reached when [`CdclkObservation::usable`]
/// said the firmware left nothing the display can run on.  The sequence is
/// §11 phase 1.4, which is `[I915]` `icl_cdclk_pll_enable` followed by
/// `bxt_cdclk_ctl`:
///
/// ```text
/// write(CDCLK_PLL_ENABLE, RATIO)                  /* ratio, PLL still off */
/// write(CDCLK_PLL_ENABLE, RATIO | PLL_ENABLE)     /* bit 31 */
/// poll(CDCLK_PLL_ENABLE.LOCK)                     /* bit 30, 200 us in the spec */
/// write(CDCLK_CTL, DIV_SEL | CD2X_PIPE | DECIMAL)
/// ```
///
/// There is deliberately **no** bit-27 step: §4.6 records that
/// `CDCLK_PLL_ENABLE[27]` is the TGL PRM's "slow clock enable", while i915's
/// `PLL_POWER_ENABLE` of the same bit number belongs to the *combo DPLL*
/// registers `0x46010`/`0x46014`, and warns that carrying that step into this
/// sequence was an error in an earlier draft of the reference.
pub(crate) fn program(
    regs: &impl Registers,
    entry: CdclkEntry,
) -> Result<CdclkProgrammed, ClockError> {
    let divider = cd2x_divider(entry.vco_khz(), entry.cdclk_khz).ok_or(ClockError::TableRow {
        reference_khz: entry.reference_khz,
        cdclk_khz: entry.cdclk_khz,
        ratio: entry.ratio,
    })?;

    // The ratio goes in first, with the PLL still disabled: `CDCLK_PLL_ENABLE`
    // holds the ratio in bits [7:0] and the enable in bit 31, and writing the
    // ratio while the PLL runs would be the crawl sequence instead.
    write(regs, regs::CDCLK_PLL_ENABLE, entry.ratio)?;
    let enable = entry.ratio | PLL_ENABLE;
    write(regs, regs::CDCLK_PLL_ENABLE, enable)?;

    // Poll for lock.  The reference gives 200 us (spec) and i915 waits 1 ms
    // (`intel_de_wait_for_set(..., BXT_DE_PLL_LOCK, 1)`); the longer bound is
    // used because it is the one the working vendor driver uses.
    let locked = poll(
        regs,
        regs::CDCLK_PLL_ENABLE,
        PLL_LOCK,
        PLL_LOCK,
        PLL_LOCK_TIMEOUT_US,
    )?;
    if !locked {
        return Err(ClockError::PllNeverLocked {
            readback: read(regs, regs::CDCLK_PLL_ENABLE)?,
        });
    }

    let ctl = cdclk_ctl_value(entry, divider, CD2X_PIPE_NONE);
    write(regs, regs::CDCLK_CTL, ctl)?;

    // The write is posted, so the read-back is what says the device took it.
    // Only the fields this sequence sets are compared: the register has bits
    // (`CDCLK_FREQ_SEL`, `MDCLK_SOURCE_SEL`) this driver deliberately writes as
    // zero, and a readback that disagreed on those would be a different fact.
    let readback = read(regs, regs::CDCLK_CTL)?;
    let fields = CDCLK_CTL_DIV_SEL_MASK | CDCLK_CTL_PIPE_MASK | CDCLK_CTL_DECIMAL_MASK;
    if readback & fields != ctl & fields {
        return Err(ClockError::ReadbackMismatch {
            register: regs::CDCLK_CTL.name(),
            wrote: ctl,
            read: readback,
        });
    }

    Ok(CdclkProgrammed {
        entry,
        divider,
        ratio_register: enable | PLL_LOCK,
    })
}

/// What [`program`] wrote.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CdclkProgrammed {
    pub(crate) entry: CdclkEntry,
    pub(crate) divider: Cd2xDivider,
    /// The PLL register after the sequence: ratio, `PLL_ENABLE`, and `LOCK` by
    /// the time it read back.
    pub(crate) ratio_register: u32,
}

/// The raw clock, as `SFUSE_STRAP[8]` states it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RawClockPlan {
    /// True when the strap selects 24 MHz, false when it selects 19.2 MHz.
    pub(crate) is_24_mhz: bool,
    /// The frequency in kHz.
    pub(crate) khz: u32,
    /// The `PCH_RAWCLK_FREQ` value that states it.
    pub(crate) register: u32,
}

/// Decode `SFUSE_STRAP[8]` into the raw clock and the register value that
/// describes it.
///
/// `[I915]` `cnp_rawclk` (`display/intel_cdclk.c:3560-3580`), which the
/// reference gives in §4.8 and which applies to ADL-N because the platform is
/// in the `PCH_CNP`..`PCH_DG1` range i915 tests for:
///
/// ```text
/// SFUSE_STRAP_RAW_FREQUENCY set   -> 24.0 MHz: DIV(24)
/// clear                           -> 19.2 MHz: DIV(19) | DEN(4) | NUM(1)
/// ```
///
/// `[PRM]` says something different — that the raw clock is expected to be
/// 38.4 MHz and that the register defaults to it — and §4.8 records the
/// disagreement.  The strap is used, because the value varies between platforms
/// and the code path exists precisely because of that; the firmware's own
/// register is read and logged alongside it so a disagreement is visible.
pub(crate) fn raw_clock_plan(sfuse_strap: u32) -> RawClockPlan {
    if sfuse_strap >> SFUSE_STRAP_RAW_FREQUENCY_BIT & 1 == 1 {
        RawClockPlan {
            is_24_mhz: true,
            khz: 24_000,
            // CNP_RAWCLK_DIV(24000 / 1000); no numerator or denominator because
            // there is no fractional part to state.
            register: 24 << 16,
        }
    } else {
        RawClockPlan {
            is_24_mhz: false,
            khz: 19_200,
            // CNP_RAWCLK_DIV(19) | CNP_RAWCLK_DEN(4) | ICP_RAWCLK_NUM(1):
            // 19 MHz plus one fifth of a megahertz.
            register: (19 << 16) | (4 << 26) | (1 << 11),
        }
    }
}

/// What `PCH_RAWCLK_FREQ` said, decoded as far as this driver can decode it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawClockProgrammed {
    /// The register states one of the two frequencies this driver programs.
    Known(u32),
    /// The register holds something else.  The raw value is kept rather than
    /// guessed at.
    ///
    /// This is not defensive padding: `intel_pch_rawclk`'s DG1 path programs
    /// `DEN(4) | DIV(37) | NUM(2)` for 38.4 MHz, and that value does *not*
    /// decode to 38.4 MHz under the arithmetic `cnp_rawclk` uses to build its
    /// two values, so the encoding for a frequency this driver never writes is
    /// genuinely not established here.  §4.8 and §13.1 item 5.
    Unrecognised(u32),
}

impl RawClockProgrammed {
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Known(khz) => format!("{khz} kHz"),
            Self::Unrecognised(register) => format!(
                "{register:#010x}, which is not one of the two encodings this driver knows how to \
                 decode"
            ),
        }
    }
}

/// Decode a `PCH_RAWCLK_FREQ` value.
///
/// The inverse of the two encodings [`raw_clock_plan`] produces:
/// `khz = DIV * 1000 + NUM * 1000 / (DEN + 1)`, with `DIV` at `[25:16]`, `DEN`
/// at `[29:26]` and `NUM` at `[13:11]` (`[I915]` `i915_reg.h:3133-3143`).
/// A register of zero is unprogrammed, not a request to run the south display
/// at zero hertz.
pub(crate) fn decode_raw_clock(register: u32) -> RawClockProgrammed {
    let div = register >> 16 & 0x3FF;
    let den = (register >> 26 & 0xF) + 1;
    let num = register >> 11 & 0b111;
    let khz = div * 1000 + num * 1000 / den;
    match khz {
        24_000 => RawClockProgrammed::Known(24_000),
        19_200 => RawClockProgrammed::Known(19_200),
        _ => RawClockProgrammed::Unrecognised(register),
    }
}

/// What the raw clock is, before and after.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RawClockState {
    pub(crate) plan: RawClockPlan,
    /// What the register held when it was first read.
    pub(crate) was: RawClockProgrammed,
    /// The register value after bring-up.
    pub(crate) now: u32,
    /// Whether the register needed writing at all.
    pub(crate) programmed: bool,
}

impl RawClockState {
    pub(crate) fn describe(&self) -> String {
        let mut text = format!(
            "raw clock: SFUSE_STRAP[8]={} states {} kHz, PCH_RAWCLK_FREQ was {}, ",
            u8::from(self.plan.is_24_mhz),
            self.plan.khz,
            self.was.describe(),
        );
        if self.programmed {
            text.push_str(&format!("programmed {:#010x}", self.now,));
        } else {
            text.push_str("already correct, left alone");
        }
        if let RawClockProgrammed::Known(khz) = self.was
            && khz != self.plan.khz
        {
            text.push_str(
                "; the firmware and the strap DISAGREE about the crystal and the strap won -- if \
                 GMBUS or hotplug de-glitching misbehaves later, this is the first thing to \
                 revisit",
            );
        }
        text
    }
}

/// Read `PCH_RAWCLK_FREQ`, and state the strap's frequency in it if it does not
/// already.
///
/// §4.8: the raw clock must be programmed before any south display function is
/// enabled, or GMBUS arbitration and hotplug de-glitching are mis-timed — which
/// shows up as *intermittent* EDID failures rather than clean ones, and is
/// therefore expensive to diagnose later.  §12.3 says to read the register
/// first, because after a vendor driver has run it is the ground truth; this
/// reads it, reports it, and writes only when it disagrees with the strap, which
/// is the hardware's own statement about the crystal.  A disagreement is carried
/// in [`RawClockState`] and printed, so a machine where the two differ is
/// visible rather than silently overruled.
pub(crate) fn bring_up_raw_clock(
    regs: &impl Registers,
    sfuse_strap: u32,
) -> Result<RawClockState, ClockError> {
    let plan = raw_clock_plan(sfuse_strap);
    let was_register = read(regs, regs::PCH_RAWCLK_FREQ)?;
    let was = decode_raw_clock(was_register);
    if was == RawClockProgrammed::Known(plan.khz) {
        return Ok(RawClockState {
            plan,
            was,
            now: was_register,
            programmed: false,
        });
    }
    write(regs, regs::PCH_RAWCLK_FREQ, plan.register)?;
    let now = read(regs, regs::PCH_RAWCLK_FREQ)?;
    if now != plan.register {
        return Err(ClockError::ReadbackMismatch {
            register: regs::PCH_RAWCLK_FREQ.name(),
            wrote: plan.register,
            read: now,
        });
    }
    Ok(RawClockState {
        plan,
        was,
        now,
        programmed: true,
    })
}

/// `CDCLK_PLL_ENABLE[31]`.
pub(crate) const PLL_ENABLE: u32 = 1 << 31;
/// `CDCLK_PLL_ENABLE[30]`.
pub(crate) const PLL_LOCK: u32 = 1 << 30;

/// `CDCLK_CTL[23:22]`.
pub(crate) const CDCLK_CTL_DIV_SEL_MASK: u32 = 0b11 << 22;
/// `CDCLK_CTL[21:19]`.
pub(crate) const CDCLK_CTL_PIPE_MASK: u32 = 0b111 << 19;
/// `CDCLK_CTL[10:0]`.
pub(crate) const CDCLK_CTL_DECIMAL_MASK: u32 = 0x7FF;

/// `SFUSE_STRAP[8]`, the raw clock strap.
pub(crate) const SFUSE_STRAP_RAW_FREQUENCY_BIT: u32 = 8;

/// How long to wait for the CDCLK PLL to lock.
///
/// The reference gives the spec's 200 us and i915's 1 ms
/// (`intel_de_wait_for_set(..., BXT_DE_PLL_LOCK, 1)`); the vendor driver's
/// longer bound is used, because a lock that arrives late is not a failure and
/// a timeout that is too tight would refuse a CDCLK that works.
pub(crate) const PLL_LOCK_TIMEOUT_US: u32 = 1_000;

/// What the CDCLK step did, and what the hardware said afterwards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CdclkState {
    /// What the registers said before anything was written.
    pub(crate) observed: CdclkObservation,
    /// Whether the firmware's CDCLK was kept.  This is the expected answer on a
    /// machine whose firmware drove the screen.
    pub(crate) kept_firmware: bool,
    /// What was programmed, when the firmware's CDCLK could not be kept.
    pub(crate) programmed: Option<CdclkProgrammed>,
    /// The registers re-read after programming: the evidence that the device is
    /// running what was asked for, rather than the write having been posted and
    /// forgotten.
    pub(crate) after: Option<CdclkObservation>,
}

impl CdclkState {
    pub(crate) fn describe(&self) -> String {
        let mut text = self.observed.describe();
        if self.kept_firmware {
            text.push_str(
                "; kept: `bxt_cdclk_init_hw` sanitises rather than reprograms and returns early \
                 on a legal CDCLK, and this one is legal",
            );
        } else if let Some(programmed) = self.programmed {
            text.push_str(&format!(
                "; no usable CDCLK was programmed, so {} kHz was (ratio {}, {}, decimal {:#05x})",
                programmed.entry.cdclk_khz,
                programmed.entry.ratio,
                programmed.divider.name(),
                cdclk_decimal(programmed.entry.cdclk_khz),
            ));
            if let Some(after) = self.after {
                text.push_str(&format!(
                    "; re-read: {} kHz at ratio {}, which {} a table row",
                    after.cdclk_khz,
                    after.ratio,
                    if after.entry.is_some() {
                        "is"
                    } else {
                        "is NOT"
                    },
                ));
            }
        }
        text
    }
}

/// Keep a usable CDCLK, or program one if there is none.
///
/// The order matters and is §11 phase 1.4's: read first, and only program when
/// what is there cannot be used.  `[I915]` `bxt_cdclk_init_hw` calls
/// `bxt_sanitize_cdclk` and then *returns early* when the PLL is enabled and
/// locked with a VCO the table knows, so a firmware CDCLK is the normal case,
/// not an unusual one.  Reprogramming it would be a frequency change on a
/// display that is already running, which the PRM says requires disabling every
/// display engine function first.
///
/// When there is nothing usable, the value programmed is the lowest the
/// platform's table allows for the reference the hardware reports — which is
/// `bxt_calc_cdclk(dev_priv, 0)`, i.e. the same choice i915 makes when it has
/// to initialise from scratch.
pub(crate) fn bring_up(regs: &impl Registers) -> Result<CdclkState, ClockError> {
    let observed = observe(regs)?;
    if observed.usable() {
        return Ok(CdclkState {
            observed,
            kept_firmware: true,
            programmed: None,
            after: None,
        });
    }

    let entry = lowest_cdclk(observed.reference).ok_or(ClockError::NoTableRow {
        reference_khz: observed.reference.khz(),
    })?;
    let programmed = program(regs, entry)?;

    // The write is posted, so the answer to "did it take" is a fresh read of
    // the same three registers the decision was made from.
    let after = observe(regs)?;
    if !after.usable() {
        return Err(ClockError::StillNotUsable {
            cdclk_khz: after.cdclk_khz,
            ratio: after.ratio,
            pll_register: after.pll_register,
        });
    }
    Ok(CdclkState {
        observed,
        kept_firmware: false,
        programmed: Some(programmed),
        after: Some(after),
    })
}

/// What can go wrong with the clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClockError {
    /// A register the sequence needs could not be read: it is outside the
    /// mapped window, which means the window is smaller than the table's own
    /// compile-time assertion assumed.
    Unreadable { register: &'static str },
    /// A write was refused, which for these registers means the same thing.
    WriteRefused { register: &'static str },
    /// The PLL never reported lock within the bound.
    PllNeverLocked { readback: u32 },
    /// A write did not read back as written.
    ReadbackMismatch {
        register: &'static str,
        wrote: u32,
        read: u32,
    },
    /// A row of this driver's own CDCLK table is not representable, which is a
    /// bug in the table rather than a fact about the hardware.
    TableRow {
        reference_khz: u32,
        cdclk_khz: u32,
        ratio: u32,
    },
    /// The table has no row for this reference frequency at all.  It cannot
    /// happen with the table as written -- every reference the decode can
    /// produce has rows -- so this is a guard against a table edit, and it is
    /// an error rather than a panic because a driver that panics on hardware
    /// nobody has characterised is worse than one that refuses to guess.
    NoTableRow { reference_khz: u32 },
    /// The CDCLK was programmed and still does not read back as a combination
    /// the table states.  The read-back is the point: a display running at a
    /// frequency nobody chose is the failure this whole module exists to
    /// prevent, and it must not be reported as success.
    StillNotUsable {
        cdclk_khz: u32,
        ratio: u32,
        pll_register: u32,
    },
}

impl ClockError {
    pub(crate) fn describe(self) -> String {
        match self {
            Self::Unreadable { register } => {
                format!("{register} could not be read: it is outside the mapped register window")
            }
            Self::WriteRefused { register } => format!(
                "{register} refused the write: either it is not declared writable or it is \
                 outside the mapped register window"
            ),
            Self::PllNeverLocked { readback } => format!(
                "the CDCLK PLL never reported lock (bit 30) within {PLL_LOCK_TIMEOUT_US} us; \
                 CDCLK_PLL_ENABLE reads {readback:#010x}.  Per reference section 4.6 this is \
                 almost always the wrong PLL ratio for the wrong reference frequency, so re-read \
                 SKL_DSSM before changing anything else"
            ),
            Self::ReadbackMismatch {
                register,
                wrote,
                read,
            } => format!(
                "{register} did not read back as written: wrote {wrote:#010x}, read {read:#010x}"
            ),
            Self::TableRow {
                reference_khz,
                cdclk_khz,
                ratio,
            } => format!(
                "the CDCLK table row {reference_khz} kHz reference / {cdclk_khz} kHz / ratio \
                 {ratio} has no legal CD2X divider, which is a bug in this driver's table"
            ),
            Self::NoTableRow { reference_khz } => format!(
                "the CDCLK table has no row for a {reference_khz} kHz reference frequency, which \
                 is a bug in this driver's table"
            ),
            Self::StillNotUsable {
                cdclk_khz,
                ratio,
                pll_register,
            } => format!(
                "the CDCLK was programmed and reads back as {cdclk_khz} kHz at ratio {ratio} \
                 (CDCLK_PLL_ENABLE {pll_register:#010x}), which is not a combination the ADL-N \
                 table states"
            ),
        }
    }
}

/// Read a register the sequence cannot do without.
pub(crate) fn read(regs: &impl Registers, register: regs::Register) -> Result<u32, ClockError> {
    regs.read(register).ok_or(ClockError::Unreadable {
        register: register.name(),
    })
}

/// Write a register, refusing to continue if the write did not happen.
pub(crate) fn write(
    regs: &impl Registers,
    register: regs::Register,
    value: u32,
) -> Result<(), ClockError> {
    if regs.write(register, value) {
        Ok(())
    } else {
        Err(ClockError::WriteRefused {
            register: register.name(),
        })
    }
}

/// Poll a register until `mask` reads `value`, or the poll budget runs out.
///
/// The budget is a count of reads rather than a clock reading; see
/// [`regs::poll`] for why, and [`regs::poll_attempts`] for how a documented
/// microsecond timeout becomes a count.
pub(crate) fn poll(
    regs: &impl Registers,
    register: regs::Register,
    mask: u32,
    value: u32,
    timeout_us: u32,
) -> Result<bool, ClockError> {
    regs::poll(regs, register, mask, value, timeout_us).ok_or(ClockError::Unreadable {
        register: register.name(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::intel::regs::mock::MockRegisters;

    /// Every reference frequency, for the table-driven tests.
    const REFERENCES: [ReferenceClock; 3] = [
        ReferenceClock::Mhz19_2,
        ReferenceClock::Mhz24,
        ReferenceClock::Mhz38_4,
    ];

    #[test]
    fn the_dssm_encoding_is_the_one_the_prm_and_i915_agree_on() {
        // The three documented encodings, bit for bit: `[I915]` i915_reg.h:2880-2884
        // (`ICL_DSSM_CDCLK_PLL_REFCLK_{24MHz,19_2MHz,38_4MHz}` = 0, 1, 2 << 29),
        // which §4.6 and §13.3 record as double-sourced against the TGL PRM.
        assert_eq!(reference_clock(0), (ReferenceClock::Mhz24, true));
        assert_eq!(reference_clock(1 << 29), (ReferenceClock::Mhz19_2, true));
        assert_eq!(reference_clock(2 << 29), (ReferenceClock::Mhz38_4, true));
        // The rest of the register must not change the answer: only [31:29] is
        // the field, and DSSM carries other live bits (bit 6 is DE_8k_DIS).
        for low in [0u32, 1 << 6, 0x1FFF_FFFF] {
            assert_eq!(reference_clock((1 << 29) | low).0, ReferenceClock::Mhz19_2);
        }
        // The three undefined encodings fall back to 24 MHz, as i915's
        // MISSING_CASE fallthrough does -- and say that they were not
        // recognised, so a log cannot present the fallback as a reading.
        for undefined in [3u32, 4, 7] {
            let (clock, recognised) = reference_clock(undefined << 29);
            assert_eq!(clock, ReferenceClock::Mhz24);
            assert!(!recognised, "{undefined:#b} is not a documented encoding");
        }
        assert_eq!(ReferenceClock::Mhz19_2.bypass_khz(), 9_600);
        assert_eq!(ReferenceClock::Mhz24.bypass_khz(), 12_000);
        assert_eq!(ReferenceClock::Mhz38_4.bypass_khz(), 19_200);
    }

    #[test]
    fn the_cdclk_table_is_the_adlp_table_the_reference_reproduces() {
        // §4.6's "CDCLK ratios for ADL-N" table, checked row for row, so a
        // transcription slip in either direction is caught.  `[I915]`
        // adlp_cdclk_table, display/intel_cdclk.c:1354-1373.
        let expected: [(u32, u32, u32); 15] = [
            (19_200, 172_800, 27),
            (19_200, 192_000, 20),
            (19_200, 307_200, 32),
            (19_200, 556_800, 58),
            (19_200, 652_800, 68),
            (24_000, 176_000, 22),
            (24_000, 192_000, 16),
            (24_000, 312_000, 26),
            (24_000, 552_000, 46),
            (24_000, 648_000, 54),
            (38_400, 179_200, 14),
            (38_400, 192_000, 10),
            (38_400, 307_200, 16),
            (38_400, 556_800, 29),
            (38_400, 652_800, 34),
        ];
        assert_eq!(ADL_N_CDCLK_TABLE.len(), expected.len());
        for (entry, (reference_khz, cdclk_khz, ratio)) in ADL_N_CDCLK_TABLE.iter().zip(expected) {
            assert_eq!(
                (entry.reference_khz, entry.cdclk_khz, entry.ratio),
                (reference_khz, cdclk_khz, ratio)
            );
        }
        // Grouped by reference and ascending within a group: `bxt_calc_cdclk`
        // walks the table and takes the first row that satisfies the minimum,
        // so a table out of order would silently pick a different frequency.
        for reference in REFERENCES {
            let rows: alloc::vec::Vec<u32> = ADL_N_CDCLK_TABLE
                .iter()
                .filter(|entry| entry.reference_khz == reference.khz())
                .map(|entry| entry.cdclk_khz)
                .collect();
            assert!(
                rows.windows(2).all(|pair| pair[0] < pair[1]),
                "{} rows are not ascending: {rows:?}",
                reference.name()
            );
        }
    }

    #[test]
    fn every_table_row_has_an_exact_legal_cd2x_divider() {
        // The check that ties the table to the hardware: the divider the
        // sequence programs has to come out of `vco / cdclk` as one of
        // 2, 3, 4 or 8.  If a ratio were transcribed wrongly this would fail,
        // which is exactly the failure §4.6 describes as "every ratio is wrong
        // by 2x" when the reference frequency is misread.
        for entry in ADL_N_CDCLK_TABLE {
            let divider = cd2x_divider(entry.vco_khz(), entry.cdclk_khz).unwrap_or_else(|| {
                panic!(
                    "{} / {} kHz has no legal divider",
                    entry.vco_khz(),
                    entry.cdclk_khz
                )
            });
            assert_eq!(entry.vco_khz() / divider.divisor(), entry.cdclk_khz);
            // And the divider field round-trips, which is what a read of the
            // firmware's register would be decoded with.
            assert_eq!(Cd2xDivider::decode(divider.field()), divider);
        }
        // Worked examples from §4.6, which states them explicitly: ref 19.2 with
        // cdclk 307.2 is ratio 32, VCO 614.4 and divider 1 (`DIV_SEL_1`); and
        // the three lowest entries of each reference column are the ones i915
        // programs when it finds no usable CDCLK.
        let entry = entry_for(ReferenceClock::Mhz19_2, 307_200, 32).unwrap();
        assert_eq!(entry.vco_khz(), 614_400);
        assert_eq!(
            cd2x_divider(entry.vco_khz(), entry.cdclk_khz),
            Some(Cd2xDivider::Div1)
        );
        assert_eq!(
            lowest_cdclk(ReferenceClock::Mhz19_2).unwrap().cdclk_khz,
            172_800
        );
        assert_eq!(
            lowest_cdclk(ReferenceClock::Mhz24).unwrap().cdclk_khz,
            176_000
        );
        assert_eq!(
            lowest_cdclk(ReferenceClock::Mhz38_4).unwrap().cdclk_khz,
            179_200
        );
        // The three lowest entries divide by 3, not by 2: that is what makes
        // 172.8/176.0/179.2 special, and it is the case `DIV_SEL_1_5` exists for.
        for reference in REFERENCES {
            let entry = lowest_cdclk(reference).unwrap();
            assert_eq!(
                cd2x_divider(entry.vco_khz(), entry.cdclk_khz),
                Some(Cd2xDivider::Div1_5),
                "{}",
                reference.name()
            );
        }
        // A VCO that is not an exact multiple is refused rather than rounded.
        assert_eq!(cd2x_divider(614_401, 307_200), None);
        assert_eq!(cd2x_divider(0, 307_200), None);
        assert_eq!(
            cd2x_divider(307_200, 307_200),
            None,
            "divisor 1 is not legal"
        );
        assert_eq!(
            cd2x_divider(3_072_000, 307_200),
            None,
            "divisor 10 is not legal"
        );
        assert_eq!(cd2x_divider(2_457_600, 307_200), Some(Cd2xDivider::Div4));
    }

    #[test]
    fn the_decimal_field_matches_the_prm_and_i915_at_the_frequencies_both_state() {
        // §4.6 works two values through both sources and shows they agree:
        // 307.2 MHz -> 612, 172.8 MHz -> 344.  These are the two the document
        // checks numerically, so they are the two asserted here; the rest of
        // the table is checked against the definition.
        assert_eq!(cdclk_decimal(307_200), 612);
        assert_eq!(cdclk_decimal(172_800), 344);
        // Every table row, against `DIV_ROUND_CLOSEST(cdclk - 1000, 500)`.
        for entry in ADL_N_CDCLK_TABLE {
            let expected = (entry.cdclk_khz - 1_000 + 250) / 500;
            assert_eq!(cdclk_decimal(entry.cdclk_khz), expected);
            // U10.1 of `round_to_0.5MHz(f) - 1`: twice the value in half-MHz
            // steps above 1 MHz.  The field is 11 bits, so the largest CDCLK it
            // can state is far above anything in this table.
            assert!(cdclk_decimal(entry.cdclk_khz) < 1 << 11);
        }
        assert_eq!(cdclk_decimal(176_000), 350);
        assert_eq!(cdclk_decimal(179_200), 356);
        assert_eq!(cdclk_decimal(652_800), 1_304);
        assert_eq!(cdclk_decimal(648_000), 1_294);
        assert_eq!(cdclk_decimal(1_000), 0);
    }

    #[test]
    fn the_ctl_value_is_the_divider_the_pipe_and_the_decimal() {
        // `[I915]` TGL_CDCLK_CD2X_PIPE(pipe) = pipe << 20 for display 12+:
        // §4.6 records that the PRM's 000/010/100/110 and this expression agree.
        assert_eq!(cd2x_pipe(0), 0x00);
        assert_eq!(cd2x_pipe(1), 1 << 20);
        assert_eq!(cd2x_pipe(2), 1 << 21);
        assert_eq!(cd2x_pipe(3), 3 << 20);
        assert_eq!(CD2X_PIPE_NONE, 7 << 19);
        // D is `110b` in the field, and the field is bits [21:19], so `110b`
        // and `pipe << 20` are the same bits.  Writing the field out is what
        // lets this line be checked against §4.6's table by eye.
        assert_eq!(cd2x_pipe(3), 0b110 << 19, "D is 110b in the field");
        assert_eq!(0b110 << 19, 3 << 20);
        assert_eq!(cd2x_pipe(0), 0b000 << 19);
        assert_eq!(cd2x_pipe(1), 0b010 << 19);
        assert_eq!(cd2x_pipe(2), 0b100 << 19);
        assert_eq!(CD2X_PIPE_NONE, 0b111 << 19);
        assert_eq!(CD2X_PIPE_NONE & CDCLK_CTL_PIPE_MASK, 0b111 << 19);

        let entry = entry_for(ReferenceClock::Mhz19_2, 307_200, 32).unwrap();
        let value = cdclk_ctl_value(entry, Cd2xDivider::Div1, CD2X_PIPE_NONE);
        assert_eq!(value >> 22 & 0b11, 0, "DIV_SEL_1");
        assert_eq!(value & CDCLK_CTL_PIPE_MASK, 7 << 19);
        assert_eq!(value & CDCLK_CTL_DECIMAL_MASK, 612);
        assert_eq!(value, 0x0038_0264);
    }

    /// `CDCLK_PLL_ENABLE` for a locked PLL at this ratio.
    fn locked_pll(ratio: u32) -> u32 {
        PLL_ENABLE | PLL_LOCK | ratio
    }

    /// `CDCLK_CTL` for a divider and decimal, as firmware would leave it.
    fn ctl(divider: Cd2xDivider, decimal: u32) -> u32 {
        (divider.field() << 22) | CD2X_PIPE_NONE | decimal
    }

    #[test]
    fn a_working_cdclk_left_by_firmware_is_observed_and_kept() {
        // The case the brief calls out: `bxt_cdclk_init_hw` sanitises rather
        // than reprograms, so a firmware CDCLK that is legal must survive
        // bring-up untouched.  This is what the target machine should look like
        // when its firmware has driven the screen.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 1 << 29); // 19.2 MHz
        regs.set(regs::CDCLK_PLL_ENABLE, locked_pll(32)); // 614.4 MHz
        regs.set(regs::CDCLK_CTL, ctl(Cd2xDivider::Div1, 612)); // 307.2 MHz

        let observation = observe(&regs).unwrap();
        assert!(observation.usable());
        assert_eq!(observation.reference, ReferenceClock::Mhz19_2);
        assert_eq!(observation.ratio, 32);
        assert_eq!(observation.vco_khz, 614_400);
        assert_eq!(observation.cdclk_khz, 307_200);
        assert_eq!(observation.divider, Cd2xDivider::Div1);
        assert_eq!(observation.decimal_field, 612);
        assert_eq!(
            observation.entry,
            entry_for(ReferenceClock::Mhz19_2, 307_200, 32)
        );
        // Nothing may have been written at all.
        assert!(
            regs.writes().is_empty(),
            "a usable CDCLK must be left alone"
        );

        // The 38.4 MHz column and the divider-by-1.5 case, which is the shape a
        // machine with a different strap would present.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 2 << 29);
        regs.set(regs::CDCLK_PLL_ENABLE, locked_pll(14));
        regs.set(regs::CDCLK_CTL, ctl(Cd2xDivider::Div1_5, 356));
        let observation = observe(&regs).unwrap();
        assert!(observation.usable());
        assert_eq!(observation.cdclk_khz, 179_200);
        assert_eq!(observation.divider, Cd2xDivider::Div1_5);

        // A working CDCLK whose decimal field is wrong is still kept: the
        // frequency is what the display runs on, and rewriting the field would
        // be a CDCLK change on a running display.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 1 << 29);
        regs.set(regs::CDCLK_PLL_ENABLE, locked_pll(32));
        regs.set(regs::CDCLK_CTL, ctl(Cd2xDivider::Div1, 0));
        let observation = observe(&regs).unwrap();
        assert!(observation.usable());
        assert_eq!(observation.decimal_expected(), Some(612));
        assert!(
            observation.describe().contains("decimal field says"),
            "{}",
            observation.describe()
        );
        assert!(regs.writes().is_empty());
    }

    #[test]
    fn a_cdclk_that_is_not_usable_is_recognised_as_such() {
        // Every way the firmware's CDCLK can be unusable, and the reason it is:
        // the PLL is off, the PLL never locked, or the frequency is not one the
        // platform's table allows.  `[I915]` bxt_sanitize_cdclk reprograms in
        // all three cases, so this driver has to reach its programming path.
        let cases: [(&str, u32, u32, u32); 5] = [
            ("PLL disabled", 1 << 29, 32, ctl(Cd2xDivider::Div1, 612)),
            (
                "PLL enabled but not locked",
                1 << 29,
                PLL_ENABLE | 32,
                ctl(Cd2xDivider::Div1, 612),
            ),
            (
                "ratio not in the table for this reference",
                1 << 29,
                locked_pll(31),
                ctl(Cd2xDivider::Div1, 612),
            ),
            (
                "ratio from another reference column",
                2 << 29,
                locked_pll(32),
                ctl(Cd2xDivider::Div1, 612),
            ),
            (
                "divider 1.5 on a ratio whose VCO does not divide by 3",
                1 << 29,
                locked_pll(32),
                ctl(Cd2xDivider::Div1_5, 612),
            ),
        ];
        for (name, dssm, pll, control) in cases {
            let regs = MockRegisters::new();
            regs.set(regs::SKL_DSSM, dssm);
            regs.set(regs::CDCLK_PLL_ENABLE, pll);
            regs.set(regs::CDCLK_CTL, control);
            let observation = observe(&regs).unwrap();
            assert!(!observation.usable(), "{name} must not be usable");
            assert!(observation.entry.is_none(), "{name}");
            // The last case is the interesting one: the divider field is legal
            // and the PLL is locked, but 614400 / 3 is 204800 kHz, which is not
            // a frequency the platform's table states.  Assert the mechanism so
            // the case cannot silently become a no-op.
            if name.starts_with("divider 1.5") {
                assert_eq!(observation.divider, Cd2xDivider::Div1_5);
                assert_eq!(observation.vco_khz, 614_400);
                assert_eq!(observation.cdclk_khz, 204_800);
            }
        }

        // A PLL that is off reports the bypass clock rather than a stale VCO.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 2 << 29);
        regs.set(regs::CDCLK_PLL_ENABLE, 34);
        regs.set(regs::CDCLK_CTL, ctl(Cd2xDivider::Div1, 1304));
        let observation = observe(&regs).unwrap();
        assert_eq!(observation.vco_khz, 0);
        assert_eq!(observation.cdclk_khz, 19_200, "the bypass clock");
        assert!(!observation.pll_enabled);
        assert!(!observation.usable());
    }

    #[test]
    fn programming_the_cdclk_writes_the_ratio_then_enables_then_locks() {
        // The fallback path, end to end, against a mock whose PLL locks as soon
        // as it is enabled.  The order of the writes is the sequence: ratio
        // with the PLL still off, then ratio with the enable, then the control
        // register.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 1 << 29);
        // LOCK follows PLL_ENABLE, which is what the hardware does and what
        // makes the poll meaningful rather than a single read.
        regs.derive(regs::CDCLK_PLL_ENABLE, |written| {
            if written & PLL_ENABLE == 0 {
                written & !PLL_LOCK
            } else {
                written | PLL_LOCK
            }
        });

        let entry = lowest_cdclk(ReferenceClock::Mhz19_2).unwrap();
        let programmed = program(&regs, entry).unwrap();
        assert_eq!(programmed.entry.cdclk_khz, 172_800);
        assert_eq!(programmed.divider, Cd2xDivider::Div1_5);

        let writes = regs.writes();
        assert_eq!(
            writes,
            alloc::vec![
                ("CDCLK_PLL_ENABLE", 27),
                ("CDCLK_PLL_ENABLE", 27 | PLL_ENABLE),
                (
                    "CDCLK_CTL",
                    cdclk_ctl_value(entry, Cd2xDivider::Div1_5, CD2X_PIPE_NONE)
                ),
            ]
        );
        // And the result is what a second observation would call usable.
        let observation = observe(&regs).unwrap();
        assert!(observation.usable());
        assert_eq!(observation.cdclk_khz, 172_800);
        assert_eq!(observation.reference, ReferenceClock::Mhz19_2);
    }

    #[test]
    fn a_pll_that_never_locks_is_an_error_that_says_what_to_check() {
        // The failure §4.6 describes as the common one -- and names the ADL-N
        // report it bit -- where the ratio does not match the reference
        // frequency.  The PLL is writable but never locks.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 1 << 29);
        // No `derive`: the lock bit never appears.
        let error = program(&regs, lowest_cdclk(ReferenceClock::Mhz19_2).unwrap()).unwrap_err();
        assert!(matches!(error, ClockError::PllNeverLocked { .. }));
        let text = error.describe();
        assert!(text.contains("never reported lock"), "{text}");
        assert!(text.contains("re-read SKL_DSSM"), "{text}");
        // The control register was not written: the sequence stops where it
        // failed rather than programming a divider on top of an unlocked PLL.
        assert_eq!(
            regs.writes(),
            alloc::vec![
                ("CDCLK_PLL_ENABLE", 27),
                ("CDCLK_PLL_ENABLE", 27 | PLL_ENABLE),
            ]
        );
    }

    #[test]
    fn a_control_register_that_does_not_read_back_is_an_error() {
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 1 << 29);
        regs.derive(regs::CDCLK_PLL_ENABLE, |written| written | PLL_LOCK);
        // The device drops the divider field, as it would if the write never
        // arrived.  A silent mismatch here is a display running at the wrong
        // frequency, so it must not pass.
        regs.derive(regs::CDCLK_CTL, |_| ctl(Cd2xDivider::Div1, 612));
        let error = program(&regs, lowest_cdclk(ReferenceClock::Mhz19_2).unwrap()).unwrap_err();
        assert!(
            matches!(error, ClockError::ReadbackMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_raw_clock_is_whichever_the_strap_selects() {
        // §4.8's two encodings, which are the two `cnp_rawclk` can produce.
        let fast = raw_clock_plan(1 << 8);
        assert!(fast.is_24_mhz);
        assert_eq!(fast.khz, 24_000);
        assert_eq!(fast.register, 0x0018_0000);
        let slow = raw_clock_plan(0);
        assert!(!slow.is_24_mhz);
        assert_eq!(slow.khz, 19_200);
        assert_eq!(slow.register, 0x1013_0800);
        // Only bit 8 matters, whatever else the strap carries.
        assert_eq!(raw_clock_plan(0xFFFF_FFFF).khz, 24_000);
        assert_eq!(raw_clock_plan(0xFFFF_FEFF).khz, 19_200);

        // Both encodings decode back to the frequency they state.
        assert_eq!(
            decode_raw_clock(fast.register),
            RawClockProgrammed::Known(24_000)
        );
        assert_eq!(
            decode_raw_clock(slow.register),
            RawClockProgrammed::Known(19_200)
        );
        // Nothing else does, and an unprogrammed register is not 0 Hz.
        assert_eq!(decode_raw_clock(0), RawClockProgrammed::Unrecognised(0));
        assert!(matches!(
            decode_raw_clock((37 << 16) | (4 << 26) | (2 << 11)),
            RawClockProgrammed::Unrecognised(_)
        ));
    }

    #[test]
    fn the_raw_clock_is_programmed_from_the_strap_and_not_overwritten_otherwise() {
        // A firmware that already programmed 19.2 MHz on a 19.2 MHz strap: the
        // register is left alone.
        let regs = MockRegisters::new();
        regs.set(regs::PCH_RAWCLK_FREQ, 0x1013_0800);
        let state = bring_up_raw_clock(&regs, 0).unwrap();
        assert!(!state.programmed);
        assert_eq!(state.was, RawClockProgrammed::Known(19_200));
        assert!(regs.writes().is_empty());

        // A firmware that programmed the wrong value, and a strap that says
        // 24 MHz: the strap wins and the disagreement is recorded.
        let regs = MockRegisters::new();
        regs.set(regs::PCH_RAWCLK_FREQ, 0x1013_0800);
        let state = bring_up_raw_clock(&regs, 1 << 8).unwrap();
        assert!(state.programmed);
        assert_eq!(state.was, RawClockProgrammed::Known(19_200));
        assert_eq!(regs.read(regs::PCH_RAWCLK_FREQ), Some(0x0018_0000));
        let text = state.describe();
        assert!(text.contains("DISAGREE"), "{text}");

        // An unprogrammed register on a 24 MHz strap is programmed too.
        let regs = MockRegisters::new();
        let state = bring_up_raw_clock(&regs, 1 << 8).unwrap();
        assert!(state.programmed);
        assert_eq!(state.was, RawClockProgrammed::Unrecognised(0));
        assert_eq!(regs.read(regs::PCH_RAWCLK_FREQ), Some(0x0018_0000));
    }

    #[test]
    fn a_register_outside_the_window_is_an_error_rather_than_a_zero() {
        // The whole point of the window check: a register that cannot be read
        // must not be mistaken for a register that reads zero.
        let regs = MockRegisters::new();
        regs.hide(regs::SKL_DSSM);
        let error = observe(&regs).unwrap_err();
        assert_eq!(
            error,
            ClockError::Unreadable {
                register: "SKL_DSSM"
            }
        );
        assert!(
            error
                .describe()
                .contains("outside the mapped register window")
        );

        let regs = MockRegisters::new();
        regs.set(regs::PCH_RAWCLK_FREQ, 0);
        regs.refuse(regs::PCH_RAWCLK_FREQ);
        let error = bring_up_raw_clock(&regs, 1 << 8).unwrap_err();
        assert_eq!(
            error,
            ClockError::WriteRefused {
                register: "PCH_RAWCLK_FREQ"
            }
        );
    }

    #[test]
    fn a_usable_cdclk_is_kept_and_an_unusable_one_is_replaced() {
        // The two halves of the decision, through the entry point.  A machine
        // whose firmware drove the screen must come out of this with its CDCLK
        // untouched; one whose firmware left nothing usable must come out with
        // a frequency the table states.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 2 << 29); // 38.4 MHz
        regs.set(regs::CDCLK_PLL_ENABLE, locked_pll(29)); // 1113.6 MHz
        regs.set(regs::CDCLK_CTL, ctl(Cd2xDivider::Div1, 1112)); // 556.8 MHz
        let state = bring_up(&regs).unwrap();
        assert!(state.kept_firmware);
        assert!(state.programmed.is_none());
        assert!(state.after.is_none());
        assert!(
            regs.writes().is_empty(),
            "a usable CDCLK must not be touched"
        );
        assert!(state.describe().contains("kept"));

        // A PLL that is off: the lowest value for the reference is programmed,
        // and the re-read confirms it.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 1 << 29); // 19.2 MHz
        regs.derive(regs::CDCLK_PLL_ENABLE, |written| {
            if written & PLL_ENABLE != 0 {
                written | PLL_LOCK
            } else {
                written & !PLL_LOCK
            }
        });
        let state = bring_up(&regs).unwrap();
        assert!(!state.kept_firmware);
        assert_eq!(state.programmed.unwrap().entry.cdclk_khz, 172_800);
        let after = state.after.unwrap();
        assert!(after.usable());
        assert_eq!(after.cdclk_khz, 172_800);
        assert_eq!(after.divider, Cd2xDivider::Div1_5);
        assert!(state.describe().contains("172800 kHz"));
        assert!(state.describe().contains("re-read"));

        // A device that locks the PLL but drops the ratio: the control register
        // takes what it is given, so `program` is satisfied, and only the
        // re-read catches that the display is not running the frequency that
        // was asked for.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 1 << 29);
        regs.derive(regs::CDCLK_PLL_ENABLE, |written| {
            (written & !0xFF) | PLL_ENABLE | PLL_LOCK
        });
        let error = bring_up(&regs).unwrap_err();
        assert!(
            matches!(error, ClockError::StillNotUsable { .. }),
            "{error:?}"
        );
        assert!(
            error
                .describe()
                .contains("not a combination the ADL-N table states")
        );

        // A device that accepts the PLL writes but not the divider write is
        // caught earlier, by the read-back inside `program`: two different
        // faults, reported differently.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 1 << 29);
        regs.derive(regs::CDCLK_PLL_ENABLE, |written| written | PLL_LOCK);
        regs.derive(regs::CDCLK_CTL, |_| ctl(Cd2xDivider::Div1, 612));
        let error = bring_up(&regs).unwrap_err();
        assert!(
            matches!(error, ClockError::ReadbackMismatch { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn an_undefined_reference_encoding_is_reported_rather_than_hidden() {
        // Three encodings of `SKL_DSSM[31:29]` mean nothing in any source read
        // for this driver.  i915 falls through to 24 MHz; this driver does the
        // same, because a caller has to have some frequency to work with, but
        // it says so -- and the flag is what keeps the fallback from being
        // presented as a reading.
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 3 << 29); // not one of the three defined values
        regs.set(regs::CDCLK_PLL_ENABLE, locked_pll(22));
        regs.set(regs::CDCLK_CTL, ctl(Cd2xDivider::Div1_5, 350));
        let observation = observe(&regs).unwrap();
        assert!(!observation.reference_recognised);
        assert_eq!(observation.reference, ReferenceClock::Mhz24);
        // 24 MHz with ratio 22 is 528 MHz, over 3 is 176 MHz -- which *is* the
        // 24 MHz column's lowest row.  So the fallback frequency makes this
        // CDCLK usable, and the honest answer is to keep it while saying that
        // the encoding it was derived from is not one the documentation
        // defines.  Refusing a working display because a reserved encoding was
        // in use would be the wrong call; hiding the fact would be worse.
        assert!(observation.usable());
        assert_eq!(observation.cdclk_khz, 176_000);
        let text = observation.describe();
        assert!(text.contains("NOT a documented encoding"), "{text}");

        // The fallback is not a licence: a CDCLK that does not match the 24 MHz
        // column is still refused, so the flag cannot be read as "anything
        // goes when the encoding is unknown".
        let regs = MockRegisters::new();
        regs.set(regs::SKL_DSSM, 3 << 29);
        regs.set(regs::CDCLK_PLL_ENABLE, locked_pll(32));
        regs.set(regs::CDCLK_CTL, ctl(Cd2xDivider::Div1, 612));
        let observation = observe(&regs).unwrap();
        assert!(!observation.reference_recognised);
        assert_eq!(observation.vco_khz, 768_000);
        assert_eq!(observation.cdclk_khz, 384_000, "divider 1 halves the VCO");
        assert!(!observation.usable());
        assert!(observation.entry.is_none());
    }
}
