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
//! # Which generation's search this is
//!
//! **This module implements the ADL-N search, `icl_calc_wrpll`, not the
//! Skylake one.**  That is a change: until `fix/intel-pll-adln` it implemented
//! `skl_ddi_calculate_wrpll`, because that was the loop the reference document
//! had extracted first, and ADL-N does not use it at all (`[REF]` §6.3
//! corrections 1 and 2).  The two are different functions with different
//! answers, and the difference was measured before it was fixed: over 985
//! symbol rates from 16 to 1000 MHz in 1 MHz steps, 114 of the 574 rates both
//! searches could reach got a different total divider, and 12 of those put the
//! DCO *outside* the PRM's `[7998, 10000] MHz` window -- a PLL that never
//! locks, which the monitor shows as no signal.  The measurement is re-derived
//! and pinned in
//! `output::tests::pll_rs_search_is_measured_against_the_documented_adl_n_search`
//! and recorded in `docs/design/intel-pll.md` §3.3.
//!
//! What the Skylake path did, and the ADL-N path does not:
//!
//! * three discrete central frequencies `{8400, 9000, 9600} MHz` with an
//!   asymmetric `+1%`/`-6%` tolerance, instead of one window
//!   `[7998, 10000] MHz` and one midpoint, 8999 MHz;
//! * a `min_deviation == 0` early exit, instead of testing every candidate;
//! * an even-divider list that wins outright if anything in it is accepted, so
//!   an even divider 5.95% off its centre beat an exact odd one.  The ADL-N
//!   list is flat, the comparison is strict `<`, and the first entry in list
//!   order achieving the minimum distance from the midpoint wins;
//! * a total divider of 35 in the odd list, which no PRM-legal `(P, Q, K)`
//!   decomposes.  The ADL-N list stops at 21 and contains no 35.
//!
//! # Where the facts come from
//!
//! `docs/design/intel-display-registers.md` §6.3 is the reference, and its
//! "The search -- now resolved from the implementation" is the transcription
//! this module follows: `icl_calc_wrpll` (the window, the flat divider list and
//! the midpoint), `icl_wrpll_get_multipliers` (the `(P, Q, K)` decomposition),
//! the Gen12 `PDIV`/`KDIV` field positions and `icl_wrpll_ref_clock`.
//!
//! The source behind that transcription is the only one in the reference's set
//! that targets **display version 13**, which is what an Alder Lake-N part is:
//! Linux v6.12 `drm/i915` (tag `v6.12`, commit
//! `adc218676eef25575469234709c2d87185ca223a`), file
//! `drivers/gpu/drm/i915/display/intel_dpll_mgr.c`, with register definitions
//! in `drivers/gpu/drm/i915/i915_reg.h`.  Facts are taken from those files --
//! symbols and line numbers are cited throughout -- and no code is copied.
//!
//! §13.1 item 11 used to list the search loop as a `[GAP]` ("I did not extract
//! the exact loop, tolerance and tie-breaking rules from
//! `skl_ddi_calculate_wrpll`").  It is closed: the loop ADL-N uses is
//! `icl_calc_wrpll`, and §6.3 carries it in full.
//!
//! One question the reference raises and this module still refuses to settle is
//! [`PllFieldEncoding`] -- two encoders that disagree about what `CFGCR1`'s
//! `PDIV` and `KDIV` codes mean.  See "the encoding gap" below.
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
//! and the search is over the integer `P * Q * K`, keeping the DCO inside
//! `[7998, 10000] MHz` and as close to that window's 8999 MHz midpoint as the
//! candidate list allows.  The doc's own worked example is 1080p60 over HDMI:
//! the TMDS character rate is 148.5 MHz, so `afe_clock` is 742.5 MHz and a
//! total divider of 12 puts the DCO at 8910 MHz, 89 MHz below the midpoint.
//! It is also the only in-window candidate at that rate, so the midpoint rule
//! is not what picks it there -- see the worked-example test.
//!
//! # The encoding gap
//!
//! `CFGCR1` holds `P` and `K` as small codes, and this module offers **two**
//! accounts of what those codes are, as [`PllFieldEncoding`], with no default.
//! They are not two readings of one generation; they are two generations'
//! encoders that share a struct in i915:
//!
//! * [`PllFieldEncoding::Named`] -- what i915's Gen12 constants say, and what
//!   ADL-N's own encoder writes.  `i915_reg.h` defines `DPLL_CFGCR1_PDIV_{2,3,5,7}`
//!   as `{1,2,4,8} << 2` (`i915_reg.h:4293-4296`) and `DPLL_CFGCR1_KDIV_{1,2,3}`
//!   as `{1,2,4} << 6` (`i915_reg.h:4287-4289`), matching the PRM's
//!   `P ∈ {2,3,5,7}`, `K ∈ {1,2,3}`.  `icl_wrpll_params_populate`
//!   (`intel_dpll_mgr.c:2546-2570`) emits exactly those values, and the read
//!   path `icl_ddi_combo_pll_get_freq` decodes with the same constants, so on
//!   this platform write and read round-trip.
//! * [`PllFieldEncoding::Executed`] -- the codes `skl_wrpll_params_populate`
//!   writes (`intel_dpll_mgr.c:1592-1658`), which are the *Skylake*
//!   `DPLL_CFGCR2` convention: `P` over `{1,2,3,7}` and `K` over `{5,2,3,1}`.
//!   ADL-N does not call that function.  The variant is kept because it is the
//!   other convention the reference records and because §6.3's "trap" section
//!   is about telling the two apart: they write the same `struct
//!   skl_wrpll_params`, so nothing in i915's source stops one generation's
//!   encoder being handed to the other's decoder.
//!
//! The two disagree on **every** `K` and on `P = 7`.  This module therefore
//! refuses to choose: there is no `Default`, and a divider set with no code
//! under the requested encoding returns [`PllError::DividerNotEncodable`] rather
//! than a value nobody can justify.  That refusal is live rather than
//! theoretical: the ADL-N decomposition reaches `P = 5` (a 360 MHz symbol rate
//! puts the odd divider 5 exactly on 9000 MHz), which `Named` codes as `4 << 2`
//! and `Executed` has no case for at all -- i915's Skylake encoder hits
//! `default: WARN(1, "Incorrect PDiv")` and silently leaves a zero-initialised
//! `pdiv`, which is `P = 1`.
//!
//! Which one a caller should write is a hardware question.  `[REF]` §6.3's
//! route 2 says to mirror `icl_wrpll_get_multipliers` + `icl_wrpll_params_populate`
//! -- the `Named` pair -- and **verify by read-back** before enabling, and
//! `PllRegisters::symbol_rate_hz` is that read-back.
//!
//! # What has not been checked
//!
//! No value from this module has been written to, or read from, real silicon.
//! The arithmetic is checked against i915 v6.12's own and against the published
//! modelines, but that is a host test against a published reference, not a
//! measurement of a PLL.  The reference document's §13.4 recommendation stands:
//! read `DPLL0_CFGCR0` and `DPLL0_CFGCR1` from a firmware-programmed working
//! mode and diff them against what this module computes for the same mode
//! before trusting either.
//!
//! The search change on `fix/intel-pll-adln` is a change of *algorithm*, taken
//! from the reference's transcription and checked against i915 v6.12's
//! `icl_calc_wrpll` and `icl_wrpll_get_multipliers` in that file.  It is not
//! evidence that the ADL-N PLL locks where this module now aims it; that is
//! what §13.4's read-back is for.

use core::fmt;

/// The PRM's DCO window, in kHz.
///
/// `docs/design/intel-display-registers.md` §6.3: "DCO ∈ [7998 MHz, 10000 MHz],
/// midpoint 8999 MHz", quoted from `[TGL12]` Clocks → combo PHY PLL.  i915's
/// `icl_calc_wrpll` carries the same two numbers as `dco_min = 7998000` and
/// `dco_max = 10000000` (`display/intel_dpll_mgr.c:2785-2786`), so the PRM and
/// the implementation agree and there is nothing to choose between them.
///
/// **The search enforces this window.**  A candidate whose DCO falls outside it
/// is rejected outright, so no divider set this module returns can put the DCO
/// where the PLL does not lock.  Before `fix/intel-pll-adln` these two
/// constants were only reported, and the Skylake search returned 12 divisible
/// rates out of 985 whose DCO they do not cover.
pub(crate) const PRM_DCO_MIN_KHZ: u64 = 7_998_000;

/// The upper end of the PRM's DCO window.  See [`PRM_DCO_MIN_KHZ`].
pub(crate) const PRM_DCO_MAX_KHZ: u64 = 10_000_000;

/// The lower end of the search's window, in Hz.
const DCO_MIN_HZ: u64 = PRM_DCO_MIN_KHZ * 1000;

/// The upper end of the search's window, in Hz.
const DCO_MAX_HZ: u64 = PRM_DCO_MAX_KHZ * 1000;

