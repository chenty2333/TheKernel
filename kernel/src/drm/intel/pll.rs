//! The DDI PLL divider arithmetic for a Gen12 combo PHY.
//!
//! An Intel display pipe is clocked by a shared "combo PHY" PLL -- a WRPLL in
//! Intel's older vocabulary -- whose output is a small integer multiple of the
//! pixel clock, or, for HDMI, five times the TMDS character rate.  Getting the
//! dividers right is what makes a mode appear at the right size and in the
//! right place; getting them wrong produces a monitor that reports no signal
//! and gives no reason.
//!
//! Everything here is integer arithmetic over the numbers in the request and
//! the numbers in the result.  There is no MMIO, no register window and no
//! device: a caller hands in a pixel clock, a reference frequency and which
//! combo PHY it means, and gets back the divider set plus the two `CFGCR`
//! register values it encodes to.  That is deliberate.  This is the half of a
//! modeset that can be tested on a machine that has no display, so it is kept
//! where a test can reach it.
//!
//! # Where the facts come from
//!
//! `docs/design/intel-display-registers.md` §6.3 is the reference.  It sources
//! the architecture, the register fields and the DCO bounds from Intel's Tiger
//! Lake PRM volumes, and it flags two things it could not settle:
//!
//! * the `PDIV`/`KDIV` field encoding -- `[GAP]`, §6.3 "An unresolved
//!   discrepancy in the PDIV encoding"; and
//! * the exact search loop, tolerance and tie-breaking -- `[GAP]`, §13.1
//!   item 11 ("I did not extract the exact loop, tolerance and tie-breaking
//!   rules from `skl_ddi_calculate_wrpll`").
//!
//! Both are settled here from the only source in the reference's set that
//! targets **display version 13**, which is what an Alder Lake-N part is:
//! Linux v6.12 `drm/i915` (tag `v6.12`, commit
//! `adc218676eef25575469234709c2d87185ca223a`), file
//! `drivers/gpu/drm/i915/display/intel_dpll_mgr.c`, with register definitions
//! in `drivers/gpu/drm/i915/i915_reg.h`.  Facts are taken from those files --
//! symbols and line numbers are cited throughout -- and no code is copied.
//!
//! Settling a `[GAP]` is not the same as closing it, and this module is built
//! so that it cannot pretend otherwise.  Where i915's *executed* path and its
//! own *named constants* disagree, both are offered as named encodings
//! ([`PllFieldEncoding`]), there is no default, and the disputed cases return
//! a named error rather than a guess.  See "the encoding gap" below.
//!
//! # The shape of the arithmetic
//!
//! The PLL is a DCO: it runs at some frequency in a documented band and is
//! divided down to the rate the port needs.  For a combo PHY on this
//! generation,
//!
//! ```text
//!     afe_clock = 5 * symbol_rate
//!     DCO       = afe_clock * P * Q * K
//! ```
//!
//! and the search is over the integer `P * Q * K`, aiming the DCO at one of
//! three central frequencies.  The doc's own worked example is 1080p60 over
//! HDMI: the TMDS character rate is 148.5 MHz, so `afe_clock` is 742.5 MHz and
//! a total divider of 12 puts the DCO at 8910 MHz, between the 8400 and 9000
//! MHz centres.
//!
//! # The encoding gap
//!
//! `CFGCR1` holds `P` and `K` as small codes, and there are two mutually
//! inconsistent accounts of what those codes are:
//!
//! * [`PllFieldEncoding::Executed`] -- what i915 actually writes.  Its values
//!   come from `skl_wrpll_params_populate` (`intel_dpll_mgr.c:1592-1658`),
//!   which encodes `P` over the candidate set `{1,2,3,7}` and `K` over
//!   `{5,2,3,1}`.
//! * [`PllFieldEncoding::Named`] -- what i915's constants say.  `i915_reg.h`
//!   defines `DPLL_CFGCR1_PDIV_{2,3,5,7}` as `{1,2,4,8} << 2`
//!   (`i915_reg.h:4293-4296`) and `DPLL_CFGCR1_KDIV_{1,2,3}` as
//!   `{1,2,4} << 6` (`i915_reg.h:4287-4289`), matching the PRM's `P ∈
//!   {2,3,5,7}`, `K ∈ {1,2,3}`.
//!
//! The two disagree on **every** `K` and on `P = 7`.  The reason is now clear
//! and is worth writing down, because the reference document could only
//! observe the symptom: `skl_wrpll_params_populate`'s candidate sets and codes
//! are exactly the *Skylake* `DPLL_CFGCR2` convention -- compare
//! `DPLL_CFGCR2_KDIV_{5,2,3,1} = {0,1,2,3} << 5` and
//! `DPLL_CFGCR2_PDIV_{1,2,3,7} = {0,1,2,4} << 2` at `i915_reg.h:4141-4150` --
//! and `icl_calc_dpll_state` (`intel_dpll_mgr.c:2884-2909`) shifts those
//! Skylake-convention values straight into the Gen12 `CFGCR1` positions.
//! i915 then reads them back with the Gen12 named-constant convention in
//! `icl_ddi_combo_pll_get_freq` (`intel_dpll_mgr.c:1740-1802`), so its own
//! write and read disagree.  Both conventions are still present in mainline.
//!
//! This module therefore refuses to choose.  `P = 7` and `P = 5` are the cases
//! where the two encodings differ in *which* `P` they can express at all, and
//! they return [`PllError::DividerNotEncodable`] under the encoding that has no
//! code for them, rather than emitting a value nobody can justify.
//!
//! # What has not been checked
//!
//! No value from this module has been written to, or read from, real silicon.
//! The arithmetic is checked against i915 v6.12's own, and the published
//! modelines below are checked against the numbers the timing tables carry, but
//! that is a host test against a published reference, not a measurement.  The
//! reference document's §13.4 recommendation stands: read `DPLL0_CFGCR0` and
//! `DPLL0_CFGCR1` from a firmware-programmed working mode and diff them against
//! what this module computes for the same mode before trusting either.

use core::fmt;

/// The DCO central frequencies the divider search aims at, in kHz.
///
/// `[I915]` `skl_ddi_calculate_wrpll` (`intel_dpll_mgr.c:1665-1667`) holds
/// `{8400, 9000, 9600} MHz` as `dco_central_freq`.  The reference document
/// (§6.3, "The search bounds") separately quotes the PRM's *window*
/// `[7998 MHz, 10000 MHz]` with a 8999 MHz midpoint; the two framings are
/// compatible in spirit but not identical, and this module implements i915's
/// because i915 is the only source in the set that targets display 13.  Both
/// are available, so the difference can be seen rather than assumed: see
/// [`PRM_DCO_MIN_KHZ`], [`PRM_DCO_MAX_KHZ`] and the
/// `the_two_dco_framings_really_do_differ` test.
/// The search is done in Hz throughout.  Mixing it with the kHz the rest of
/// the module reports in is how a deviation comes out a thousand times too
/// large and every candidate is rejected, so the unit is in the name.
const DCO_CENTRAL_FREQ_HZ: [u64; 3] = [8_400_000_000, 9_000_000_000, 9_600_000_000];

/// The PRM's DCO window, in kHz, for cross-checking a candidate divider.
///
/// `docs/design/intel-display-registers.md` §6.3: "DCO ∈ [7998 MHz, 10000 MHz],
/// midpoint 8999 MHz", quoted from `[TGL12]` Clocks → combo PHY PLL.  This is
/// *not* what the search uses -- [`DCO_CENTRAL_FREQ_KHZ`] and the deviation
/// limits are -- but a candidate outside this window is worth reporting,
/// because the PRM is the only source that states a hard bound.
pub(crate) const PRM_DCO_MIN_KHZ: u64 = 7_998_000;

/// The upper end of the PRM's DCO window.  See [`PRM_DCO_MIN_KHZ`].
pub(crate) const PRM_DCO_MAX_KHZ: u64 = 10_000_000;

/// Total dividers i915 tries on the even path, in the order it tries them.
///
/// `[I915]` `skl_ddi_calculate_wrpll` (`intel_dpll_mgr.c:1668-1672`)
/// `even_dividers[]`.  The list is not every even number: it is exactly the
/// even numbers whose half decomposes under
/// `skl_wrpll_get_multipliers`, so every entry has a `(P, Q, K)`.  That is
/// checked by the `every_candidate_divider_decomposes` test rather than assumed.
const EVEN_TOTAL_DIVIDERS: &[u32] = &[
    4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 52, 54, 56, 60, 64, 66,
    68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98,
];

/// Total dividers i915 tries on the odd path.
///
/// `[I915]` `skl_ddi_calculate_wrpll` (`intel_dpll_mgr.c:1673`)
/// `odd_dividers[]`.
const ODD_TOTAL_DIVIDERS: &[u32] = &[3, 5, 7, 9, 15, 21, 35];

/// The most a candidate DCO may sit **above** the central frequency it aims at,
/// in units of 0.01%.
///
/// `[I915]` `SKL_DCO_MAX_PDEVIATION` (`intel_dpll_mgr.c:1501`), with the
/// comment "DCO freq must be within +1%/-6% of the DCO central freq"
/// (`intel_dpll_mgr.c:1500`).  The deviation is computed in hundredths of a
/// percent, so 100 is 1.00%.
const DCO_MAX_POSITIVE_DEVIATION: u64 = 100;

/// The most a candidate DCO may sit **below** the central frequency it aims at,
/// in units of 0.01%.  See [`DCO_MAX_POSITIVE_DEVIATION`].  `[I915]`
/// `SKL_DCO_MAX_NDEVIATION` (`intel_dpll_mgr.c:1502`).
const DCO_MAX_NEGATIVE_DEVIATION: u64 = 600;

/// How far the achieved symbol rate may sit from the requested one, in parts
/// per billion, before the search refuses its own answer.
///
/// The divider set does not introduce any error at all: the DCO is
/// `afe_clock * P * Q * K` by construction, so dividing it back by `5 * P * Q
/// * K` returns the requested symbol rate exactly.  The only error is the
/// quantisation of `DCO_FRACTION`, which has 15 bits and therefore steps by
/// `ref / 0x8000`: 24 MHz / 32768 = 732 Hz at the largest reference this part
/// uses, against a DCO of at least 7896 MHz, which is 93 parts per billion.
/// 1000 ppb (1 ppm) is ten times that bound, so a solution that trips it is a
/// bug in this module rather than a hardware limit -- which is the point of
/// checking.
const MAX_SYMBOL_RATE_ERROR_PPB: u64 = 1000;

/// The `SKL_DSSM` register's reference-clock field, as a mask.
///
/// `[I915]` `i915_reg.h:2881-2884`: `ICL_DSSM_CDCLK_PLL_REFCLK_MASK = 7 << 29`,
/// with 24 MHz at `0 << 29`, 19.2 MHz at `1 << 29` and 38.4 MHz at `2 << 29`.
/// The register itself is `SKL_DSSM` at offset `0x51004` (`i915_reg.h:2880`).
/// Reading it is the *only* way to learn this part's reference; the reference
/// document §4.6 records that the DG1 PRM's "38.4 MHz, not programmable" is a
/// DG1 special case and §13.1 item 4 lists the question as a `[GAP]`.
pub(crate) const DSSM_REFCLK_MASK: u32 = 7 << 29;

/// The three defined values of [`DSSM_REFCLK_MASK`], as named constants.
///
/// They are `const` rather than inline shifts because a shift is an expression
/// and a `match` arm needs a pattern.  `[I915]` `i915_reg.h:2882-2884` names
/// them `ICL_DSSM_CDCLK_PLL_REFCLK_24MHz`, `..._19_2MHz` and `..._38_4MHz`.
pub(crate) const DSSM_REFCLK_24MHZ: u32 = 0 << 29;
pub(crate) const DSSM_REFCLK_19_2MHZ: u32 = 1 << 29;
pub(crate) const DSSM_REFCLK_38_4MHZ: u32 = 2 << 29;

/// Which combo PHY, and therefore which shared DPLL, a port is on.
///
/// `[I915]` `adlp_plls[]` (`intel_dpll_mgr.c:4273-4283`): DPLL 0 and DPLL 1 are
/// the two `combo_pll_funcs` entries, and the reference document §6.3 records
/// that combo PHY A is the one on DPLL 0 and combo PHY B on DPLL 1.  ADL-N's
/// rear HDMI is on a combo PHY (§8.1), which is why these two are the ones this
/// module models; the TBT and DKL PLLs are for Type-C and are deliberately not
/// represented, because a caller cannot ask this module for one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ComboPhy {
    A,
    B,
}

impl ComboPhy {
    /// The shared-DPLL index this PHY is wired to.
    pub(crate) const fn dpll_index(self) -> u32 {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
    }

    /// The bit position of this PHY's `DDI_CLK_SEL` field in
    /// `ICL_DPCLKA_CFGCR0`.
    ///
    /// `[I915]` `ICL_DPCLKA_CFGCR0_DDI_CLK_SEL_SHIFT(phy) = phy * 2`
    /// (`i915_reg.h:4162`), a two-bit field whose value is the PLL id.
    pub(crate) const fn ddi_clock_select_shift(self) -> u32 {
        self.dpll_index() * 2
    }

    /// The `DDI_CLK_SEL` field value that routes this PHY to its DPLL.
    ///
    /// `[I915]` `ICL_DPCLKA_CFGCR0_DDI_CLK_SEL(pll, phy) = pll << (phy * 2)`
    /// (`i915_reg.h:4164`).  The caller writes this into
    /// `ICL_DPCLKA_CFGCR0` at offset `0x164280` (`i915_reg.h:4158`), and clears
    /// the matching `DDI_CLK_OFF` bit in a **separate** write -- the reference
    /// document §6.3 quotes the spec's requirement that the two "must be done
    /// with separate register writes".
    pub(crate) const fn ddi_clock_select(self) -> u32 {
        self.dpll_index() << self.ddi_clock_select_shift()
    }

    /// The bit position of this PHY's `DDI_CLK_OFF` field in
    /// `ICL_DPCLKA_CFGCR0`.
    ///
    /// `[I915]` `ICL_DPCLKA_CFGCR0_DDI_CLK_OFF(phy) = 1 << _PICK(phy, 10, 11,
    /// 24, 4, 5)` (`i915_reg.h:4159`), which is bit 10 for PHY A and bit 11 for
    /// PHY B.  The encoding is the pre-Rocket-Lake one, which is what the
    /// reference document §6.3 says this generation uses.
    pub(crate) const fn ddi_clock_off_bit(self) -> u32 {
        match self {
            Self::A => 1 << 10,
            Self::B => 1 << 11,
        }
    }
}

/// Which of the two mutually inconsistent `CFGCR1` field encodings to use.
///
/// There is deliberately no `Default`.  Choosing one silently is the failure
/// this type exists to prevent; see the module documentation's "the encoding
/// gap", and `docs/design/intel-pll.md` for the full argument.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PllFieldEncoding {
    /// The codes i915's `skl_wrpll_params_populate` actually writes
    /// (`intel_dpll_mgr.c:1611-1643`): `P` over `{1,2,3,7}` and `K` over
    /// `{5,2,3,1}`.  These are the Skylake `DPLL_CFGCR2` codes, reused for
    /// Gen12 without translation.
    Executed,
    /// The codes i915's named constants define (`i915_reg.h:4287-4296`): `P`
    /// over `{2,3,5,7}` and `K` over `{1,2,3}`, which are the value sets the
    /// PRM states.  This is also the convention i915's own read-back path,
    /// `icl_ddi_combo_pll_get_freq`, decodes with.
    Named,
}

impl fmt::Display for PllFieldEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Executed => "i915 executed (Skylake CFGCR2 code values)",
            Self::Named => "i915 named constants (Gen12/PRM code values)",
        })
    }
}

/// Whether to program half the computed `DCO_FRACTION`.
///
/// `[I915]` `ehl_combo_pll_div_frac_wa_needed` (`intel_dpll_mgr.c:2598-2605`)
/// returns true for TGL, ADL-S, ADL-P and Elkhart Lake B0 onwards **when the
/// strapped reference is 38.4 MHz**, with the comment "Program half of the
/// nominal DCO divider fraction value" (`intel_dpll_mgr.c:2596`).  ADL-N is an
/// ADL-P subplatform, so this applies to the target machine whenever its
/// reference is 38.4 MHz.
///
/// This is a fact the reference document does not record: §6.3 documents
/// `DPLL_CFGCR1`'s fields and the 38.4 → 19.2 reference division but not the
/// fraction workaround that goes with it.  It is modelled explicitly rather
/// than assumed, because whether it applies depends on the platform, and this
/// module is given no platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DcoFractionWorkaround {
    /// Program the computed fraction unchanged.
    NotNeeded,
    /// Program half the computed fraction, rounded to nearest.
    HalveFraction,
}

impl DcoFractionWorkaround {
    /// The workaround as it applies to an ADL-P/N part at this reference.
    ///
    /// The caller supplies the platform because the search does not know it.
    /// The condition is exactly i915's: the workaround is needed when the
    /// reference is 38.4 MHz.  (i915 also lists TGL, ADL-S and EHL B0+; a
    /// caller on one of those parts wants the same answer, and a caller on a
    /// part that is none of them still has the same 38.4 MHz question to
    /// answer, so the parameter is the reference and the name says which
    /// family this was established for.)
    pub(crate) const fn for_adl_p_n(platform_ref_khz: u32) -> Self {
        if platform_ref_khz == 38_400 {
            Self::HalveFraction
        } else {
            Self::NotNeeded
        }
    }
}

/// Why a divider set could not be produced or could not be encoded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PllError {
    /// The requested pixel or symbol rate was zero.
    ZeroSymbolRate,
    /// The reference frequency was zero.
    ZeroReference,
    /// The reference frequency is not one a Gen12 combo PHY PLL can be
    /// strapped to.
    ///
    /// `[I915]` reads the reference from `SKL_DSSM` and decodes exactly three
    /// values -- 24, 19.2 and 38.4 MHz (`i915_reg.h:2881-2884`) -- and the
    /// reference document §14 records the disagreement about which of them
    /// ADL-N uses without resolving it.  A frequency outside that set is not
    /// something this module can do arithmetic for, so it says so instead of
    /// trying.
    UnsupportedReference { ref_khz: u32 },
    /// No divider in the sourced candidate lists put the DCO within the
    /// documented tolerance of any central frequency.
    ///
    /// This is the honest answer for a pixel clock the PLL cannot make, and it
    /// is deliberately an error rather than the nearest divider: a silently
    /// wrong pixel clock is a monitor that shows nothing.
    NoLegalDividerSet {
        symbol_rate_khz: u32,
        ref_khz: u32,
    },
    /// A divider i915's lists contain could not be decomposed into `(P, Q, K)`.
    ///
    /// `skl_wrpll_get_multipliers` leaves its outputs untouched for a divider
    /// it does not recognise, which the caller zero-initialised; i915 then
    /// warns and programs a zero.  Every entry of the two lists does decompose
    /// (the `every_candidate_divider_decomposes` test checks exactly that), so
    /// this is a guard against a list being edited, not a reachable state.
    DividerNotDecomposable { total_divider: u32 },
    /// The chosen divider set has no code in the requested field encoding.
    ///
    /// This is the encoding `[GAP]` of §6.3 surfaced as a value rather than
    /// hidden: `P = 5` has no `Executed` code (i915 warns "Incorrect PDiv" and
    /// programs whatever `pdiv` was left as), and `K = 5` has no `Named` code
    /// (and is not a legal `K` in the PRM's `K ∈ {1,2,3}` at all).
    DividerNotEncodable {
        /// The post divider `P` that could not be coded.
        p: u32,
        /// The `K` divider under which the failure occurred.
        k: u32,
        /// Whether it was `P` or `K` that had no code.
        field: PllDividerField,
        encoding: PllFieldEncoding,
    },
    /// A field value did not fit the register field it has to go in.
    FieldOverflow {
        field: PllDividerField,
        value: u32,
        bits: u32,
    },
    /// The divider set was found, but the DCO the registers actually produce is
    /// further from the requested rate than this module's tolerance allows.
    AchievedRateOutOfTolerance { error_ppb: i64, limit_ppb: u64 },
    /// The `SKL_DSSM` reference field held a value no source defines.
    UnsupportedDssmReference { field: u32 },
}

/// Which divider a [`PllError`] is about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PllDividerField {
    /// The post divider `P`, `CFGCR1[5:2]`.
    Post,
    /// The qdiv ratio `Q`, `CFGCR1[17:10]`.
    QdivRatio,
    /// The `K` divider, `CFGCR1[8:6]`.
    K,
    /// The DCO integer, `CFGCR0[9:0]`.
    DcoInteger,
    /// The DCO fraction, `CFGCR0[24:10]`.
    DcoFraction,
}

impl fmt::Display for PllError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSymbolRate => f.write_str("the requested symbol rate is zero"),
            Self::ZeroReference => f.write_str("the reference frequency is zero"),
            Self::UnsupportedReference { ref_khz } => write!(
                f,
                "a reference of {ref_khz} kHz is not one a Gen12 combo PHY PLL is strapped to \
                 (24 MHz, 19.2 MHz and 38.4 MHz are the ones i915 decodes from SKL_DSSM)"
            ),
            Self::NoLegalDividerSet {
                symbol_rate_khz,
                ref_khz,
            } => write!(
                f,
                "no divider in the sourced candidate lists puts the DCO within tolerance of a \
                 central frequency for a {symbol_rate_khz} kHz symbol rate at a {ref_khz} kHz \
                 reference; the pixel clock is outside what this PLL can make"
            ),
            Self::DividerNotDecomposable { total_divider } => write!(
                f,
                "total divider {total_divider} has no (P, Q, K) decomposition"
            ),
            Self::DividerNotEncodable {
                p,
                k,
                field,
                encoding,
            } => write!(
                f,
                "the divider set (P={p}, K={k}) has no code for {field:?} under the {encoding} \
                 encoding; this is the PDIV/KDIV gap the register reference flags in §6.3 and it \
                 must be settled by hardware read-back, not guessed"
            ),
            Self::FieldOverflow { field, value, bits } => write!(
                f,
                "{field:?} value {value} does not fit its {bits}-bit register field"
            ),
            Self::AchievedRateOutOfTolerance { error_ppb, limit_ppb } => write!(
                f,
                "the registers would produce a symbol rate {error_ppb} ppb from the request, \
                 beyond this module's {limit_ppb} ppb tolerance"
            ),
            Self::UnsupportedDssmReference { field } => write!(
                f,
                "SKL_DSSM's reference-clock field is {field}, which no source defines (i915 \
                 decodes 0, 1 and 2 only)"
            ),
        }
    }
}