/// The DCO the search aims at, in Hz.
///
/// `[I915]` `icl_calc_wrpll` (`display/intel_dpll_mgr.c:2787`):
/// `dco_mid = (dco_min + dco_max) / 2`, so `(7998 + 10000) / 2 = 8999` MHz --
/// the PRM's "midpoint 8999".  The Skylake path this module used to implement
/// had three discrete central frequencies instead; §6.3: "**ADL-N takes the
/// midpoint form, not the three-frequency form.**"
const DCO_MIDPOINT_HZ: u64 = (PRM_DCO_MIN_KHZ + PRM_DCO_MAX_KHZ) / 2 * 1000;

/// The total dividers `icl_calc_wrpll` tries, in the order it tries them.
///
/// `[I915]` `icl_calc_wrpll` (`display/intel_dpll_mgr.c:2788-2793`), which §6.3
/// transcribes.  It is a **single flat list** -- the forty even entries first,
/// then the six odd ones -- and not the Skylake `even_dividers` +
/// `odd_dividers` pair: it starts at 2 where the Skylake list starts at 4, and
/// it stops at 21, so the Skylake-only total divider 35 -- the one with no
/// PRM-legal `(P, Q, K)` -- is not in it.
///
/// The order is behaviour and not presentation: the comparison in
/// [`search_total_divider`] is strict, so the first entry in this order
/// achieving the minimum distance from the midpoint wins.
///
/// Every entry decomposes under [`icl_wrpll_get_multipliers`], and every
/// decomposition satisfies the PRM's `P`/`Q`/`K` bounds.  The
/// `every_candidate_divider_decomposes` test proves both rather than assuming
/// them.
const ADL_N_TOTAL_DIVIDERS: &[u32] = &[
    2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40, 42, 44, 48, 50, 52, 54, 56, 60, 64,
    66, 68, 70, 72, 76, 78, 80, 84, 88, 90, 92, 96, 98, 100, 102, 3, 5, 7, 9, 15, 21,
];

/// How far the achieved symbol rate may sit from the requested one, in parts
/// per billion, before the search refuses its own answer.
///
/// The divider set does not introduce any error at all: the DCO is
/// `afe_clock * P * Q * K` by construction, so dividing it back by
/// `5 * P * Q * K` returns the requested symbol rate exactly.  The only error
/// is the quantisation of `DCO_FRACTION`, which has 15 bits and therefore
/// steps by `ref / 0x8000`: 24 MHz / 32768 = 732 Hz at the largest reference
/// this part uses, against a DCO of at least 7998 MHz, which is under 92 parts
/// per billion.  1000 ppb (1 ppm) is ten times that bound, so a solution that
/// trips it is a bug in this module rather than a hardware limit -- which is
/// the point of checking.
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

/// Which of the two `CFGCR1` field encodings to write.
///
/// There is deliberately no `Default`.  Choosing one silently is the failure
/// this type exists to prevent; see the module documentation's "the encoding
/// gap", and `docs/design/intel-pll.md` for the full argument.
///
/// These are not two readings of one generation's encoder: they are the
/// **Skylake** encoder's codes and the **Gen12** encoder's codes, which i915
/// keeps in two functions that write the same struct.  ADL-N runs the Gen12
/// one, so [`Self::Named`] is what this platform's i915 writes; [`Self::Executed`]
/// is retained because it is the other sourced convention and telling them
/// apart is the exercise `[REF]` §6.3's "trap" section sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PllFieldEncoding {
    /// The codes `skl_wrpll_params_populate` writes
    /// (`intel_dpll_mgr.c:1611-1643`): `P` over `{1,2,3,7}` and `K` over
    /// `{5,2,3,1}`.  These are the Skylake `DPLL_CFGCR2` codes, and ADL-N does
    /// not go through this function at all.
    Executed,
    /// The codes i915's Gen12 named constants define (`i915_reg.h:4287-4296`):
    /// `P` over `{2,3,5,7}` and `K` over `{1,2,3}`, which are the value sets
    /// the PRM states.  `icl_wrpll_params_populate` (`intel_dpll_mgr.c:2546-2570`)
    /// emits exactly these, and i915's own read-back path,
    /// `icl_ddi_combo_pll_get_freq`, decodes with them, so this is the
    /// convention the ADL-N path writes and reads.
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
    /// No divider in the sourced candidate list put the DCO inside the PRM's
    /// documented window.
    ///
    /// This is the honest answer for a pixel clock the PLL cannot make, and it
    /// is deliberately an error rather than the nearest divider: a silently
    /// wrong pixel clock is a monitor that shows nothing.
    NoLegalDividerSet { symbol_rate_khz: u32, ref_khz: u32 },
    /// A divider i915's list contains could not be decomposed into `(P, Q, K)`.
    ///
    /// `icl_wrpll_get_multipliers` leaves its outputs untouched for a divider
    /// no branch matches, which the caller zero-initialised; i915 then programs
    /// a zero.  Every entry of the list does decompose (the
    /// `every_candidate_divider_decomposes` test checks exactly that), so this
    /// is a guard against a list being edited, not a reachable state.
    DividerNotDecomposable { total_divider: u32 },
    /// The chosen divider set has no code in the requested field encoding.
    ///
    /// This is the encoding question of §6.3 surfaced as a value rather than
    /// hidden: `P = 5` is reachable from the ADL-N search and has no code in
    /// the Skylake `Executed` encoder (i915 warns "Incorrect PDiv" and programs
    /// whatever `pdiv` was left as), and `K = 5` -- which the Skylake
    /// decomposition produced and the ADL-N one cannot -- has no `Named` code
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
                "a reference of {ref_khz} kHz is not one a Gen12 combo PHY PLL is strapped to (24 \
                 MHz, 19.2 MHz and 38.4 MHz are the ones i915 decodes from SKL_DSSM)"
            ),
            Self::NoLegalDividerSet {
                symbol_rate_khz,
                ref_khz,
            } => write!(
                f,
                "no divider in the sourced candidate list puts the DCO inside the PRM's [7998, \
                 10000] MHz window for a {symbol_rate_khz} kHz symbol rate at a {ref_khz} kHz \
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
                 encoding; these are the two encoders §6.3 warns share one struct, and which one \
                 the hardware implements must be settled by read-back, not guessed"
            ),
            Self::FieldOverflow { field, value, bits } => write!(
                f,
                "{field:?} value {value} does not fit its {bits}-bit register field"
            ),
            Self::AchievedRateOutOfTolerance {
                error_ppb,
                limit_ppb,
            } => write!(
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

    /// The DCO the search aimed at, in kHz.
    ///
    /// **On the ADL-N path this is always 8999 MHz**, the midpoint of the PRM's
    /// `[7998, 10000] MHz` window (`[I915]` `icl_calc_wrpll`,
    /// `intel_dpll_mgr.c:2787`).  The name is the Skylake path's, which had
    /// three central frequencies `{8400, 9000, 9600} MHz` instead of one
    /// midpoint; §6.3 records that ADL-N takes the midpoint form.  A caller
    /// that wants the *chosen* frequency wants [`Self::target_dco_khz`]; this
    /// is the fixed point the choice was measured against.
    pub(crate) const fn central_freq_khz(&self) -> u64 {
        self.central_freq_khz
    }

    /// The DCO the divider set asks for, in kHz: `5 * symbol_rate * P * Q * K`.
    pub(crate) const fn target_dco_khz(&self) -> u64 {
        self.target_dco_khz
    }

    /// How far the chosen DCO sits from the midpoint above, in 0.01%.
    ///
    /// This is the quantity the ADL-N search minimises: i915 computes
    /// `dco_centrality = abs(dco - dco_mid)` (`intel_dpll_mgr.c:2802`) and
    /// keeps the smallest.  A larger figure is a DCO further from the middle of
    /// the band, not an out-of-tolerance one -- the window is enforced
    /// separately and cannot be violated.
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
    /// **This is now an invariant rather than a report.**  The ADL-N search
    /// rejects a candidate outside the window before it looks at the distance
    /// from the midpoint (`icl_calc_wrpll`'s `dco <= dco_max && dco >= dco_min`,
    /// `intel_dpll_mgr.c:2801`), so every divider set that exists at all is
    /// inside it, and a rate with no in-window candidate is
    /// [`PllError::NoLegalDividerSet`] instead.  The accessor is kept because
    /// the property is the whole point of the ADL-N search and the sweep test
    /// asserts it on every answer.
    ///
    /// The Skylake search this module used to implement did *not* have this
    /// property; see [`PRM_DCO_MIN_KHZ`].
    pub(crate) const fn inside_prm_dco_window(&self) -> bool {
        self.target_dco_khz >= PRM_DCO_MIN_KHZ && self.target_dco_khz <= PRM_DCO_MAX_KHZ
    }

    /// The `(P, Q, K)` that the PRM's own bounds allow for this total divider,
    /// if any.
    ///
    /// The PRM states `P ∈ {2,3,5,7}`, `K ∈ {1,2,3}`, `Q ∈ 1..255` and
    /// "`K != 2 ⇒ Q = 1`" (reference document §6.3, "The search bounds").
    /// `icl_wrpll_get_multipliers` satisfies all of them by construction, and
    /// the `every_candidate_divider_decomposes` test checks that this function
    /// agrees with it for every candidate divider -- so on the ADL-N path this
    /// is a *check* on the decomposition rather than a substitute for it.
    ///
    /// It did not always agree: the Skylake decomposition this module used to
    /// carry produced `K = 5` (which the PRM does not define) for the total
    /// divider 10, and the Skylake list contained 35, which has no PRM-legal
    /// decomposition at all.  `[REF]` §6.3's "Two more facts" and §13.1 item 11
    /// record why: neither the list nor the decomposition is ADL-N's.
    ///
    /// This is still a *report*, not a substitution: a caller that wants to
    /// know whether the divider it is about to program is PRM-legal asks this;
    /// a caller that wants what i915 would write asks [`Self::p`] and friends.
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
            DcoFractionWorkaround::HalveFraction => self.dco_fraction.div_ceil(2),
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
/// this needs is the pixel clock itself: `[I915]` `icl_calc_wrpll` takes
/// `crtc_state->port_clock` and sets `afe_clock = crtc_state->port_clock * 5`
/// (`intel_dpll_mgr.c:2784`), which is the same statement.  That is the case
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
/// wrapper over it.  The search is i915's `icl_calc_wrpll`
/// (`intel_dpll_mgr.c:2779-2820`) and the decomposition is
/// `icl_wrpll_get_multipliers` (`:2507-2543`), which is what ADL-N runs;
/// `docs/design/intel-display-registers.md` §6.3 transcribes both, and §13.1
/// item 11 -- the `[GAP]` that said the search loop had not been extracted -- is
/// closed by that transcription.
///
/// The returned error is the honest one: when no candidate puts the DCO inside
/// the PRM's window this says so, instead of returning the nearest divider and
/// a pixel clock nobody asked for.
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
    // DCO the search can return is the window's 10 GHz, which is well inside a
    // u64 and far outside the u32 a register field would hold -- which is why
    // `dco_integer` is range-checked before it is handed back.
    let afe_clock_hz = 5 * u64::from(symbol_rate_khz) * 1000;

    let chosen = search_total_divider(afe_clock_hz).ok_or(PllError::NoLegalDividerSet {
        symbol_rate_khz,
        ref_khz,
    })?;

    let (p, q, k) = icl_wrpll_get_multipliers(chosen.total_divider).ok_or(
        PllError::DividerNotDecomposable {
            total_divider: chosen.total_divider,
        },
    )?;

    let target_dco_hz = chosen.dco_hz;
    let ref_hz = u64::from(wrpll_ref_khz) * 1000;
    let dco_integer = target_dco_hz / ref_hz;
    let dco_fraction = ((target_dco_hz % ref_hz) * 0x8000) / ref_hz;

    let dco_integer = field_value(
        u32::try_from(dco_integer).unwrap_or(u32::MAX),
        10,
        PllDividerField::DcoInteger,
    )?;
    let dco_fraction = field_value(
        u32::try_from(dco_fraction).unwrap_or(u32::MAX),
        15,
        PllDividerField::DcoFraction,
    )?;

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
        central_freq_khz: DCO_MIDPOINT_HZ / 1000,
        target_dco_khz: target_dco_hz / 1000,
        deviation_centipercent: chosen.centrality_centipercent,
        dco_integer,
        dco_fraction,
        achieved_dco_hz,
        achieved_symbol_rate_hz,
        rate_error_ppb: error_ppb,
    })
}