/// The divider set a DCO needs, and everything the numbers behind it.
///
/// Every field is a fact about the arithmetic rather than about a register, so
/// a caller can log the whole decision -- which is what the reference document
/// §11 phase 3.3 asks for ("Compute the PLL dividers for the pixel clock and
/// log them before writing").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DdiPllDividers {
    phy: ComboPhy,
    platform_ref_khz: u32,
    wrpll_ref_khz: u32,
    symbol_rate_khz: u32,
    total_divider: u32,
    p: u32,
    q: u32,
    k: u32,
    central_freq_khz: u64,
    target_dco_khz: u64,
    deviation_centipercent: u64,
    dco_integer: u32,
    dco_fraction: u32,
    achieved_dco_hz: u64,
    achieved_symbol_rate_hz: u64,
    rate_error_ppb: i64,
}

impl DdiPllDividers {
    /// The PHY this set was computed for.
    pub(crate) const fn phy(&self) -> ComboPhy {
        self.phy
    }

    /// The reference frequency as the platform straps it, in kHz.
    pub(crate) const fn platform_ref_khz(&self) -> u32 {
        self.platform_ref_khz
    }

    /// The reference frequency the DCO arithmetic actually divides by, in kHz.
    ///
    /// This is not always [`Self::platform_ref_khz`]: `[I915]`
    /// `icl_wrpll_ref_clock` (`intel_dpll_mgr.c:1737-1752`) maps 38.4 MHz to
    /// 19.2 MHz with the comment "For ICL+, the spec states: if reference
    /// frequency is 38.4, use 19.2 because the DPLL automatically divides that
    /// by 2."  A caller that programs `DCO_INTEGER` from the platform reference
    /// instead of from this one gets a pixel clock that is wrong by a factor of
    /// two.
    pub(crate) const fn wrpll_ref_khz(&self) -> u32 {
        self.wrpll_ref_khz
    }

    /// The symbol rate these dividers produce, in kHz.
    pub(crate) const fn symbol_rate_khz(&self) -> u32 {
        self.symbol_rate_khz
    }

    /// `P * Q * K`, the total division from DCO to `afe_clock`.
    pub(crate) const fn total_divider(&self) -> u32 {
        self.total_divider
    }

    /// The post divider `P`.
    pub(crate) const fn p(&self) -> u32 {
        self.p
    }

    /// The qdiv ratio `Q`.
    pub(crate) const fn q(&self) -> u32 {
        self.q
    }

    /// The `K` divider.
    pub(crate) const fn k(&self) -> u32 {
        self.k
    }

    /// The central frequency the chosen DCO was aimed at, in kHz.
    pub(crate) const fn central_freq_khz(&self) -> u64 {
        self.central_freq_khz
    }

    /// The DCO the divider set asks for, in kHz: `5 * symbol_rate * P * Q * K`.
    pub(crate) const fn target_dco_khz(&self) -> u64 {
        self.target_dco_khz
    }

    /// How far the chosen DCO sits from its central frequency, in 0.01%.
    pub(crate) const fn deviation_centipercent(&self) -> u64 {
        self.deviation_centipercent
    }

    /// The value for `CFGCR0`'s `DCO_INTEGER` field, `[9:0]`.
    pub(crate) const fn dco_integer(&self) -> u32 {
        self.dco_integer
    }

    /// The value for `CFGCR0`'s `DCO_FRACTION` field, `[24:10]`, before any
    /// workaround is applied.
    pub(crate) const fn dco_fraction(&self) -> u32 {
        self.dco_fraction
    }

    /// The DCO the registers will actually produce, in Hz.
    ///
    /// Computed at Hz rather than kHz because the fraction is resolved finely:
    /// the quantum is `ref / 0x8000`, which is 732 Hz at a 24 MHz reference.
    /// Reporting in kHz would quantise away the very error this figure exists
    /// to show.
    pub(crate) const fn achieved_dco_hz(&self) -> u64 {
        self.achieved_dco_hz
    }

    /// The symbol rate the registers will actually produce, in Hz.
    pub(crate) const fn achieved_symbol_rate_hz(&self) -> u64 {
        self.achieved_symbol_rate_hz
    }

    /// How far the achieved symbol rate sits from the requested one, in parts
    /// per billion.  Negative means the registers produce a slower clock.
    pub(crate) const fn rate_error_ppb(&self) -> i64 {
        self.rate_error_ppb
    }

    /// Whether the chosen DCO is inside the PRM's `[7998, 10000] MHz` window.
    ///
    /// i915's deviation rule and the PRM's window are not the same test, and a
    /// candidate can satisfy one and not the other; see
    /// [`PRM_DCO_MIN_KHZ`].  This is reported rather than enforced, because the
    /// search follows i915.
    pub(crate) const fn inside_prm_dco_window(&self) -> bool {
        self.target_dco_khz >= PRM_DCO_MIN_KHZ && self.target_dco_khz <= PRM_DCO_MAX_KHZ
    }

    /// The `(P, Q, K)` that the PRM's own bounds allow for this total divider,
    /// if any.
    ///
    /// The PRM states `P ∈ {2,3,5,7}`, `K ∈ {1,2,3}`, `Q ∈ 1..255` and
    /// "`K != 2 ⇒ Q = 1`" (reference document §6.3, "The search bounds").  The
    /// divider i915 picks does **not** always satisfy them: its decomposition
    /// can produce `K = 5`, which is not a legal `K` at all, and `total = 35`
    /// has no legal decomposition whatsoever.
    ///
    /// This is a *report*, not a substitution.  Returning a different triple
    /// here and programming it would replace the only display-13 implementation
    /// that exists with this module's reading of a PRM for a different
    /// generation, which is exactly the kind of swap that cannot be debugged on
    /// a machine with no serial port.  A caller that wants to know whether the
    /// divider it is about to program is PRM-legal asks this; a caller that
    /// wants what i915 would write asks [`Self::p`] and friends.
    pub(crate) fn prm_legal_divider_set(&self) -> Option<(u32, u32, u32)> {
        prm_legal_divider_set(self.total_divider)
    }

    /// `CFGCR0` for this divider set: `DCO_FRACTION[24:10]` and
    /// `DCO_INTEGER[9:0]`.
    ///
    /// `[I915]` `icl_calc_dpll_state` (`intel_dpll_mgr.c:2894-2895`) builds this
    /// as `DPLL_CFGCR0_DCO_FRACTION(fraction) | dco_integer`, and `i915_reg.h`
    /// gives the fields as `DCO_FRACTION[24:10]` with mask `0x7fff << 10`
    /// (`i915_reg.h:4270-4273`) and `DCO_INTEGER` as the low ten bits.  The
    /// `LINK_RATE` override at `[28:25]` is left zero, which the reference
    /// document §6.3 says is correct for HDMI.
    ///
    /// `workaround` is a parameter because it is a property of the platform,
    /// which this module is not given: see [`DcoFractionWorkaround`].
    ///
    /// The remaining `CFGCR1` bits are not here because they are not this
    /// module's: `DPLL_CFGCR1_CENTRAL_FREQ` is the Gen11 encoding, and on Gen12
    /// `[1:0]` is `CFSELOVRD` and must be
    /// `TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL` = 0 (`i915_reg.h:4299`), which
    /// `icl_calc_dpll_state` selects only for `DISPLAY_VER >= 12`
    /// (`intel_dpll_mgr.c:2902-2905`).  Zero is therefore already correct and
    /// this function does not have to say so.
    pub(crate) const fn cfgcr0(&self, workaround: DcoFractionWorkaround) -> u32 {
        let fraction = match workaround {
            DcoFractionWorkaround::NotNeeded => self.dco_fraction,
            // i915 uses DIV_ROUND_CLOSEST here (`intel_dpll_mgr.c:2892`);
            // rounding to nearest rather than truncating halves the error the
            // halving itself introduces.
            DcoFractionWorkaround::HalveFraction => (self.dco_fraction + 1) / 2,
        };
        (fraction << 10) | self.dco_integer
    }

    /// `CFGCR1` for this divider set under a named encoding.
    ///
    /// Fields, from `i915_reg.h:4279-4299`: `QDIV_RATIO[17:10]`,
    /// `QDIV_MODE[9]`, `KDIV[8:6]`, `PDIV[5:2]`, `CFSELOVRD[1:0]`.
    /// `QDIV_MODE` is 0 when `Q == 1` and 1 otherwise, which is i915's rule
    /// (`intel_dpll_mgr.c:1646`).
    ///
    /// This is the fallible half.  It returns [`PllError::DividerNotEncodable`]
    /// rather than a value when the encoding has no code for the chosen
    /// divider, which is the §6.3 `[GAP]`; see [`PllFieldEncoding`].
    pub(crate) fn cfgcr1(&self, encoding: PllFieldEncoding) -> Result<u32, PllError> {
        let qdiv_ratio = field_value(self.q, 8, PllDividerField::QdivRatio)?;
        let qdiv_mode = u32::from(self.q != 1);
        let kdiv = kdiv_code(self.k, encoding)?;
        let pdiv = pdiv_code(self.p, encoding)?;
        Ok((qdiv_ratio << 10) | (qdiv_mode << 9) | (kdiv << 6) | (pdiv << 2))
    }

    /// `CFGCR0` and `CFGCR1` together, which is what a modeset actually writes.
    pub(crate) fn registers(
        &self,
        encoding: PllFieldEncoding,
        workaround: DcoFractionWorkaround,
    ) -> Result<PllRegisters, PllError> {
        Ok(PllRegisters {
            cfgcr0: self.cfgcr0(workaround),
            cfgcr1: self.cfgcr1(encoding)?,
        })
    }
}

/// A `CFGCR0`/`CFGCR1` pair, as written or as read back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PllRegisters {
    pub(crate) cfgcr0: u32,
    pub(crate) cfgcr1: u32,
}

impl PllRegisters {
    /// Decode these registers back into a symbol rate in Hz.
    ///
    /// This is a reimplementation of i915's read-back path,
    /// `icl_ddi_combo_pll_get_freq` (`intel_dpll_mgr.c:1740-1802`), and it
    /// exists so that the write path can be round-tripped in a host test: the
    /// reference document §6.3 route 2 asks for exactly this ("implement
    /// i915's executed encoding, and verify by read-back before enabling"),
    /// and the same decode is what a bring-up should run against real
    /// registers once it can.
    ///
    /// `encoding` must be the encoding the registers were written with, and
    /// that is the point rather than a wart: i915 writes with one convention
    /// and reads with the other, so a caller that assumes they agree gets a
    /// confident wrong answer.  Passing the wrong encoding here is how that
    /// shows up as a test failure instead of a blank screen.
    pub(crate) fn symbol_rate_hz(
        &self,
        platform_ref_khz: u32,
        encoding: PllFieldEncoding,
        workaround: DcoFractionWorkaround,
    ) -> Result<u64, PllError> {
        let wrpll_ref_khz = wrpll_reference_khz(platform_ref_khz)?;

        let p = decode_pdiv((self.cfgcr1 >> 2) & 0xf, encoding)?;
        let k = decode_kdiv((self.cfgcr1 >> 6) & 0x7, encoding)?;
        let q = if self.cfgcr1 & (1 << 9) != 0 {
            (self.cfgcr1 >> 10) & 0xff
        } else {
            1
        };

        let dco_integer = u64::from(self.cfgcr0 & 0x3ff);
        let mut dco_fraction = u64::from((self.cfgcr0 >> 10) & 0x7fff);
        if workaround == DcoFractionWorkaround::HalveFraction {
            dco_fraction *= 2;
        }

        let ref_hz = u64::from(wrpll_ref_khz) * 1000;
        let dco_hz = dco_integer * ref_hz + (dco_fraction * ref_hz) / 0x8000;
        let divisor = 5 * u64::from(p) * u64::from(q) * u64::from(k);
        Ok(dco_hz / divisor)
    }
}

/// Decode `SKL_DSSM`'s reference-clock field into a frequency in kHz.
///
/// `[I915]` `icl_readout_refclk` (`display/intel_cdclk.c:1583-1603`) decodes
/// `SKL_DSSM[31:29]` as 24 MHz, 19.2 MHz and 38.4 MHz, and calls
/// `MISSING_CASE` on anything else.  This returns an error for anything else
/// rather than falling back to 24 MHz, because the reference frequency scales
/// every number this module produces.
pub(crate) fn dssm_reference_khz(dssm: u32) -> Result<u32, PllError> {
    let field = dssm & DSSM_REFCLK_MASK;
    match field {
        DSSM_REFCLK_24MHZ => Ok(24_000),
        DSSM_REFCLK_19_2MHZ => Ok(19_200),
        DSSM_REFCLK_38_4MHZ => Ok(38_400),
        _ => Err(PllError::UnsupportedDssmReference { field: field >> 29 }),
    }
}

/// The reference frequency the DCO arithmetic divides by, in kHz.
///
/// `[I915]` `icl_wrpll_ref_clock` (`intel_dpll_mgr.c:1737-1752`): a 38.4 MHz
/// platform reference becomes 19.2 MHz, because "the DPLL automatically divides
/// that by 2".  Every other sourced value passes through.
///
/// The accepted set is the three frequencies i915 decodes from `SKL_DSSM`; any
/// other value is refused rather than used, because this module has no source
/// for what the arithmetic would mean.
pub(crate) fn wrpll_reference_khz(platform_ref_khz: u32) -> Result<u32, PllError> {
    match platform_ref_khz {
        0 => Err(PllError::ZeroReference),
        38_400 => Ok(19_200),
        other @ (19_200 | 24_000) => Ok(other),
        other => Err(PllError::UnsupportedReference { ref_khz: other }),
    }
}

/// The DDI PLL dividers for a pixel clock on an HDMI/DVI port.
///
/// For HDMI the TMDS character rate equals the pixel clock, so the symbol rate
/// this needs is the pixel clock itself: `[I915]` `skl_ddi_calculate_wrpll`
/// takes `crtc_state->port_clock` and sets `afe_clock = clock * 1000 * 5`
/// (`intel_dpll_mgr.c:1686`), which is the same statement.  That is the case
/// the reference document §6.3 works through, and it is the case a first
/// light-up on this machine uses.
///
/// DisplayPort is **not** this function.  On DP the symbol rate is the link
/// rate, which is not the pixel clock, so a DP caller must use
/// [`ddi_pll_dividers_for_symbol_rate`] with the link symbol rate its trained
/// link uses.  This wrapper exists because the HDMI case is the one with a
/// published modeline to check against.
pub(crate) fn ddi_pll_dividers(
    pixel_clock_khz: u32,
    ref_khz: u32,
    phy: ComboPhy,
) -> Result<DdiPllDividers, PllError> {
    ddi_pll_dividers_for_symbol_rate(pixel_clock_khz, ref_khz, phy)
}

/// The DDI PLL dividers for an arbitrary symbol rate.
///
/// This is the real entry point; [`ddi_pll_dividers`] is the HDMI convenience
/// wrapper over it.  The search reproduces i915's `skl_ddi_calculate_wrpll`
/// (`intel_dpll_mgr.c:1660-1730`) exactly, including its preference for an even
/// total divider, because that `[GAP]` -- §13.1 item 11 -- is the one thing the
/// reference document could not extract and this is the only display-13 source
/// that has it.
///
/// The returned error is the honest one: when no candidate is legal this says
/// so, instead of returning the nearest divider and a pixel clock nobody asked
/// for.
pub(crate) fn ddi_pll_dividers_for_symbol_rate(
    symbol_rate_khz: u32,
    ref_khz: u32,
    phy: ComboPhy,
) -> Result<DdiPllDividers, PllError> {
    if symbol_rate_khz == 0 {
        return Err(PllError::ZeroSymbolRate);
    }
    let wrpll_ref_khz = wrpll_reference_khz(ref_khz)?;

    // `afe_clock` is 5x the symbol rate, in Hz.  u64 throughout: the largest
    // DCO this can produce is 98 * 5 * 646 MHz, about 3.2e11, which is well
    // inside a u64 and far outside the u32 a register field would hold -- which
    // is why `dco_integer` is range-checked before it is handed back.
    let afe_clock_hz = 5 * u64::from(symbol_rate_khz) * 1000;

    let chosen = search_total_divider(afe_clock_hz)
        .ok_or(PllError::NoLegalDividerSet {
            symbol_rate_khz,
            ref_khz,
        })?;

    let (p, q, k) = decompose(chosen.total_divider)
        .ok_or(PllError::DividerNotDecomposable {
            total_divider: chosen.total_divider,
        })?;

    let target_dco_hz = u64::from(chosen.total_divider) * afe_clock_hz;
    let ref_hz = u64::from(wrpll_ref_khz) * 1000;
    let dco_integer = target_dco_hz / ref_hz;
    let dco_fraction = ((target_dco_hz % ref_hz) * 0x8000) / ref_hz;

    let dco_integer = field_value(u32::try_from(dco_integer).unwrap_or(u32::MAX), 10, PllDividerField::DcoInteger)?;
    let dco_fraction = field_value(u32::try_from(dco_fraction).unwrap_or(u32::MAX), 15, PllDividerField::DcoFraction)?;

    // What the registers will really produce, which is the target DCO rounded
    // down to the fraction's resolution.
    let achieved_dco_hz =
        u64::from(dco_integer) * ref_hz + (u64::from(dco_fraction) * ref_hz) / 0x8000;
    let divisor = 5 * u64::from(chosen.total_divider);
    let achieved_symbol_rate_hz = achieved_dco_hz / divisor;

    // Measured against the DCO rather than the divided-down rate, so the
    // division's own truncation does not enter the figure.
    let error_ppb = rate_error_ppb(achieved_dco_hz, target_dco_hz);
    if error_ppb.unsigned_abs() > MAX_SYMBOL_RATE_ERROR_PPB {
        return Err(PllError::AchievedRateOutOfTolerance {
            error_ppb,
            limit_ppb: MAX_SYMBOL_RATE_ERROR_PPB,
        });
    }

    Ok(DdiPllDividers {
        phy,
        platform_ref_khz: ref_khz,
        wrpll_ref_khz,
        symbol_rate_khz,
        total_divider: chosen.total_divider,
        p,
        q,
        k,
        central_freq_khz: chosen.central_freq_hz / 1000,
        target_dco_khz: target_dco_hz / 1000,
        deviation_centipercent: chosen.deviation,
        dco_integer,
        dco_fraction,
        achieved_dco_hz,
        achieved_symbol_rate_hz,
        rate_error_ppb: error_ppb,
    })
}

/// A divider the search accepted, and why.
#[derive(Clone, Copy, Debug)]
struct Chosen {
    total_divider: u32,
    central_freq_hz: u64,
    deviation: u64,
}

/// The search state, mirroring i915's `struct skl_wrpll_context`
/// (`intel_dpll_mgr.c:1493-1498`).
///
/// `total_divider == 0` means "nothing accepted yet", which is how i915 uses
/// `ctx.p`; the field is not a divider value in that case.
#[derive(Clone, Copy, Debug)]
struct Search {
    min_deviation: u64,
    central_freq_hz: u64,
    total_divider: u32,
    deviation: u64,
}

/// i915's `skl_wrpll_try_divider` (`intel_dpll_mgr.c:1504-1531`).
///
/// Both frequencies are in Hz.  The deviation is a ratio, so its unit cancels;
/// what does not cancel is the *comparison*, and a kHz DCO against a Hz centre
/// would be a thousand times off.
fn try_divider(search: &mut Search, central_freq_hz: u64, dco_hz: u64, total_divider: u32) {
    let deviation = 10_000 * central_freq_hz.abs_diff(dco_hz) / central_freq_hz;
    let within_tolerance = if dco_hz >= central_freq_hz {
        deviation < DCO_MAX_POSITIVE_DEVIATION
    } else {
        deviation < DCO_MAX_NEGATIVE_DEVIATION
    };
    if within_tolerance && deviation < search.min_deviation {
        search.min_deviation = deviation;
        search.central_freq_hz = central_freq_hz;
        search.total_divider = total_divider;
        search.deviation = deviation;
    }
}