/// A divider the search accepted, and the arithmetic behind it.
#[derive(Clone, Copy, Debug)]
struct Chosen {
    /// The total divider `P * Q * K`.
    total_divider: u32,
    /// The DCO that divider produces, in Hz: `total_divider * afe_clock`.
    dco_hz: u64,
    /// `|DCO - midpoint|`, in Hz.  This is the quantity the search minimises.
    centrality_hz: u64,
    /// The same distance as a fraction of the midpoint, in 0.01%.
    centrality_centipercent: u64,
}

/// i915's `icl_calc_wrpll` search (`intel_dpll_mgr.c:2779-2820`), which §6.3
/// transcribes and which is the loop ADL-N actually runs.
///
/// The loop structure is behaviour, and none of it is the Skylake loop:
///
/// * **every** candidate in the flat list is tested; there is no
///   `min_deviation == 0` early exit on this path, so the answer does not
///   depend on the order candidates are enumerated in beyond the tie-break
///   below;
/// * a candidate whose DCO is outside `[7998000, 10000000]` kHz is skipped
///   *before* its distance is considered
///   (`dco <= dco_max && dco >= dco_min`, `intel_dpll_mgr.c:2801`), so the
///   search cannot return an out-of-window DCO even when that DCO would be
///   closer to the midpoint -- which is why the returned error for a rate with
///   no in-window candidate is [`PllError::NoLegalDividerSet`] and not a
///   nearest-divider guess;
/// * the comparison is strict `<` against the best so far, which i915
///   initialises to `U32_MAX` with the comment "Spec meaning of 999999 MHz"
///   (`intel_dpll_mgr.c:2795`).  The **first** entry in list order achieving
///   the minimum distance therefore wins.  That is the smallest even divider on
///   a tie between evens, and the even one on a tie between an even and an odd
///   divider, because the evens come first in the list -- a genuine tie-break,
///   not the Skylake path's "any even divider beats every odd one".
///
/// The search is done in Hz throughout.  Mixing it with the kHz the rest of the
/// module reports in is how a distance comes out a thousand times too large and
/// every candidate is rejected, so the unit is in the name.
fn search_total_divider(afe_clock_hz: u64) -> Option<Chosen> {
    let mut best: Option<Chosen> = None;
    for &total_divider in ADL_N_TOTAL_DIVIDERS {
        let dco_hz = u64::from(total_divider) * afe_clock_hz;
        if !(DCO_MIN_HZ..=DCO_MAX_HZ).contains(&dco_hz) {
            continue;
        }
        let centrality_hz = dco_hz.abs_diff(DCO_MIDPOINT_HZ);
        if best.is_none_or(|best| centrality_hz < best.centrality_hz) {
            best = Some(Chosen {
                total_divider,
                dco_hz,
                centrality_hz,
                centrality_centipercent: 10_000 * centrality_hz / DCO_MIDPOINT_HZ,
            });
        }
    }
    best
}

/// i915's `icl_wrpll_get_multipliers` (`intel_dpll_mgr.c:2507-2543`), the
/// decomposition the ADL-N path uses.
///
/// The branch order is behaviour and not presentation: `%4` is tested before
/// `%6` before `%5` before `%14`, so the total divider 20 takes the `%4` branch
/// (`P = 2, Q = 5, K = 2`) and not `%5`.  Every branch yields `P ∈ {2,3,5,7}`
/// and `K ∈ {1,2,3}` and sets `Q = 1` whenever `K != 2`, which is the PRM's own
/// rule and i915's own assertion (`WARN_ON(kdiv != 2 && qdiv != 1)`,
/// `intel_dpll_mgr.c:2583`).  **No candidate the ADL-N search can choose
/// produces `K = 5`, and every one of them has a PRM-legal decomposition** --
/// both checked by the `every_candidate_divider_decomposes` test, and both
/// false of the Skylake decomposition this module used to carry.
///
/// # Divergences from i915, both unreachable from the candidate list
///
/// * If an even divider matches none of the five branches, i915 assigns
///   nothing, leaving the caller's zero-initialised outputs as `(0, 0, 0)`.
///   This returns `None`, and the caller turns that into
///   [`PllError::DividerNotDecomposable`] rather than a zero divider.
/// * i915's final `else` is commented `/* 9, 15, 21 */` but catches *any* other
///   odd divider, so it would answer `icl_wrpll_get_multipliers(11)` with
///   `P = 3, Q = 1, K = 3` -- a triple whose product is 9, not 11.  This
///   returns `None` for an odd divider that is not 3, 5, 7, 9, 15 or 21, so
///   [`ADL_N_TOTAL_DIVIDERS`] cannot silently acquire an entry that decomposes
///   to something else.  11 is not in that list and never was.
fn icl_wrpll_get_multipliers(total_divider: u32) -> Option<(u32, u32, u32)> {
    if total_divider.is_multiple_of(2) {
        if total_divider == 2 {
            Some((2, 1, 1))
        } else if total_divider.is_multiple_of(4) {
            Some((2, total_divider / 4, 2))
        } else if total_divider.is_multiple_of(6) {
            Some((3, total_divider / 6, 2))
        } else if total_divider.is_multiple_of(5) {
            Some((5, total_divider / 10, 2))
        } else if total_divider.is_multiple_of(14) {
            Some((7, total_divider / 14, 2))
        } else {
            None
        }
    } else if total_divider == 3 || total_divider == 5 || total_divider == 7 {
        Some((total_divider, 1, 1))
    } else if total_divider == 9 || total_divider == 15 || total_divider == 21 {
        Some((total_divider / 3, 1, 3))
    } else {
        None
    }
}