/// i915's `skl_ddi_calculate_wrpll` search (`intel_dpll_mgr.c:1681-1718`),
/// including both of its tie-breaks.
///
/// The loop structure is copied as *behaviour* and matters:
///
/// * a candidate whose deviation is exactly zero ends the search immediately,
///   across all three central frequencies; and
/// * if **any** even total divider was accepted, the odd list is never tried --
///   "If a solution is found with an even divider, prefer this one"
///   (`intel_dpll_mgr.c:1709-1714`).
///
/// The second rule is strong enough to be surprising: it prefers an even
/// divider with a deviation of 5.95% over an odd one that is exact.  The
/// `an_even_divider_wins_even_when_an_odd_one_is_exact` test pins that, because
/// a well-meaning "fix" here would silently change which pixel clock the
/// machine produces.
fn search_total_divider(afe_clock_hz: u64) -> Option<Chosen> {
    let mut search = Search {
        min_deviation: u64::MAX,
        central_freq_hz: 0,
        total_divider: 0,
        deviation: 0,
    };

    for (index, dividers) in [EVEN_TOTAL_DIVIDERS, ODD_TOTAL_DIVIDERS]
        .into_iter()
        .enumerate()
    {
        let mut exact = false;
        for central_freq_hz in DCO_CENTRAL_FREQ_HZ {
            for &total_divider in dividers {
                let dco_hz = u64::from(total_divider) * afe_clock_hz;
                try_divider(&mut search, central_freq_hz, dco_hz, total_divider);
                if search.min_deviation == 0 {
                    exact = true;
                    break;
                }
            }
            if exact {
                break;
            }
        }
        // The even list is index 0; an accepted even divider ends the search.
        if index == 0 && search.total_divider != 0 {
            break;
        }
    }

    if search.total_divider == 0 {
        return None;
    }
    Some(Chosen {
        total_divider: search.total_divider,
        central_freq_hz: search.central_freq_hz,
        deviation: search.deviation,
    })
}

/// i915's `skl_wrpll_get_multipliers` (`intel_dpll_mgr.c:1533-1580`).
///
/// `None` where i915 leaves its outputs untouched and warns, which cannot
/// happen for the two candidate lists -- the
/// `every_candidate_divider_decomposes` test proves that -- but is returned
/// rather than assumed so that editing a list cannot silently produce a
/// zero divider.
fn decompose(total_divider: u32) -> Option<(u32, u32, u32)> {
    if total_divider % 2 == 0 {
        let half = total_divider / 2;
        if half == 1 || half == 2 || half == 3 || half == 5 {
            Some((2, 1, half))
        } else if half % 2 == 0 {
            Some((2, half / 2, 2))
        } else if half % 3 == 0 {
            Some((3, half / 3, 2))
        } else if half % 7 == 0 {
            Some((7, half / 7, 2))
        } else {
            None
        }
    } else if total_divider == 3 || total_divider == 9 {
        Some((3, 1, total_divider / 3))
    } else if total_divider == 5 || total_divider == 7 {
        Some((total_divider, 1, 1))
    } else if total_divider == 15 {
        Some((3, 1, 5))
    } else if total_divider == 21 {
        Some((7, 1, 3))
    } else if total_divider == 35 {
        Some((7, 1, 5))
    } else {
        None
    }
}

/// A `(P, Q, K)` satisfying the bounds the PRM states, for a total divider.
///
/// Reference document §6.3, "The search bounds": `P ∈ {2,3,5,7}`,
/// `K ∈ {1,2,3}`, `Q ∈ 1..255`, and `K != 2 ⇒ Q = 1`.  The search is over `K`
/// then `P` in ascending order, so the answer is deterministic but is *a* legal
/// triple rather than necessarily i915's -- for `total = 6` it finds
/// `(3, 1, 2)` where i915 writes `(2, 1, 3)`, and both are legal.
fn prm_legal_divider_set(total_divider: u32) -> Option<(u32, u32, u32)> {
    for k in [1u32, 2, 3] {
        for p in [2u32, 3, 5, 7] {
            let product = p * k;
            if total_divider % product != 0 {
                continue;
            }
            let q = total_divider / product;
            if q == 0 || q > 255 {
                continue;
            }
            if k != 2 && q != 1 {
                continue;
            }
            return Some((p, q, k));
        }
    }
    None
}

/// The `PDIV` field code for a post divider, in the low bits (not shifted).
///
/// Both conventions come from i915 and both are cited in [`PllFieldEncoding`].
/// The `Executed` arm has no case for `P = 5`, which is what i915's
/// `default: WARN(1, "Incorrect PDiv")` means; `5` is *reachable* from the odd
/// candidate list, so this is a live path and not a theoretical one -- see the
/// `a_post_divider_of_five_has_no_executed_code` test.
fn pdiv_code(p: u32, encoding: PllFieldEncoding) -> Result<u32, PllError> {
    let code = match encoding {
        // intel_dpll_mgr.c:1611-1626.
        PllFieldEncoding::Executed => match p {
            1 => 0,
            2 => 1,
            3 => 2,
            7 => 4,
            _ => return Err(not_encodable(p, 0, PllDividerField::Post, encoding)),
        },
        // i915_reg.h:4293-4296.
        PllFieldEncoding::Named => match p {
            2 => 1,
            3 => 2,
            5 => 4,
            7 => 8,
            _ => return Err(not_encodable(p, 0, PllDividerField::Post, encoding)),
        },
    };
    Ok(code)
}

/// The `KDIV` field code for a `K`, in the low bits (not shifted).
fn kdiv_code(k: u32, encoding: PllFieldEncoding) -> Result<u32, PllError> {
    let code = match encoding {
        // intel_dpll_mgr.c:1628-1643.
        PllFieldEncoding::Executed => match k {
            5 => 0,
            2 => 1,
            3 => 2,
            1 => 3,
            _ => return Err(not_encodable(0, k, PllDividerField::K, encoding)),
        },
        // i915_reg.h:4287-4289.
        PllFieldEncoding::Named => match k {
            1 => 1,
            2 => 2,
            3 => 4,
            _ => return Err(not_encodable(0, k, PllDividerField::K, encoding)),
        },
    };
    Ok(code)
}

/// The inverse of [`pdiv_code`].
fn decode_pdiv(code: u32, encoding: PllFieldEncoding) -> Result<u32, PllError> {
    match encoding {
        PllFieldEncoding::Executed => match code {
            0 => Ok(1),
            1 => Ok(2),
            2 => Ok(3),
            4 => Ok(7),
            // 3 and 5..15 are codes i915 never writes; "Incorrect PDiv".
            _ => Err(PllError::DividerNotEncodable {
                p: code,
                k: 0,
                field: PllDividerField::Post,
                encoding,
            }),
        },
        PllFieldEncoding::Named => match code {
            1 => Ok(2),
            2 => Ok(3),
            4 => Ok(5),
            8 => Ok(7),
            _ => Err(PllError::DividerNotEncodable {
                p: code,
                k: 0,
                field: PllDividerField::Post,
                encoding,
            }),
        },
    }
}

/// The inverse of [`kdiv_code`].
fn decode_kdiv(code: u32, encoding: PllFieldEncoding) -> Result<u32, PllError> {
    match encoding {
        PllFieldEncoding::Executed => match code {
            0 => Ok(5),
            1 => Ok(2),
            2 => Ok(3),
            3 => Ok(1),
            _ => Err(PllError::DividerNotEncodable {
                p: 0,
                k: code,
                field: PllDividerField::K,
                encoding,
            }),
        },
        PllFieldEncoding::Named => match code {
            1 => Ok(1),
            2 => Ok(2),
            4 => Ok(3),
            _ => Err(PllError::DividerNotEncodable {
                p: 0,
                k: code,
                field: PllDividerField::K,
                encoding,
            }),
        },
    }
}

/// The error [`pdiv_code`] and [`kdiv_code`] return when they have no code.
const fn not_encodable(p: u32, k: u32, field: PllDividerField, encoding: PllFieldEncoding) -> PllError {
    PllError::DividerNotEncodable {
        p,
        k,
        field,
        encoding,
    }
}

/// Check that a value fits a register field of `bits` bits.
fn field_value(value: u32, bits: u32, field: PllDividerField) -> Result<u32, PllError> {
    if bits >= 32 || value < (1u32 << bits) {
        Ok(value)
    } else {
        Err(PllError::FieldOverflow {
            field,
            value,
            bits,
        })
    }
}

/// A signed relative error in parts per billion.
fn rate_error_ppb(achieved_hz: u64, target_hz: u64) -> i64 {
    if target_hz == 0 {
        return 0;
    }
    let difference = achieved_hz as i128 - target_hz as i128;
    let ppb = difference * 1_000_000_000i128 / target_hz as i128;
    i64::try_from(ppb).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference frequency the ADL-N PLL strips use most often, and the one
    /// the worked example in the reference document uses.
    const REF_24: u32 = 24_000;

    fn hdmi(pixel_clock_khz: u32) -> DdiPllDividers {
        ddi_pll_dividers(pixel_clock_khz, REF_24, ComboPhy::A).expect("a legal divider set")
    }

    // -- published modelines -------------------------------------------------

    /// CTA-861 VIC 16: 1920x1080@60, 148.5 MHz, htotal 2200, vtotal 1125.
    ///
    /// The totals are the ones the reference document §6.3 uses for its worked
    /// example and the ones the kernel's own CTA-861 table carries.  Over HDMI
    /// the TMDS character rate is the pixel clock, so `afe_clock` is 742.5 MHz
    /// and the doc's table says the total divider must be 12 to land the DCO on
    /// 8910 MHz.
    #[test]
    fn cta_vic_16_1080p60_over_hdmi_lands_where_the_reference_says() {
        let dividers = hdmi(148_500);
        assert_eq!(dividers.total_divider(), 12, "the doc's worked example");
        assert_eq!(dividers.target_dco_khz(), 8_910_000);
        assert_eq!(dividers.central_freq_khz(), 9_000_000);
        // The doc offers "P=2, Q=2, K=3 or P=3, Q=2, K=2" and then rules the
        // second out because K=2 requires Q=1.  i915's own decomposition of 12
        // is the first shape's sibling, (2, 3, 2).
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (2, 3, 2));
        assert!(dividers.inside_prm_dco_window());
        // 8910 MHz is 24 MHz * 371.25, so the fraction is exactly a quarter and
        // the registers reproduce the rate exactly.
        assert_eq!(dividers.dco_integer(), 371);
        assert_eq!(dividers.dco_fraction(), 0x2000);
        assert_eq!(dividers.rate_error_ppb(), 0);
        assert_eq!(dividers.achieved_symbol_rate_hz(), 148_500_000);
    }

    /// CTA-861 VIC 4: 1280x720@60, 74.25 MHz.  Half of VIC 16's clock, so the
    /// same DCO with twice the division.
    #[test]
    fn cta_vic_4_720p60_over_hdmi_doubles_the_divider() {
        let dividers = hdmi(74_250);
        assert_eq!(dividers.total_divider(), 24);
        assert_eq!(dividers.target_dco_khz(), 8_910_000);
        assert_eq!(dividers.central_freq_khz(), 9_000_000);
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (2, 6, 2));
        assert_eq!(dividers.achieved_symbol_rate_hz(), 74_250_000);
        assert_eq!(dividers.rate_error_ppb(), 0);
    }

    /// DMT 0x04: 640x480@60, 25.175 MHz.  The interesting one, because the DCO
    /// does not land on a central frequency: the search has to accept a
    /// candidate that is off by a fraction of a percent, which is what the
    /// asymmetric +1%/-6% tolerance is for.
    #[test]
    fn dmt_0x04_640x480_60_finds_an_off_centre_dco() {
        let dividers = hdmi(25_175);
        assert_eq!(dividers.total_divider(), 76);
        assert_eq!(dividers.target_dco_khz(), 9_566_500);
        assert_eq!(dividers.central_freq_khz(), 9_600_000);
        // 0.34% below the 9600 MHz centre, well inside the 6% negative limit.
        assert_eq!(dividers.deviation_centipercent(), 34);
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (2, 19, 2));
        assert!(dividers.inside_prm_dco_window());
        // The rate is reproduced to within the fraction's resolution.
        assert_eq!(dividers.achieved_symbol_rate_hz(), 25_174_999);
        assert!(dividers.rate_error_ppb().abs() < 100, "{dividers:?}");
    }

    /// Every published modeline this module is asked about must produce a
    /// symbol rate that is right to well inside a part per million, and the
    /// register round trip must agree with the request.
    #[test]
    fn published_modelines_round_trip_through_their_registers() {
        // (name, pixel clock in kHz) -- CTA-861 VICs 4, 16, 31, 93, 97 and VESA
        // DMT codes 0x04, 0x09, 0x10, 0x52, 0x53.
        let modelines: &[(&str, u32)] = &[
            ("CTA VIC 4 1280x720@60", 74_250),
            ("CTA VIC 16 1920x1080@60", 148_500),
            ("CTA VIC 31 1920x1080@50", 148_500),
            ("CTA VIC 93 3840x2160@24", 297_000),
            ("CTA VIC 97 3840x2160@60", 594_000),
            ("DMT 0x04 640x480@60", 25_175),
            ("DMT 0x09 800x600@60", 40_000),
            ("DMT 0x10 1024x768@60", 65_000),
            ("DMT 0x52 1920x1080@60 CVT-RB", 138_500),
            ("DMT 0x53 1920x1080@50 CVT-RB", 115_500),
        ];
        for &(name, clock_khz) in modelines {
            for encoding in [PllFieldEncoding::Executed, PllFieldEncoding::Named] {
                let dividers = hdmi(clock_khz);
                let Ok(registers) = dividers.registers(encoding, DcoFractionWorkaround::NotNeeded)
                else {
                    // A divider set with no code under this encoding is the
                    // §6.3 gap, and is asserted separately.
                    continue;
                };
                let decoded = registers
                    .symbol_rate_hz(REF_24, encoding, DcoFractionWorkaround::NotNeeded)
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                let requested_hz = u64::from(clock_khz) * 1000;
                let error_ppb =
                    (decoded as i128 - requested_hz as i128) * 1_000_000_000i128 / requested_hz as i128;
                assert!(
                    error_ppb.abs() < 1_000,
                    "{name}: round trip produced {decoded} Hz for {requested_hz} Hz \
                     ({error_ppb} ppb) under {encoding}"
                );
            }
        }
    }

    // -- the search's documented rules --------------------------------------

    /// Every divider in both candidate lists must decompose.  i915's lists are
    /// chosen so this holds; if a future edit breaks it, the search would
    /// otherwise silently return a zero divider.
    #[test]
    fn every_candidate_divider_decomposes() {
        for &total_divider in EVEN_TOTAL_DIVIDERS.iter().chain(ODD_TOTAL_DIVIDERS) {
            let (p, q, k) = decompose(total_divider)
                .unwrap_or_else(|| panic!("divider {total_divider} does not decompose"));
            assert_eq!(
                p * q * k,
                total_divider,
                "divider {total_divider} decomposed to ({p}, {q}, {k})"
            );
            assert!(q >= 1 && q <= 255, "divider {total_divider}: Q = {q}");
            // i915's decomposition always satisfies the PRM's Q/K rule; that
            // the two agree here is worth pinning, because it is the one place
            // they do.
            if k != 2 {
                assert_eq!(q, 1, "divider {total_divider}: K = {k} requires Q = 1");
            }
        }
        // And a divider that is not in either list does not silently decompose.
        assert!(decompose(11).is_none());
        assert!(decompose(1).is_none());
        assert!(decompose(49).is_none());
    }

    /// i915 prefers an even total divider strongly enough to take a worse DCO.
    ///
    /// At a 200 MHz symbol rate `afe_clock` is 1000 MHz: the even divider 8
    /// gives an 8000 MHz DCO, which is 4.76% below the 8400 MHz centre and is
    /// therefore *accepted*, so the odd divider 9 -- which would land exactly
    /// on 9000 MHz -- is never tried.  This is i915's documented intent
    /// (`intel_dpll_mgr.c:1709-1714`) and this module reproduces it.
    #[test]
    fn an_even_divider_wins_even_when_an_odd_one_is_exact() {
        let dividers = hdmi(200_000);
        assert_eq!(dividers.total_divider(), 8, "the even divider, not 9");
        assert_eq!(dividers.target_dco_khz(), 8_000_000);
        assert_eq!(dividers.central_freq_khz(), 8_400_000);
        assert_eq!(dividers.deviation_centipercent(), 476);
        // 9 would have been exact: 9 * 1000 MHz = 9000 MHz.
        assert_eq!(9 * 5 * 200_000, 9_000_000);
    }

    /// The DCO window and the central-frequency rule are different tests and
    /// pick different dividers.  This is the `[INF]` the reference document
    /// §6.3 flags when it says the two framings are "compatible in spirit but
    /// not bit-identical"; the divergence is pinned here so nobody has to take
    /// it on trust.
    #[test]
    fn the_two_dco_framings_really_do_differ() {
        // 395 MHz: afe_clock is 1975 MHz.
        let dividers = hdmi(395_000);
        // i915 accepts the even divider 4 at 7900 MHz, 5.95% below 8400 MHz.
        assert_eq!(dividers.total_divider(), 4);
        assert_eq!(dividers.target_dco_khz(), 7_900_000);
        assert_eq!(dividers.deviation_centipercent(), 595);
        // That DCO is *outside* the PRM's documented window.
        assert!(!dividers.inside_prm_dco_window());
        // The PRM framing would instead take the divider 5, at 9875 MHz, which
        // is the candidate closest to the 8999 MHz midpoint.
        assert_eq!(5 * 1_975_000, 9_875_000);
        assert!(9_875_000 >= PRM_DCO_MIN_KHZ && 9_875_000 <= PRM_DCO_MAX_KHZ);
        assert!(
            (9_875_000u64).abs_diff(8_999_000) < (7_900_000u64).abs_diff(8_999_000),
            "the midpoint rule really does prefer the other divider"
        );
    }

    // -- the PDIV/KDIV gap ---------------------------------------------------

    /// `P = 5` has no `Executed` code at all.
    ///
    /// A 360 MHz symbol rate has `afe_clock` 1800 MHz, and the odd divider 5
    /// lands exactly on the 9000 MHz centre while no even divider is within
    /// tolerance.  The search therefore returns `P = 5`, which
    /// `skl_wrpll_params_populate` has no case for: i915 warns "Incorrect
    /// PDiv" and programs whatever `pdiv` its zero-initialised struct held.
    /// This module refuses instead.
    #[test]
    fn a_post_divider_of_five_has_no_executed_code() {
        let dividers = hdmi(360_000);
        assert_eq!(dividers.total_divider(), 5);
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (5, 1, 1));
        assert_eq!(dividers.target_dco_khz(), 9_000_000);
        assert_eq!(dividers.deviation_centipercent(), 0);
        assert_eq!(
            dividers.cfgcr1(PllFieldEncoding::Executed),
            Err(PllError::DividerNotEncodable {
                p: 5,
                k: 0,
                field: PllDividerField::Post,
                encoding: PllFieldEncoding::Executed,
            })
        );
        // The named-constant convention does have one, and the PRM agrees that
        // P = 5 is a legal post divider.
        assert!(dividers.cfgcr1(PllFieldEncoding::Named).is_ok());
        assert_eq!(dividers.prm_legal_divider_set(), Some((5, 1, 1)));
        // The DCO arithmetic is unaffected: this is an encoding problem only.
        assert_eq!(dividers.achieved_symbol_rate_hz(), 360_000_000);
    }

    /// `K = 5` is a divider i915 writes but the PRM does not define.
    ///
    /// A 180 MHz symbol rate puts the DCO exactly on 9000 MHz with the even
    /// divider 10, which `skl_wrpll_get_multipliers` decomposes as
    /// `(2, 1, 5)`.  `K = 5` is not in the PRM's `K ∈ {1,2,3}`, and there is no
    /// `DPLL_CFGCR1_KDIV_5` constant -- but `skl_wrpll_params_populate` has a
    /// `case 5: kdiv = 0`, so i915 writes a code for it anyway.
    #[test]
    fn a_k_of_five_is_written_but_is_not_a_legal_prm_k() {
        let dividers = hdmi(180_000);
        assert_eq!(dividers.total_divider(), 10);
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (2, 1, 5));
        assert_eq!(dividers.target_dco_khz(), 9_000_000);
        // i915 has a code for it ...
        assert!(dividers.cfgcr1(PllFieldEncoding::Executed).is_ok());
        // ... and its named constants do not.
        assert_eq!(
            dividers.cfgcr1(PllFieldEncoding::Named),
            Err(PllError::DividerNotEncodable {
                p: 0,
                k: 5,
                field: PllDividerField::K,
                encoding: PllFieldEncoding::Named,
            })
        );
        // The PRM's own bounds reach the same total a different way, which is
        // what makes this worth reporting rather than crashing on.
        assert_eq!(dividers.prm_legal_divider_set(), Some((5, 1, 2)));
        assert_ne!(dividers.prm_legal_divider_set(), Some((2, 1, 5)));
    }

    /// The two encodings disagree for every `K` and for `P = 7`, and agree for
    /// `P ∈ {2, 3}`.  This is the §6.3 discrepancy, enumerated.
    #[test]
    fn the_two_encodings_disagree_where_the_reference_says_they_do() {
        // P = 7: the executed code is the Skylake {1,2,3,7} -> {0,1,2,4} one,
        // the named constant is the Gen12 {2,3,5,7} -> {1,2,4,8} one.
        assert_eq!(pdiv_code(7, PllFieldEncoding::Executed), Ok(4));
        assert_eq!(pdiv_code(7, PllFieldEncoding::Named), Ok(8));
        // P = 2 and P = 3 happen to agree between the two conventions.
        for p in [2u32, 3] {
            assert_eq!(
                pdiv_code(p, PllFieldEncoding::Executed),
                pdiv_code(p, PllFieldEncoding::Named),
                "P = {p}"
            );
        }
        // P = 1 exists only in the Skylake convention, P = 5 only in Gen12's.
        assert!(pdiv_code(1, PllFieldEncoding::Executed).is_ok());
        assert!(pdiv_code(1, PllFieldEncoding::Named).is_err());
        assert!(pdiv_code(5, PllFieldEncoding::Executed).is_err());
        assert!(pdiv_code(5, PllFieldEncoding::Named).is_ok());

        // Every K disagrees.
        for k in [1u32, 2, 3] {
            let executed = kdiv_code(k, PllFieldEncoding::Executed);
            let named = kdiv_code(k, PllFieldEncoding::Named);
            assert!(executed.is_ok() && named.is_ok(), "K = {k}");
            assert_ne!(executed, named, "K = {k} must differ between conventions");
        }
    }

    /// A divider the search can find, and encode under one convention, gets a
    /// different `CFGCR1` under the other.  The pair below is the clearest
    /// single example: `P = 2`, `K = 2`, `Q = 3`.
    #[test]
    fn the_encoding_choice_changes_the_register_value() {
        let dividers = hdmi(148_500);
        let executed = dividers.cfgcr1(PllFieldEncoding::Executed).unwrap();
        let named = dividers.cfgcr1(PllFieldEncoding::Named).unwrap();
        // QDIV_RATIO(3) << 10 | QDIV_MODE(1) << 9, shared by both.
        assert_eq!(executed & 0x000f_fe00, 0x0000_0e00);
        assert_eq!(named & 0x000f_fe00, 0x0000_0e00);
        // PDIV: both conventions code P = 2 as 1, so this part agrees.
        assert_eq!((executed >> 2) & 0xf, 1);
        assert_eq!((named >> 2) & 0xf, 1);
        // KDIV: they do not agree.  Executed codes K = 2 as 1, named as 2.
        assert_eq!((executed >> 6) & 0x7, 1);
        assert_eq!((named >> 6) & 0x7, 2);
        assert_ne!(executed, named);
    }

    /// i915 writes with one convention and reads back with the other.  Doing
    /// what i915 does -- encode `Executed`, decode `Named` -- does not return
    /// the rate that was asked for, and the size of the error depends only on
    /// `K`.  This is the concrete reason both encodings are offered explicitly
    /// instead of a default being chosen.
    #[test]
    fn i915s_own_write_and_read_conventions_do_not_agree() {
        let dividers = hdmi(148_500);
        let registers = dividers
            .registers(PllFieldEncoding::Executed, DcoFractionWorkaround::NotNeeded)
            .unwrap();

        // Read back the way it was written: exact.
        let honest = registers
            .symbol_rate_hz(REF_24, PllFieldEncoding::Executed, DcoFractionWorkaround::NotNeeded)
            .unwrap();
        assert_eq!(honest, 148_500_000);

        // Read back i915's way -- `icl_ddi_combo_pll_get_freq` decodes KDIV with
        // the named constants.  The chosen set has K = 2, written as the
        // executed code 1, which the named convention reads as K = 1.
        let i915_way = registers
            .symbol_rate_hz(REF_24, PllFieldEncoding::Named, DcoFractionWorkaround::NotNeeded)
            .unwrap();
        assert_eq!(i915_way, 297_000_000, "i915's own read-back doubles the rate");
        assert_ne!(i915_way, honest);
    }

    // -- reference clock -----------------------------------------------------

    /// The reference a 38.4 MHz strap is divided to, and the consequence.
    #[test]
    fn a_38_4_mhz_reference_is_divided_to_19_2_for_the_arithmetic() {
        assert_eq!(wrpll_reference_khz(38_400), Ok(19_200));
        assert_eq!(wrpll_reference_khz(24_000), Ok(24_000));
        assert_eq!(wrpll_reference_khz(19_200), Ok(19_200));
        assert_eq!(
            wrpll_reference_khz(0),
            Err(PllError::ZeroReference)
        );
        assert_eq!(
            wrpll_reference_khz(100_000),
            Err(PllError::UnsupportedReference { ref_khz: 100_000 })
        );

        // The divider set does not depend on which of the two strapped forms
        // the caller names, because both divide to the same reference.
        let at_38_4 = ddi_pll_dividers(148_500, 38_400, ComboPhy::A).unwrap();
        let at_19_2 = ddi_pll_dividers(148_500, 19_200, ComboPhy::A).unwrap();
        assert_eq!(at_38_4.total_divider(), at_19_2.total_divider());
        assert_eq!(at_38_4.central_freq_khz(), at_19_2.central_freq_khz());
        assert_eq!(at_38_4.wrpll_ref_khz(), 19_200);
        assert_eq!(at_38_4.platform_ref_khz(), 38_400);
        // But the register value is genuinely different: DCO_INTEGER is
        // `DCO / 19.2 MHz`, not `DCO / 38.4 MHz`, so it is twice what a
        // caller who used the platform reference would compute.
        assert_eq!(at_38_4.dco_integer(), 464, "8910 MHz / 19.2 MHz");
        assert_eq!(at_19_2.dco_integer(), 464);
        // 8910 MHz is 464 * 19.2 MHz plus 1.2 MHz, and 1.2/19.2 is 1/16, so the
        // fraction is 0x800 rather than the 0x2000 a 24 MHz reference gives.
        assert_eq!(at_38_4.dco_fraction(), 0x800);
        assert_eq!(
            at_38_4.cfgcr0(DcoFractionWorkaround::HalveFraction),
            464 | (0x800 / 2) << 10,
            "the fraction is halved for this workaround"
        );
        // The two references agree on the DCO and the dividers, and disagree
        // about nothing except how the DCO is spelled in the register.
        assert_eq!(at_38_4.target_dco_khz(), at_19_2.target_dco_khz());
        assert_eq!(at_38_4.achieved_dco_hz(), at_19_2.achieved_dco_hz());
    }

    /// The fraction workaround is a property of the platform and the reference,
    /// and the module refuses to assume it.
    #[test]
    fn the_fraction_workaround_is_selected_by_the_reference_not_assumed() {
        assert_eq!(
            DcoFractionWorkaround::for_adl_p_n(38_400),
            DcoFractionWorkaround::HalveFraction
        );
        assert_eq!(
            DcoFractionWorkaround::for_adl_p_n(24_000),
            DcoFractionWorkaround::NotNeeded
        );
        assert_eq!(
            DcoFractionWorkaround::for_adl_p_n(19_200),
            DcoFractionWorkaround::NotNeeded
        );

        let dividers = ddi_pll_dividers(148_500, 38_400, ComboPhy::A).unwrap();
        let plain = dividers.cfgcr0(DcoFractionWorkaround::NotNeeded);
        let halved = dividers.cfgcr0(DcoFractionWorkaround::HalveFraction);
        assert_eq!(plain & 0x3ff, halved & 0x3ff, "the integer is untouched");
        assert_eq!((plain >> 10) & 0x7fff, 0x800);
        assert_eq!((halved >> 10) & 0x7fff, 0x400);
        // Halving and doubling round-trips through the decoder, which is how
        // i915 keeps its own read-back self-consistent.
        let registers = dividers
            .registers(PllFieldEncoding::Named, DcoFractionWorkaround::HalveFraction)
            .unwrap();
        assert_eq!(
            registers
                .symbol_rate_hz(38_400, PllFieldEncoding::Named, DcoFractionWorkaround::HalveFraction)
                .unwrap(),
            148_500_000
        );
    }

    /// `SKL_DSSM`'s three defined values, and the refusal of the five that are
    /// not defined.
    #[test]
    fn the_dssm_reference_field_decodes_only_what_sources_define() {
        assert_eq!(dssm_reference_khz(0), Ok(24_000));
        assert_eq!(dssm_reference_khz(1 << 29), Ok(19_200));
        assert_eq!(dssm_reference_khz(2 << 29), Ok(38_400));
        // Unrelated bits in the register must not change the answer.
        assert_eq!(dssm_reference_khz(0x1fff_ffff), Ok(24_000));
        // Unrelated bits set alongside a defined field must not change it.
        assert_eq!(dssm_reference_khz(0x1fff_ffff | (1 << 29)), Ok(19_200));
        for field in 3..8u32 {
            assert_eq!(
                dssm_reference_khz(field << 29),
                Err(PllError::UnsupportedDssmReference { field }),
                "field {field} is not defined by any source"
            );
        }
    }

    // -- boundaries ----------------------------------------------------------

    /// Below the smallest DCO the PLL can make, and above the largest, the
    /// answer is an error and not the nearest divider.
    #[test]
    fn a_pixel_clock_outside_the_plls_range_is_refused() {
        // The total divider is at least 3 and at most 98, and the DCO must land
        // inside [7896, 9696] MHz, so the symbol rate has to be in
        // [7896/(98*5), 9696/(3*5)] = [16.11, 646.4] MHz.
        let below = ddi_pll_dividers(10_000, REF_24, ComboPhy::A);
        assert_eq!(
            below,
            Err(PllError::NoLegalDividerSet {
                symbol_rate_khz: 10_000,
                ref_khz: REF_24,
            })
        );
        let above = ddi_pll_dividers(700_000, REF_24, ComboPhy::A);
        assert_eq!(
            above,
            Err(PllError::NoLegalDividerSet {
                symbol_rate_khz: 700_000,
                ref_khz: REF_24,
            })
        );
        // Immediately inside both ends there is an answer.
        assert!(ddi_pll_dividers(20_000, REF_24, ComboPhy::A).is_ok());
        assert!(ddi_pll_dividers(600_000, REF_24, ComboPhy::A).is_ok());
        assert_eq!(
            ddi_pll_dividers(0, REF_24, ComboPhy::A),
            Err(PllError::ZeroSymbolRate)
        );
    }

    /// The smallest and largest total dividers the lists contain are both
    /// reachable, and they are the ones the arithmetic says they are.
    #[test]
    fn the_smallest_and_largest_legal_dividers_are_reachable() {
        // 640 MHz: afe_clock 3200 MHz, and 3 * 3200 = 9600 MHz exactly.  No
        // even divider is within tolerance, so the odd list is reached.
        let smallest = hdmi(640_000);
        assert_eq!(smallest.total_divider(), 3);
        assert_eq!(smallest.target_dco_khz(), 9_600_000);
        assert_eq!(smallest.deviation_centipercent(), 0);
        assert_eq!((smallest.p(), smallest.q(), smallest.k()), (3, 1, 1));

        // 18.45 MHz: afe_clock 92.25 MHz, and 98 * 92.25 = 9040.5 MHz, which is
        // 0.45% above the 9000 MHz centre.  An even divider wins.
        let largest = hdmi(18_450);
        assert_eq!(largest.total_divider(), 98);
        assert_eq!(largest.target_dco_khz(), 9_040_500);
        assert_eq!(largest.central_freq_khz(), 9_000_000);
        assert_eq!(largest.deviation_centipercent(), 45);
        assert_eq!((largest.p(), largest.q(), largest.k()), (7, 7, 2));
        // P = 7 is the other half of the encoding gap, so the two encodings
        // give different register values for the same divider set.
        let executed = largest.cfgcr1(PllFieldEncoding::Executed).unwrap();
        let named = largest.cfgcr1(PllFieldEncoding::Named).unwrap();
        assert_eq!((executed >> 2) & 0xf, 4);
        assert_eq!((named >> 2) & 0xf, 8);
    }

    // -- properties ----------------------------------------------------------

    /// Sweep the whole legal range and check the module's own invariants on
    /// every answer: the dividers multiply out, the encodings agree with the
    /// divider values they claim, and the rate is right.
    #[test]
    fn every_answer_satisfies_the_invariants() {
        let mut found = 0u32;
        for symbol_rate_khz in (17_000..=640_000).step_by(250) {
            let Ok(dividers) = ddi_pll_dividers(symbol_rate_khz, REF_24, ComboPhy::B) else {
                continue;
            };
            found += 1;

            // The dividers multiply back to the total.
            assert_eq!(
                dividers.p() * dividers.q() * dividers.k(),
                dividers.total_divider(),
                "{symbol_rate_khz} kHz"
            );
            // The total divider is one the lists actually contain.
            assert!(
                EVEN_TOTAL_DIVIDERS.contains(&dividers.total_divider())
                    || ODD_TOTAL_DIVIDERS.contains(&dividers.total_divider()),
                "{symbol_rate_khz} kHz: {} is not a candidate",
                dividers.total_divider()
            );
            // The target DCO is exactly the definition.
            assert_eq!(
                dividers.target_dco_khz(),
                5 * u64::from(symbol_rate_khz) * u64::from(dividers.total_divider()),
                "{symbol_rate_khz} kHz"
            );
            // The declared error matches the registers.
            assert!(
                dividers.rate_error_ppb().unsigned_abs() <= MAX_SYMBOL_RATE_ERROR_PPB,
                "{symbol_rate_khz} kHz: {} ppb",
                dividers.rate_error_ppb()
            );
            assert_eq!(
                dividers.achieved_symbol_rate_hz(),
                dividers.achieved_dco_hz() / (5 * u64::from(dividers.total_divider())),
                "{symbol_rate_khz} kHz"
            );
            // The deviation is inside one of the two documented limits.
            let deviation = dividers.deviation_centipercent();
            if dividers.target_dco_khz() >= dividers.central_freq_khz() {
                assert!(deviation < DCO_MAX_POSITIVE_DEVIATION, "{symbol_rate_khz} kHz");
            } else {
                assert!(deviation < DCO_MAX_NEGATIVE_DEVIATION, "{symbol_rate_khz} kHz");
            }
            // Every encoding that accepts the set round-trips through its own
            // decode.
            for encoding in [PllFieldEncoding::Executed, PllFieldEncoding::Named] {
                let Ok(registers) = dividers.registers(encoding, DcoFractionWorkaround::NotNeeded)
                else {
                    continue;
                };
                let decoded = registers
                    .symbol_rate_hz(REF_24, encoding, DcoFractionWorkaround::NotNeeded)
                    .unwrap_or_else(|error| panic!("{symbol_rate_khz} kHz: {error}"));
                // The round trip is exact to the fraction's resolution, which
                // is a relative error, not an absolute one: 93 ppb at the
                // smallest legal DCO is 60 Hz at a 640 MHz symbol rate.
                let requested_hz = u64::from(symbol_rate_khz) * 1000;
                let error_ppb = (decoded as i128 - requested_hz as i128) * 1_000_000_000i128
                    / requested_hz as i128;
                assert!(
                    error_ppb.abs() <= MAX_SYMBOL_RATE_ERROR_PPB as i128,
                    "{symbol_rate_khz} kHz decoded to {decoded} Hz under {encoding} \
                     ({error_ppb} ppb)"
                );
            }
        }
        // The sweep must actually have covered most of the range, or the test
        // proves nothing.
        assert!(found > 2_000, "only {found} rates in the range were solvable");
    }

    /// The reference frequency does not change which divider set is chosen --
    /// the DCO does not depend on it -- but it does change the registers, and
    /// every sourced reference must produce a self-consistent answer.
    #[test]
    fn every_sourced_reference_gives_a_self_consistent_answer() {
        for ref_khz in [19_200u32, 24_000, 38_400] {
            let dividers = ddi_pll_dividers(148_500, ref_khz, ComboPhy::A).unwrap();
            assert_eq!(dividers.total_divider(), 12, "reference {ref_khz} kHz");
            assert_eq!(dividers.wrpll_ref_khz(), wrpll_reference_khz(ref_khz).unwrap());
            let registers = dividers
                .registers(PllFieldEncoding::Named, DcoFractionWorkaround::NotNeeded)
                .unwrap();
            // DCO_INTEGER is `DCO / wrpll_ref`, so the three references give
            // three different integers for the same DCO.
            assert_eq!(
                dividers.dco_integer(),
                (8_910_000_000u64 / (u64::from(dividers.wrpll_ref_khz()) * 1000)) as u32,
                "reference {ref_khz} kHz"
            );
            // The integer is what CFGCR0's low ten bits carry, and it fits.
            assert_eq!(registers.cfgcr0 & 0x3ff, dividers.dco_integer());
            assert!(dividers.dco_integer() <= 0x3ff, "reference {ref_khz} kHz");
            // The fraction is what the field above it carries.
            assert_eq!(
                (registers.cfgcr0 >> 10) & 0x7fff,
                dividers.dco_fraction(),
                "reference {ref_khz} kHz"
            );
            // Whatever the reference, the register pair decodes back to the
            // rate that was asked for.
            assert_eq!(
                registers
                    .symbol_rate_hz(ref_khz, PllFieldEncoding::Named, DcoFractionWorkaround::NotNeeded)
                    .unwrap(),
                148_500_000,
                "reference {ref_khz} kHz"
            );
        }
    }

    /// A reference that is not one of the three is refused by the entry point
    /// too, not only by the helper.
    #[test]
    fn an_unsourced_reference_is_refused_at_the_entry_point() {
        assert_eq!(
            ddi_pll_dividers(148_500, 100_000, ComboPhy::A),
            Err(PllError::UnsupportedReference { ref_khz: 100_000 })
        );
        assert_eq!(
            ddi_pll_dividers(148_500, 0, ComboPhy::A),
            Err(PllError::ZeroReference)
        );
    }

    // -- routing -------------------------------------------------------------

    /// The DPLL index and the `ICL_DPCLKA_CFGCR0` fields a modeset needs.
    #[test]
    fn the_phy_selects_its_dpll_and_its_routing_field() {
        assert_eq!(ComboPhy::A.dpll_index(), 0);
        assert_eq!(ComboPhy::B.dpll_index(), 1);
        // `DDI_CLK_SEL_SHIFT(phy) = phy * 2`, value is the PLL id.
        assert_eq!(ComboPhy::A.ddi_clock_select_shift(), 0);
        assert_eq!(ComboPhy::B.ddi_clock_select_shift(), 2);
        assert_eq!(ComboPhy::A.ddi_clock_select(), 0);
        assert_eq!(ComboPhy::B.ddi_clock_select(), 1 << 2);
        // `DDI_CLK_OFF` is bit 10 for PHY A and 11 for PHY B.
        assert_eq!(ComboPhy::A.ddi_clock_off_bit(), 1 << 10);
        assert_eq!(ComboPhy::B.ddi_clock_off_bit(), 1 << 11);
        // The two fields do not overlap, and the clock-off clear is a separate
        // write from the clock-select write.
        assert_eq!(
            ComboPhy::B.ddi_clock_select() & ComboPhy::B.ddi_clock_off_bit(),
            0
        );
    }

    // -- the PRM's own bounds ------------------------------------------------

    /// The PRM's `(P, Q, K)` bounds, checked against the totals i915's lists
    /// contain.  Some totals have no legal decomposition at all, which is a
    /// real disagreement between the two sources rather than a bug here.
    #[test]
    fn the_prm_bounds_do_not_cover_every_divider_i915_uses() {
        // Totals both sources can express.
        assert_eq!(prm_legal_divider_set(3), Some((3, 1, 1)));
        assert_eq!(prm_legal_divider_set(5), Some((5, 1, 1)));
        assert_eq!(prm_legal_divider_set(12), Some((2, 3, 2)));
        assert_eq!(prm_legal_divider_set(24), Some((2, 6, 2)));
        assert_eq!(prm_legal_divider_set(76), Some((2, 19, 2)));
        // A total i915 decomposes to K = 5, which the PRM does not define, but
        // which the PRM can reach another way.
        assert_eq!(prm_legal_divider_set(10), Some((5, 1, 2)));
        assert_eq!(prm_legal_divider_set(15), Some((5, 1, 3)));
        // The PRM cannot express 35 at all: 35 = 5 * 7 needs Q = 7 with K = 1,
        // and K != 2 forces Q = 1.
        assert_eq!(prm_legal_divider_set(35), None);
        assert!(ODD_TOTAL_DIVIDERS.contains(&35));
        assert_eq!(decompose(35), Some((7, 1, 5)));
        // Whatever it returns must satisfy the bounds it claims.
        for total in 3u32..=98 {
            let Some((p, q, k)) = prm_legal_divider_set(total) else {
                continue;
            };
            assert_eq!(p * q * k, total);
            assert!([2, 3, 5, 7].contains(&p), "total {total}: P = {p}");
            assert!([1, 2, 3].contains(&k), "total {total}: K = {k}");
            assert!(q >= 1 && q <= 255, "total {total}: Q = {q}");
            assert!(k == 2 || q == 1, "total {total}: K = {k} requires Q = 1");
        }
    }

    // -- errors --------------------------------------------------------------

    /// Every error has something a log reader can act on.
    #[test]
    fn errors_describe_themselves() {
        use alloc::string::ToString;
        let errors = [
            PllError::ZeroSymbolRate,
            PllError::ZeroReference,
            PllError::UnsupportedReference { ref_khz: 100_000 },
            PllError::NoLegalDividerSet {
                symbol_rate_khz: 10_000,
                ref_khz: REF_24,
            },
            PllError::DividerNotDecomposable { total_divider: 11 },
            PllError::DividerNotEncodable {
                p: 5,
                k: 0,
                field: PllDividerField::Post,
                encoding: PllFieldEncoding::Executed,
            },
            PllError::FieldOverflow {
                field: PllDividerField::DcoInteger,
                value: 4096,
                bits: 10,
            },
            PllError::AchievedRateOutOfTolerance {
                error_ppb: 5_000,
                limit_ppb: 1_000,
            },
            PllError::UnsupportedDssmReference { field: 5 },
        ];
        for error in errors {
            let text = error.to_string();
            assert!(text.len() > 20, "{error:?} rendered as {text:?}");
            assert!(!text.contains("Err("), "{error:?} rendered as {text:?}");
        }
    }

    /// The `Debug` of a divider set is the log line the reference document
    /// §11 phase 3.3 asks for before anything is written.
    #[test]
    fn a_divider_set_renders_a_usable_log_line() {
        use alloc::format;
        let text = format!("{:?}", hdmi(148_500));
        // The derived `Debug` prints the variant, not the enum path, so this
        // also pins that the field is the PHY and not something else.
        for expected in [
            "phy: A",
            "platform_ref_khz: 24000",
            "wrpll_ref_khz: 24000",
            "total_divider: 12",
            "p: 2",
            "q: 3",
            "k: 2",
            "central_freq_khz: 9000000",
            "target_dco_khz: 8910000",
            "dco_integer: 371",
            "dco_fraction: 8192",
            "rate_error_ppb: 0",
        ] {
            assert!(text.contains(expected), "{expected} missing from {text}");
        }
    }
}