/// A `(P, Q, K)` satisfying the bounds the PRM states, for a total divider.
///
/// Reference document §6.3, "The search bounds": `P ∈ {2,3,5,7}`,
/// `K ∈ {1,2,3}`, `Q ∈ 1..255`, and `K != 2 ⇒ Q = 1`.  The search is over `K`
/// then `P` in ascending order, so the answer is deterministic but is *a* legal
/// triple rather than necessarily i915's: for a total divider the two
/// decompositions can differ, and both are legal.
///
/// On the ADL-N path this agrees with [`icl_wrpll_get_multipliers`] for every
/// candidate divider, which is §6.3's point that the ADL-N decomposition
/// satisfies the PRM by construction.  The Skylake decomposition this module
/// used to carry did not: `total = 10` came out as `(2, 1, 5)` with an illegal
/// `K`, and `total = 35` -- which the Skylake list contained and the ADL-N list
/// does not -- had no legal decomposition at all.
fn prm_legal_divider_set(total_divider: u32) -> Option<(u32, u32, u32)> {
    for k in [1u32, 2, 3] {
        for p in [2u32, 3, 5, 7] {
            let product = p * k;
            if !total_divider.is_multiple_of(product) {
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
const fn not_encodable(
    p: u32,
    k: u32,
    field: PllDividerField,
    encoding: PllFieldEncoding,
) -> PllError {
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
        Err(PllError::FieldOverflow { field, value, bits })
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
        // The aim point is the window's midpoint, 8999 MHz, not one of the
        // Skylake path's three central frequencies.
        assert_eq!(dividers.central_freq_khz(), 8_999_000);
        // 8910 MHz is 89 MHz below the midpoint: 10_000 * 89 / 8999 = 98.9, so
        // 98 in whole hundredths of a percent.
        assert_eq!(dividers.deviation_centipercent(), 98);
        // §6.3: `12 % 4 == 0 -> P = 2, Q = 12/4 = 3, K = 2`.  The `%4` branch
        // is tested before `%6`, which is why 12 is not `(3, 2, 2)`.
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (2, 3, 2));
        assert!(dividers.inside_prm_dco_window());
        // 8910 MHz is 24 MHz * 371.25, so the fraction is exactly a quarter and
        // the registers reproduce the rate exactly.
        assert_eq!(dividers.dco_integer(), 371);
        assert_eq!(dividers.dco_fraction(), 0x2000);
        assert_eq!(dividers.rate_error_ppb(), 0);
        assert_eq!(dividers.achieved_symbol_rate_hz(), 148_500_000);
    }

    /// The reference's worked example, worked all the way to register values.
    ///
    /// §6.3, "Worked example -- 1080p60, 148.5 MHz pixel clock, HDMI, 38.4 MHz
    /// strap" prints the chain and ends at `DPLL0_CFGCR0 = 0x001001D0` and
    /// `DPLL0_CFGCR1 = 0x00000E84`.  The register values are under the
    /// **named** encoding -- `icl_wrpll_params_populate`'s -- and the halved
    /// fraction (`0x800` before the workaround, `0x400` after it) is the ADL-P/N
    /// workaround at a 38.4 MHz strap, which is why this needs `HalveFraction`.
    ///
    /// Note what this test does *not* discriminate: 148.5 MHz is the one rate
    /// where the divergent case is easy -- the only in-window candidate is 12,
    /// so the Skylake search finds the same divider and the same
    /// `(P, Q, K)`.  The discriminating cases are
    /// `the_search_uses_the_prm_window_it_reports` and
    /// `the_adl_n_search_and_the_skylake_search_disagree_where_measured`.
    #[test]
    fn the_reference_worked_example_reproduces_the_published_registers() {
        let dividers = ddi_pll_dividers(148_500, 38_400, ComboPhy::A).expect("1080p60");
        assert_eq!(dividers.wrpll_ref_khz(), 19_200, "38.4 MHz is divided by 2");
        assert_eq!(dividers.total_divider(), 12);
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (2, 3, 2));
        assert_eq!(dividers.target_dco_khz(), 8_910_000);
        // `dco = (8_910_000 << 15) / 19_200 = 15_206_400`, so the integer is
        // `15_206_400 >> 15 = 464` (0x1D0) and the fraction is
        // `15_206_400 & 0x7FFF = 2048` (0x800).
        assert_eq!(dividers.dco_integer(), 464);
        assert_eq!(dividers.dco_fraction(), 0x800);
        assert_eq!(
            dividers
                .registers(
                    PllFieldEncoding::Named,
                    DcoFractionWorkaround::HalveFraction
                )
                .unwrap(),
            PllRegisters {
                // DCO_FRACTION[24:10] = 1024, DCO_INTEGER[9:0] = 464.
                cfgcr0: 0x0010_01D0,
                // QDIV_RATIO[17:10] = 3, QDIV_MODE[9] = 1, KDIV[8:6] = 2,
                // PDIV[5:2] = 1, CFSELOVRD[1:0] = 0.
                cfgcr1: 0x0000_0E84,
            }
        );
    }

    /// CTA-861 VIC 4: 1280x720@60, 74.25 MHz.  Half of VIC 16's clock, so the
    /// same DCO with twice the division.
    #[test]
    fn cta_vic_4_720p60_over_hdmi_doubles_the_divider() {
        let dividers = hdmi(74_250);
        assert_eq!(dividers.total_divider(), 24);
        assert_eq!(dividers.target_dco_khz(), 8_910_000);
        assert_eq!(dividers.central_freq_khz(), 8_999_000);
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (2, 6, 2));
        assert_eq!(dividers.achieved_symbol_rate_hz(), 74_250_000);
        assert_eq!(dividers.rate_error_ppb(), 0);
    }

    /// DMT 0x04: 640x480@60, 25.175 MHz.  The interesting one, because no
    /// candidate lands on the 8999 MHz midpoint: the search has to take the
    /// divider whose DCO is closest, and every other candidate is hundreds of
    /// megahertz further away.
    #[test]
    fn dmt_0x04_640x480_60_finds_an_off_centre_dco() {
        let dividers = hdmi(25_175);
        assert_eq!(dividers.total_divider(), 72);
        assert_eq!(dividers.target_dco_khz(), 9_063_000);
        assert_eq!(dividers.central_freq_khz(), 8_999_000);
        // 9063 MHz is 64 MHz above the 8999 MHz midpoint:
        // 10_000 * 64 / 8999 = 71.1, so 71 in whole hundredths of a percent.
        assert_eq!(dividers.deviation_centipercent(), 71);
        // 72 % 4 == 0 -> P = 2, Q = 18, K = 2.
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (2, 18, 2));
        assert!(dividers.inside_prm_dco_window());
        // 9063 MHz is 24 MHz * 377.625, and 0.625 * 0x8000 = 0x5000 exactly, so
        // the registers reproduce this rate exactly.  (Under the Skylake search
        // this mode took divider 76 at 9566.5 MHz and came out 1 Hz low; the
        // ADL-N midpoint rule takes 72, which is closer to 8999 MHz.)
        assert_eq!(dividers.dco_integer(), 377);
        assert_eq!(dividers.dco_fraction(), 0x5000);
        assert_eq!(dividers.achieved_symbol_rate_hz(), 25_175_000);
        assert_eq!(dividers.rate_error_ppb(), 0);
    }

    /// Every published modeline this module is asked about must produce a
    /// symbol rate that is right to well inside a part per million, and the
    /// register round trip must agree with the request.
    ///
    /// The total divider is pinned alongside the clock, because it is what moved
    /// when the search did: under the Skylake search these ten modes took
    /// `{24, 12, 12, 6, 3, 76, 42, 28, 14, 14}` and under the ADL-N search they
    /// take `{24, 12, 12, 6, 3, 72, 44, 28, 12, 16}`.  Four of the ten change.
    #[test]
    fn published_modelines_round_trip_through_their_registers() {
        // (name, pixel clock in kHz, total divider) -- CTA-861 VICs 4, 16, 31,
        // 93, 97 and VESA DMT codes 0x04, 0x09, 0x10, 0x52, 0x53.
        let modelines: &[(&str, u32, u32)] = &[
            ("CTA VIC 4 1280x720@60", 74_250, 24),
            ("CTA VIC 16 1920x1080@60", 148_500, 12),
            ("CTA VIC 31 1920x1080@50", 148_500, 12),
            ("CTA VIC 93 3840x2160@24", 297_000, 6),
            ("CTA VIC 97 3840x2160@60", 594_000, 3),
            ("DMT 0x04 640x480@60", 25_175, 72),
            ("DMT 0x09 800x600@60", 40_000, 44),
            ("DMT 0x10 1024x768@60", 65_000, 28),
            ("DMT 0x52 1920x1080@60 CVT-RB", 138_500, 12),
            ("DMT 0x53 1920x1080@50 CVT-RB", 115_500, 16),
        ];
        for &(name, clock_khz, expected_divider) in modelines {
            for encoding in [PllFieldEncoding::Executed, PllFieldEncoding::Named] {
                let dividers = hdmi(clock_khz);
                assert_eq!(
                    dividers.total_divider(),
                    expected_divider,
                    "{name}: total divider"
                );
                let Ok(registers) = dividers.registers(encoding, DcoFractionWorkaround::NotNeeded)
                else {
                    // A divider set with no code under this encoding is the
                    // §6.3 encoder split, and is asserted separately.
                    continue;
                };
                let decoded = registers
                    .symbol_rate_hz(REF_24, encoding, DcoFractionWorkaround::NotNeeded)
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                let requested_hz = u64::from(clock_khz) * 1000;
                let error_ppb = (decoded as i128 - requested_hz as i128) * 1_000_000_000i128
                    / requested_hz as i128;
                assert!(
                    error_ppb.abs() < 1_000,
                    "{name}: round trip produced {decoded} Hz for {requested_hz} Hz ({error_ppb} \
                     ppb) under {encoding}"
                );
            }
        }
    }

    // -- the search's documented rules --------------------------------------

    /// Every divider in the candidate list must decompose, to the divider it
    /// came from, into a `(P, Q, K)` the PRM allows.
    ///
    /// This is §6.3's "the PRM's rules are satisfied by construction": every
    /// branch of `icl_wrpll_get_multipliers` yields `P ∈ {2,3,5,7}` and
    /// `K ∈ {1,2,3}` and sets `Q = 1` whenever `K != 2`.  The test checks the
    /// product, the bounds, *and* that [`prm_legal_divider_set`] agrees -- the
    /// last is what makes the report function a check rather than a second
    /// opinion.
    #[test]
    fn every_candidate_divider_decomposes() {
        for &total_divider in ADL_N_TOTAL_DIVIDERS {
            let (p, q, k) = icl_wrpll_get_multipliers(total_divider)
                .unwrap_or_else(|| panic!("divider {total_divider} does not decompose"));
            assert_eq!(
                p * q * k,
                total_divider,
                "divider {total_divider} decomposed to ({p}, {q}, {k})"
            );
            assert!(
                [2, 3, 5, 7].contains(&p),
                "divider {total_divider}: P = {p}"
            );
            assert!([1, 2, 3].contains(&k), "divider {total_divider}: K = {k}");
            assert!(q >= 1 && q <= 255, "divider {total_divider}: Q = {q}");
            if k != 2 {
                assert_eq!(q, 1, "divider {total_divider}: K = {k} requires Q = 1");
            }
            assert_eq!(
                prm_legal_divider_set(total_divider),
                Some((p, q, k)),
                "divider {total_divider}: the PRM's own search disagrees with the decomposition"
            );
        }
        // The total divider 35 is the Skylake list's one extension past 21, and
        // it is the one with no PRM-legal decomposition at all: 35 = 5 * 7
        // needs Q = 7 with K = 1, and K != 2 forces Q = 1.  The ADL-N list
        // stops at 21 and does not contain it.
        assert!(!ADL_N_TOTAL_DIVIDERS.contains(&35));
        assert_eq!(prm_legal_divider_set(35), None);
        // A divider that is in neither list does not silently decompose.
        assert!(icl_wrpll_get_multipliers(11).is_none());
        assert!(icl_wrpll_get_multipliers(1).is_none());
        assert!(icl_wrpll_get_multipliers(49).is_none());
    }

    /// The midpoint rule takes the odd divider the Skylake path threw away.
    ///
    /// At a 200 MHz symbol rate `afe_clock` is 1000 MHz.  The ADL-N search
    /// tests both 8 (8000 MHz, 999 MHz from the midpoint) and 9 (9000 MHz,
    /// 1 MHz from it) and takes **9**, because it is the closest to 8999 MHz.
    /// The Skylake search took 8: it accepted an even divider 4.76% below its
    /// 8400 MHz centre and never tried the odd list at all.  This test is the
    /// exact inverse of the one it replaced
    /// (`an_even_divider_wins_even_when_an_odd_one_is_exact`), which pinned
    /// that Skylake rule while `pll.rs` implemented the Skylake search; §6.3
    /// says "no even-list-wins rule" on this path and that is what changed.
    #[test]
    fn the_midpoint_rule_takes_the_odd_divider_the_skylake_path_skipped() {
        let dividers = hdmi(200_000);
        assert_eq!(dividers.total_divider(), 9, "the odd divider, not 8");
        assert_eq!(dividers.target_dco_khz(), 9_000_000);
        assert_eq!(dividers.central_freq_khz(), 8_999_000);
        // 9000 MHz is 1 MHz above the midpoint: 10_000 * 1 / 8999 = 1.1, so 1.
        assert_eq!(dividers.deviation_centipercent(), 1);
        // 9 is odd and not 3, 5 or 7, so P = 9/3 = 3, Q = 1, K = 3.
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (3, 1, 3));
        assert!(dividers.inside_prm_dco_window());
        // The losing candidate is *also* in the PRM's window -- 8 * 1000 MHz is
        // 8000 MHz, above the 7998 MHz floor -- so the difference here is the
        // midpoint rule and nothing else.
        assert!(8 * 1_000_000 >= PRM_DCO_MIN_KHZ && 8 * 1_000_000 <= PRM_DCO_MAX_KHZ);
        assert!((9_000_000u64).abs_diff(8_999_000) < (8_000_000u64).abs_diff(8_999_000));
    }

    /// The two searches this module has implemented disagree, and this pins
    /// where -- with the arithmetic, so the difference is checkable by hand.
    ///
    /// 198 MHz and 138.5 MHz are two of the rates a sibling workstream measured
    /// (`output::tests::pll_rs_search_is_measured_against_the_documented_adl_n_search`,
    /// over 1 MHz steps from 16 to 1000 MHz).  Both are cases where the two
    /// searches return a different total divider.
    ///
    /// **198 MHz -- the Skylake search put the DCO outside the PRM's window.**
    ///
    /// ```text
    /// afe_clock = 5 * 198 000 = 990 000 kHz
    /// d = 8 : 7 920 000 kHz  < 7 998 000  -> rejected, and this is what the
    ///                                         Skylake search returned
    /// d = 9 : 8 910 000 kHz  |8 910 000 - 8 999 000| =    89 000  -> chosen
    /// d = 10: 9 900 000 kHz  |9 900 000 - 8 999 000| =   901 000
    /// ```
    ///
    /// 7920 MHz is not "slightly wrong": the PRM's window starts at 7998 MHz, so
    /// the Skylake answer asks the DCO to run below its documented minimum.  That
    /// is the failure this workstream exists to remove, and 12 of the 985
    /// measured rates were in it.
    ///
    /// **138.5 MHz -- both answers are in the window, and they still differ.**
    ///
    /// ```text
    /// afe_clock = 5 * 138 500 = 692 500 kHz
    /// d = 12: 8 310 000 kHz  |8 310 000 - 8 999 000| = 689 000  -> chosen
    /// d = 14: 9 695 000 kHz  |9 695 000 - 8 999 000| = 696 000  -> what the
    ///                                                              Skylake search
    ///                                                              returned
    /// ```
    ///
    /// This is DMT 0x52, a mode the kernel publishes, so the difference reaches
    /// the mode layer and not just a synthetic rate.
    #[test]
    fn the_adl_n_search_and_the_skylake_search_disagree_where_measured() {
        // 198 MHz: the Skylake search's answer is out of the DCO window.
        let outside = hdmi(198_000);
        assert_eq!(outside.total_divider(), 9);
        assert_eq!(outside.target_dco_khz(), 8_910_000);
        assert_eq!((outside.p(), outside.q(), outside.k()), (3, 1, 3));
        assert!(outside.inside_prm_dco_window());
        // The Skylake search's answer for this rate, by its own arithmetic
        // above: divider 8 at 7920 MHz, below the window's 7998 MHz floor.
        assert_eq!(8 * 5 * 198_000, 7_920_000);
        assert!(7_920_000 < PRM_DCO_MIN_KHZ);

        // 138.5 MHz: DMT 0x52.  Both in the window; the midpoint picks 12.
        let inside = hdmi(138_500);
        assert_eq!(inside.total_divider(), 12);
        assert_eq!(inside.target_dco_khz(), 8_310_000);
        assert!(inside.inside_prm_dco_window());
        // 14 (9695 MHz) is the Skylake answer and is also in the window.
        assert_eq!(14 * 5 * 138_500, 9_695_000);
        assert!(9_695_000 <= PRM_DCO_MAX_KHZ);
        assert!(
            (8_310_000u64).abs_diff(8_999_000) < (9_695_000u64).abs_diff(8_999_000),
            "the midpoint rule prefers 12"
        );
    }

    /// The search uses the PRM's window as a hard bound, which is what makes
    /// `inside_prm_dco_window` an invariant instead of a report.
    ///
    /// 395 MHz is the case the test this replaces
    /// (`the_two_dco_framings_really_do_differ`) used to show the two framings
    /// disagreeing: the Skylake search took the even divider 4 at 7900 MHz,
    /// 5.95% below its 8400 MHz centre and *outside* the PRM's window, where
    /// the ADL-N search takes 5 at 9875 MHz.  §6.3 records that disagreement;
    /// the branch now implements the ADL-N side of it, so the test asserts the
    /// ADL-N answer and states the Skylake one as the counterfactual.
    #[test]
    fn the_search_uses_the_prm_window_it_reports() {
        // 395 MHz: afe_clock is 1975 MHz.
        let dividers = hdmi(395_000);
        // 4 * 1975 = 7900 MHz is below the window's floor, so it is not even a
        // candidate; 5 * 1975 = 9875 MHz is in the window and is the closest to
        // the 8999 MHz midpoint.
        assert_eq!(4 * 1_975_000, 7_900_000);
        assert!(
            7_900_000 < PRM_DCO_MIN_KHZ,
            "the Skylake answer is out of the window"
        );
        assert_eq!(5 * 1_975_000, 9_875_000);
        assert!(9_875_000 >= PRM_DCO_MIN_KHZ && 9_875_000 <= PRM_DCO_MAX_KHZ);

        assert_eq!(dividers.total_divider(), 5);
        assert_eq!(dividers.target_dco_khz(), 9_875_000);
        assert!(dividers.inside_prm_dco_window());
        // 9875 MHz is 876 MHz above the midpoint: 10_000 * 876 / 8999 = 973.4.
        assert_eq!(dividers.deviation_centipercent(), 973);
        // 5 is odd and is one of 3, 5, 7, so P = 5, Q = 1, K = 1.
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (5, 1, 1));
    }

    // -- the PDIV/KDIV gap ---------------------------------------------------

    /// `P = 5` has no `Executed` code at all, and the ADL-N decomposition
    /// reaches it.
    ///
    /// A 360 MHz symbol rate has `afe_clock` 1800 MHz, and the odd divider 5
    /// lands on 9000 MHz, 1 MHz from the midpoint, while no other candidate is
    /// even in the window (4 * 1800 = 7200 MHz is below it, 6 * 1800 = 10 800
    /// above).  The search therefore returns `P = 5`, which the *Skylake*
    /// `skl_wrpll_params_populate` has no case for: i915 warns "Incorrect PDiv"
    /// and programs whatever `pdiv` its zero-initialised struct held.  ADL-N's
    /// `icl_wrpll_params_populate` codes it as `4 << 2` like any other `P`, so
    /// the `Named` encoding accepts it and only `Executed` refuses.  That is
    /// §6.3's "the representability asymmetry is the sharp edge", as a test.
    #[test]
    fn a_post_divider_of_five_has_no_executed_code() {
        let dividers = hdmi(360_000);
        assert_eq!(dividers.total_divider(), 5);
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (5, 1, 1));
        assert_eq!(dividers.target_dco_khz(), 9_000_000);
        // 9000 MHz is 1 MHz above the midpoint: 10_000 * 1 / 8999 = 1.1.
        assert_eq!(dividers.deviation_centipercent(), 1);
        assert_eq!(
            dividers.cfgcr1(PllFieldEncoding::Executed),
            Err(PllError::DividerNotEncodable {
                p: 5,
                k: 0,
                field: PllDividerField::Post,
                encoding: PllFieldEncoding::Executed,
            })
        );
        // The named-constant convention -- the one ADL-N's own encoder emits --
        // has a code, and the PRM agrees that P = 5 is a legal post divider.
        assert!(dividers.cfgcr1(PllFieldEncoding::Named).is_ok());
        assert_eq!(dividers.prm_legal_divider_set(), Some((5, 1, 1)));
        // The DCO arithmetic is unaffected: this is an encoder question only.
        assert_eq!(dividers.achieved_symbol_rate_hz(), 360_000_000);
    }

    /// `K = 5` is not reachable from the ADL-N search, which is what removes
    /// the Skylake path's PRM-illegal case.
    ///
    /// The test this replaces
    /// (`a_k_of_five_is_written_but_is_not_a_legal_prm_k`) pinned that a 180 MHz
    /// symbol rate took the total divider 10 and the *Skylake* decomposition
    /// turned it into `(2, 1, 5)`, with a `K = 5` the PRM does not define.  The
    /// ADL-N decomposition takes the `%5` branch for 10 and answers
    /// `(5, 1, 2)` -- `K = 2`, `Q = 1` -- so the illegal case no longer arises,
    /// and the same total divider now reaches the same answer through both the
    /// decomposition and the PRM's own bounds.
    ///
    /// The encoder guard is still tested rather than assumed: `K = 5` has no
    /// `Named` code, and no candidate divider decomposes to it.
    #[test]
    fn the_adl_n_decomposition_never_produces_a_k_of_five() {
        // 180 MHz: afe_clock 900 MHz, and 10 * 900 = 9000 MHz is the candidate
        // closest to the 8999 MHz midpoint (9 * 900 = 8100 is 899 away).
        let dividers = hdmi(180_000);
        assert_eq!(dividers.total_divider(), 10);
        assert_eq!(dividers.target_dco_khz(), 9_000_000);
        // §6.3's branch list: 10 % 4 != 0, 10 % 6 != 0, 10 % 5 == 0 ->
        // P = 5, Q = 10/10 = 1, K = 2.
        assert_eq!((dividers.p(), dividers.q(), dividers.k()), (5, 1, 2));
        assert_eq!(dividers.prm_legal_divider_set(), Some((5, 1, 2)));
        // The same total divider the Skylake decomposition answered with an
        // illegal `K = 5` is now answered with `P = 5`, so the encoder refusal
        // moves from `Named` to `Executed`: the Skylake encoder has no case for
        // `P = 5` either, while the Gen12 one codes it `4 << 2` and accepts it.
        assert!(dividers.cfgcr1(PllFieldEncoding::Named).is_ok());
        assert_eq!(
            dividers.cfgcr1(PllFieldEncoding::Executed),
            Err(PllError::DividerNotEncodable {
                p: 5,
                k: 0,
                field: PllDividerField::Post,
                encoding: PllFieldEncoding::Executed,
            })
        );

        // The three cases the Skylake decomposition got wrong are gone from the
        // ADL-N list's answers.
        assert_eq!(icl_wrpll_get_multipliers(10), Some((5, 1, 2)));
        assert_eq!(icl_wrpll_get_multipliers(6), Some((3, 1, 2)));
        assert_eq!(icl_wrpll_get_multipliers(50), Some((5, 5, 2)));
        assert_eq!(icl_wrpll_get_multipliers(70), Some((5, 7, 2)));
        assert_eq!(icl_wrpll_get_multipliers(15), Some((5, 1, 3)));
        for &total_divider in ADL_N_TOTAL_DIVIDERS {
            let (_, _, k) = icl_wrpll_get_multipliers(total_divider).unwrap();
            assert_ne!(k, 5, "divider {total_divider} decomposed to K = 5");
        }
        // `K = 5` is still not a value the Gen12 encoder can write, so the
        // guard is real and not merely unreachable.
        assert_eq!(
            kdiv_code(5, PllFieldEncoding::Named),
            Err(PllError::DividerNotEncodable {
                p: 0,
                k: 5,
                field: PllDividerField::K,
                encoding: PllFieldEncoding::Named,
            })
        );
        assert_eq!(kdiv_code(5, PllFieldEncoding::Executed), Ok(0));
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
            .symbol_rate_hz(
                REF_24,
                PllFieldEncoding::Executed,
                DcoFractionWorkaround::NotNeeded,
            )
            .unwrap();
        assert_eq!(honest, 148_500_000);

        // Read back i915's way -- `icl_ddi_combo_pll_get_freq` decodes KDIV with
        // the named constants.  The chosen set has K = 2, written as the
        // executed code 1, which the named convention reads as K = 1.
        let i915_way = registers
            .symbol_rate_hz(
                REF_24,
                PllFieldEncoding::Named,
                DcoFractionWorkaround::NotNeeded,
            )
            .unwrap();
        assert_eq!(
            i915_way, 297_000_000,
            "i915's own read-back doubles the rate"
        );
        assert_ne!(i915_way, honest);
    }

    // -- reference clock -----------------------------------------------------

    /// The reference a 38.4 MHz strap is divided to, and the consequence.
    #[test]
    fn a_38_4_mhz_reference_is_divided_to_19_2_for_the_arithmetic() {
        assert_eq!(wrpll_reference_khz(38_400), Ok(19_200));
        assert_eq!(wrpll_reference_khz(24_000), Ok(24_000));
        assert_eq!(wrpll_reference_khz(19_200), Ok(19_200));
        assert_eq!(wrpll_reference_khz(0), Err(PllError::ZeroReference));
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
        assert_eq!(at_38_4.central_freq_khz(), 8_999_000);
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
            .registers(
                PllFieldEncoding::Named,
                DcoFractionWorkaround::HalveFraction,
            )
            .unwrap();
        assert_eq!(
            registers
                .symbol_rate_hz(
                    38_400,
                    PllFieldEncoding::Named,
                    DcoFractionWorkaround::HalveFraction
                )
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
    ///
    /// The bounds are now exact rather than approximate, because the window is a
    /// hard constraint on the candidates.  The smallest total divider is 2 and
    /// the largest is 102, so the symbol rate has to be in
    /// `[7998/(102*5), 10000/(2*5)]` = `[15.6824, 1000]` MHz.
    #[test]
    fn a_pixel_clock_outside_the_plls_range_is_refused() {
        let below = ddi_pll_dividers(15_682, REF_24, ComboPhy::A);
        assert_eq!(
            below,
            Err(PllError::NoLegalDividerSet {
                symbol_rate_khz: 15_682,
                ref_khz: REF_24,
            })
        );
        // The lower end is the largest divider: 102 * 78410 = 7 997 820 kHz,
        // which is 180 kHz below the window's floor.  78315 kHz -- one step of
        // 1 kHz up -- is 7 998 330 kHz and is accepted.
        assert_eq!(102 * 5 * 15_682, 7_997_820);
        assert!(7_997_820 < PRM_DCO_MIN_KHZ);
        let just_above = hdmi(15_683);
        assert_eq!(just_above.total_divider(), 102);
        assert_eq!(just_above.target_dco_khz(), 7_998_330);
        assert!(just_above.inside_prm_dco_window());

        // The upper end is the smallest divider, 2, and it lands exactly on the
        // window's ceiling: 2 * 5 * 1 000 000 = 10 000 000 kHz.
        let at_ceiling = hdmi(1_000_000);
        assert_eq!(at_ceiling.total_divider(), 2);
        assert_eq!(at_ceiling.target_dco_khz(), PRM_DCO_MAX_KHZ);
        assert_eq!((at_ceiling.p(), at_ceiling.q(), at_ceiling.k()), (2, 1, 1));
        let above = ddi_pll_dividers(1_001_000, REF_24, ComboPhy::A);
        assert_eq!(
            above,
            Err(PllError::NoLegalDividerSet {
                symbol_rate_khz: 1_001_000,
                ref_khz: REF_24,
            })
        );
        // A rate that is inside neither end: 700 MHz needs a total divider
        // between 2.29 and 2.86, and the list has no such entry.
        assert_eq!(
            ddi_pll_dividers(700_000, REF_24, ComboPhy::A),
            Err(PllError::NoLegalDividerSet {
                symbol_rate_khz: 700_000,
                ref_khz: REF_24,
            })
        );
        assert!(ddi_pll_dividers(20_000, REF_24, ComboPhy::A).is_ok());
        assert!(ddi_pll_dividers(600_000, REF_24, ComboPhy::A).is_ok());
        assert_eq!(
            ddi_pll_dividers(0, REF_24, ComboPhy::A),
            Err(PllError::ZeroSymbolRate)
        );
    }

    /// The smallest and largest total dividers the list contains are both
    /// reachable, and they are the ones the arithmetic says they are.
    ///
    /// Under the Skylake search this test used 3 and 98, because the Skylake
    /// even list starts at 4 and the ADL-N list starts at 2 and runs to 102.
    #[test]
    fn the_smallest_and_largest_legal_dividers_are_reachable() {
        // 900 MHz: afe_clock 4500 MHz, and 2 * 4500 = 9000 MHz is in the window
        // while 3 * 4500 = 13 500 MHz is far above it.  The list's smallest
        // entry, 2, is the `P = 2, Q = 1, K = 1` branch.
        let smallest = hdmi(900_000);
        assert_eq!(smallest.total_divider(), 2);
        assert_eq!(smallest.target_dco_khz(), 9_000_000);
        assert_eq!(smallest.deviation_centipercent(), 1);
        assert_eq!((smallest.p(), smallest.q(), smallest.k()), (2, 1, 1));

        // 15.683 MHz: afe_clock 78.415 MHz, and 102 * 78.415 = 7998.33 MHz, just
        // inside the window's floor.  100 * 78.415 = 7841.5 MHz is below it.
        // This is the largest entry, 102 = 3 * 17 * 2, at the very bottom of
        // the band -- 1111 centipercent from the midpoint and still the right
        // answer, because the window is the constraint and the midpoint only
        // breaks ties within it.
        let largest = hdmi(15_683);
        assert_eq!(largest.total_divider(), 102);
        assert_eq!(largest.target_dco_khz(), 7_998_330);
        assert_eq!(largest.deviation_centipercent(), 1111);
        assert_eq!((largest.p(), largest.q(), largest.k()), (3, 17, 2));
        assert!(largest.inside_prm_dco_window());

        // P = 7 is reachable too, and it is the half of the encoder split the
        // Skylake P = 5 case does not cover.
        //
        // 18.45 MHz: afe_clock 92.25 MHz, and 98 * 92.25 = 9040.5 MHz, which is
        // 46 centipercent above the 8999 MHz midpoint.
        let p_is_seven = hdmi(18_450);
        assert_eq!(p_is_seven.total_divider(), 98);
        assert_eq!(p_is_seven.target_dco_khz(), 9_040_500);
        assert_eq!(p_is_seven.deviation_centipercent(), 46);
        assert_eq!((p_is_seven.p(), p_is_seven.q(), p_is_seven.k()), (7, 7, 2));
        // The two encoders code P = 7 differently, so the same divider set has
        // two different CFGCR1 values.
        let executed = p_is_seven.cfgcr1(PllFieldEncoding::Executed).unwrap();
        let named = p_is_seven.cfgcr1(PllFieldEncoding::Named).unwrap();
        assert_eq!((executed >> 2) & 0xf, 4);
        assert_eq!((named >> 2) & 0xf, 8);
    }

    // -- properties ----------------------------------------------------------

    /// Sweep the whole range and check the module's own invariants on every
    /// answer: the dividers multiply out, the DCO is inside the PRM's window,
    /// the decomposition is the PRM-legal one, the encodings agree with the
    /// divider values they claim, and the rate is right.
    ///
    /// The sweep also pins where the search has *no* answer, which is not the
    /// whole range: the divider list has gaps between consecutive entries, and
    /// a symbol rate whose window `[7998000/(5r), 10000000/(5r)]` falls entirely
    /// inside one of them cannot be served.  On this grid there are two such
    /// bands, and they are a property of the documented algorithm rather than of
    /// this implementation.
    #[test]
    fn every_answer_satisfies_the_invariants() {
        let mut found = 0u32;
        let mut refused = 0u32;
        for symbol_rate_khz in (17_000..=1_000_000).step_by(250) {
            let dividers = match ddi_pll_dividers(symbol_rate_khz, REF_24, ComboPhy::B) {
                Ok(dividers) => dividers,
                Err(PllError::NoLegalDividerSet { .. }) => {
                    refused += 1;
                    continue;
                }
                Err(other) => panic!("{symbol_rate_khz} kHz: unexpected error {other}"),
            };
            found += 1;

            // The dividers multiply back to the total.
            assert_eq!(
                dividers.p() * dividers.q() * dividers.k(),
                dividers.total_divider(),
                "{symbol_rate_khz} kHz"
            );
            // The total divider is one the list actually contains.
            assert!(
                ADL_N_TOTAL_DIVIDERS.contains(&dividers.total_divider()),
                "{symbol_rate_khz} kHz: {} is not a candidate",
                dividers.total_divider()
            );
            // The DCO is inside the PRM's window, which is what the ADL-N
            // search enforces and the Skylake one did not.
            assert!(
                dividers.inside_prm_dco_window(),
                "{symbol_rate_khz} kHz: DCO {} kHz is outside [7998, 10000] MHz",
                dividers.target_dco_khz()
            );
            // The midpoint is the only aim point this path has.
            assert_eq!(
                dividers.central_freq_khz(),
                8_999_000,
                "{symbol_rate_khz} kHz"
            );
            // The decomposition is the one the PRM's own bounds would pick as
            // well, because icl_wrpll_get_multipliers satisfies them by
            // construction.
            assert_eq!(
                dividers.prm_legal_divider_set(),
                Some((dividers.p(), dividers.q(), dividers.k())),
                "{symbol_rate_khz} kHz"
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
            // The declared distance from the midpoint is the definition.
            assert_eq!(
                dividers.deviation_centipercent(),
                10_000 * dividers.target_dco_khz().abs_diff(8_999_000) / 8_999_000,
                "{symbol_rate_khz} kHz"
            );
            // No other candidate in the list is closer to the midpoint while
            // still inside the window -- that is the search's whole contract,
            // re-derived here rather than taken from it.
            let afe_clock_khz = 5 * u64::from(symbol_rate_khz);
            for &candidate in ADL_N_TOTAL_DIVIDERS {
                let dco_khz = u64::from(candidate) * afe_clock_khz;
                if dco_khz < PRM_DCO_MIN_KHZ || dco_khz > PRM_DCO_MAX_KHZ {
                    continue;
                }
                assert!(
                    dco_khz.abs_diff(8_999_000) >= dividers.target_dco_khz().abs_diff(8_999_000),
                    "{symbol_rate_khz} kHz: divider {candidate} at {dco_khz} kHz is closer to the \
                     midpoint than the chosen {} at {} kHz",
                    dividers.total_divider(),
                    dividers.target_dco_khz()
                );
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
                // is a relative error, not an absolute one: 92 ppb at the
                // smallest legal DCO is 2 Hz at a 17 MHz symbol rate.
                let requested_hz = u64::from(symbol_rate_khz) * 1000;
                let error_ppb = (decoded as i128 - requested_hz as i128) * 1_000_000_000i128
                    / requested_hz as i128;
                assert!(
                    error_ppb.abs() <= MAX_SYMBOL_RATE_ERROR_PPB as i128,
                    "{symbol_rate_khz} kHz decoded to {decoded} Hz under {encoding} ({error_ppb} \
                     ppb)"
                );
            }
        }
        // The sweep must actually have covered the range, or the test proves
        // nothing: 3933 sampled rates, of which 665 are in the two gap bands.
        assert_eq!(found + refused, 3_933, "the sweep did not cover the range");
        assert_eq!(found, 3_268, "solvable rates in the sweep");
        assert_eq!(refused, 665, "refused rates in the sweep");
    }

    /// Where the candidate list cannot reach, and why -- to the kilohertz.
    ///
    /// The gaps are between consecutive entries of [`ADL_N_TOTAL_DIVIDERS`], and
    /// a symbol rate is servable exactly when some entry's DCO lands in
    /// `[7998, 10000] MHz`.  Two ranges of rates fall between entries:
    ///
    /// * `(500 000, 533 200)` kHz -- between 4 and 3.  500 MHz puts 4 * 2500 MHz
    ///   exactly on the 10 000 MHz ceiling, and 533.2 MHz puts 3 * 2666 MHz
    ///   exactly on the 7998 MHz floor.
    /// * `(666 666, 799 800)` kHz -- between 3 and 2.  666.666 MHz puts
    ///   3 * 3333.33 MHz just under the ceiling, and 799.8 MHz puts
    ///   2 * 3999 MHz exactly on the floor.
    ///
    /// Every value in between is refused, and every value outside them has an
    /// answer.  This is bounded and worth knowing rather than discovering on the
    /// machine: a caller that asks for one of these and gets
    /// [`PllError::NoLegalDividerSet`] has a rate this PLL cannot make, not a
    /// bug.  On the 1 MHz grid the two bands are 501-533 MHz and 667-799 MHz --
    /// 166 of the 985 rates
    /// `output::tests::pll_rs_search_is_measured_against_the_documented_adl_n_search`
    /// measures.
    ///
    /// The Skylake search this module used to use could serve 7 of those rates
    /// (527-533 MHz) -- and in every one of the 7 it programmed a DCO *below*
    /// 7998 MHz, i.e. outside the window, which is why they are not a real
    /// loss.  That is asserted in the `output` test above.
    #[test]
    fn the_candidate_list_has_two_gaps_and_they_are_refused() {
        // The first band, from both sides: 500 MHz is the ceiling with divider
        // 4, and 533.2 MHz is the floor with divider 3.
        assert_eq!(hdmi(500_000).total_divider(), 4);
        assert_eq!(hdmi(500_000).target_dco_khz(), PRM_DCO_MAX_KHZ);
        assert!(ddi_pll_dividers(500_001, REF_24, ComboPhy::A).is_err());
        assert!(ddi_pll_dividers(533_199, REF_24, ComboPhy::A).is_err());
        assert_eq!(hdmi(533_200).total_divider(), 3);
        assert_eq!(hdmi(533_200).target_dco_khz(), PRM_DCO_MIN_KHZ);

        // The second band.  666.666 MHz is 3 * 3333.33 MHz, just inside the
        // ceiling; 666.667 MHz is 10 000 005 kHz with divider 3 and 6 666 670
        // kHz with divider 2, so it has nothing.  At the other end 799.8 MHz is
        // 2 * 3999 MHz, exactly the floor.
        assert_eq!(hdmi(666_666).total_divider(), 3);
        assert_eq!(hdmi(666_666).target_dco_khz(), 9_999_990);
        assert!(ddi_pll_dividers(666_667, REF_24, ComboPhy::A).is_err());
        assert!(ddi_pll_dividers(799_799, REF_24, ComboPhy::A).is_err());
        assert_eq!(hdmi(799_800).total_divider(), 2);
        assert_eq!(hdmi(799_800).target_dco_khz(), PRM_DCO_MIN_KHZ);

        // A refusal carries the rate and the reference, so a log reader can tell
        // which one happened.
        assert_eq!(
            ddi_pll_dividers(700_000, REF_24, ComboPhy::A),
            Err(PllError::NoLegalDividerSet {
                symbol_rate_khz: 700_000,
                ref_khz: REF_24,
            })
        );
    }

    /// The `DCO_INTEGER`/`DCO_FRACTION` split is the one
    /// `icl_wrpll_params_populate` computes, and not the Skylake encoder's.
    ///
    /// `[I915]` `icl_wrpll_params_populate` (`intel_dpll_mgr.c:2588-2591`):
    ///
    /// ```text
    /// dco          = div_u64((u64)dco_freq << 15, ref_freq);
    /// dco_integer  = dco >> 15;
    /// dco_fraction = dco & 0x7fff;
    /// ```
    ///
    /// with `dco_freq` and `ref_freq` in kHz.  This module computes the integer
    /// and the fraction separately from the remainder, which is the same
    /// arithmetic; the test is here because the *Skylake* encoder
    /// (`skl_wrpll_params_populate`, `:1654-1657`) divides by a reference
    /// truncated to whole megahertz instead, and reaching for that one is the
    /// mistake the whole workstream is about.
    #[test]
    fn the_dco_fraction_split_is_the_icl_encoders() {
        for ref_khz in [19_200u32, 24_000, 38_400] {
            let wrpll_ref_khz = u64::from(wrpll_reference_khz(ref_khz).unwrap());
            for rate_khz in [
                15_683u32, 25_175, 74_250, 148_500, 297_000, 594_000, 900_000,
            ] {
                let dividers = ddi_pll_dividers(rate_khz, ref_khz, ComboPhy::A).unwrap();
                let packed = (dividers.target_dco_khz() << 15) / wrpll_ref_khz;
                assert_eq!(
                    u64::from(dividers.dco_integer()),
                    packed >> 15,
                    "reference {ref_khz} kHz, rate {rate_khz} kHz: DCO_INTEGER"
                );
                assert_eq!(
                    u64::from(dividers.dco_fraction()),
                    packed & 0x7fff,
                    "reference {ref_khz} kHz, rate {rate_khz} kHz: DCO_FRACTION"
                );
            }
        }
    }

    /// The reference frequency does not change which divider set is chosen --
    /// the DCO does not depend on it -- but it does change the registers, and
    /// every sourced reference must produce a self-consistent answer.
    #[test]
    fn every_sourced_reference_gives_a_self_consistent_answer() {
        for ref_khz in [19_200u32, 24_000, 38_400] {
            let dividers = ddi_pll_dividers(148_500, ref_khz, ComboPhy::A).unwrap();
            assert_eq!(dividers.total_divider(), 12, "reference {ref_khz} kHz");
            assert_eq!(
                dividers.wrpll_ref_khz(),
                wrpll_reference_khz(ref_khz).unwrap()
            );
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
                    .symbol_rate_hz(
                        ref_khz,
                        PllFieldEncoding::Named,
                        DcoFractionWorkaround::NotNeeded
                    )
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

    /// The PRM's `(P, Q, K)` bounds, checked across every total divider there
    /// is -- not only the candidates.
    ///
    /// The test this replaces
    /// (`the_prm_bounds_do_not_cover_every_divider_i915_uses`) asserted the
    /// opposite conclusion: that `prm_legal_divider_set(35)` is `None` while the
    /// Skylake candidate list contains 35, and that `total = 10` decomposed to
    /// `K = 5`, which the PRM does not define.  Both were true of the Skylake
    /// path and are false of the ADL-N one, which is §6.3's point that "the
    /// PRM's rules are satisfied by construction" here.  What remains worth
    /// checking is that the report function's bounds are the ones it claims, on
    /// every input rather than only the reachable ones.
    #[test]
    fn the_prm_bounds_hold_wherever_the_report_function_answers() {
        // Totals both sources can express, and the answers agree.
        assert_eq!(prm_legal_divider_set(3), Some((3, 1, 1)));
        assert_eq!(prm_legal_divider_set(5), Some((5, 1, 1)));
        assert_eq!(prm_legal_divider_set(12), Some((2, 3, 2)));
        assert_eq!(prm_legal_divider_set(24), Some((2, 6, 2)));
        assert_eq!(prm_legal_divider_set(76), Some((2, 19, 2)));
        // 10 and 15 are the totals the Skylake decomposition answered with an
        // illegal K = 5; the ADL-N decomposition answers the PRM's way.
        assert_eq!(prm_legal_divider_set(10), Some((5, 1, 2)));
        assert_eq!(prm_legal_divider_set(15), Some((5, 1, 3)));
        assert_eq!(icl_wrpll_get_multipliers(10), Some((5, 1, 2)));
        assert_eq!(icl_wrpll_get_multipliers(15), Some((5, 1, 3)));
        // The PRM cannot express 35 at all: 35 = 5 * 7 needs Q = 7 with K = 1,
        // and K != 2 forces Q = 1.  It is the one total the Skylake list
        // carried past 21, and the ADL-N list does not carry it.
        assert_eq!(prm_legal_divider_set(35), None);
        assert!(!ADL_N_TOTAL_DIVIDERS.contains(&35));
        assert!(icl_wrpll_get_multipliers(35).is_none());
        // Whatever it returns must satisfy the bounds it claims.
        for total in 1u32..=200 {
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
            "central_freq_khz: 8999000",
            "target_dco_khz: 8910000",
            "deviation_centipercent: 98",
            "dco_integer: 371",
            "dco_fraction: 8192",
            "rate_error_ppb: 0",
        ] {
            assert!(text.contains(expected), "{expected} missing from {text}");
        }
    }
}
