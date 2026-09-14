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
            let error_ppb =
                (decoded as i128 - requested_hz as i128) * 1_000_000_000i128 / requested_hz as i128;
            assert!(
                error_ppb.abs() < 1_000,
                "{name}: round trip produced {decoded} Hz for {requested_hz} Hz ({error_ppb} ppb) \
                 under {encoding}"
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
        assert!((1..=255).contains(&q), "divider {total_divider}: Q = {q}");
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
            let error_ppb =
                (decoded as i128 - requested_hz as i128) * 1_000_000_000i128 / requested_hz as i128;
            assert!(
                error_ppb.abs() <= MAX_SYMBOL_RATE_ERROR_PPB as i128,
                "{symbol_rate_khz} kHz decoded to {decoded} Hz under {encoding} ({error_ppb} ppb)"
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
        assert!((1..=255).contains(&q), "total {total}: Q = {q}");
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
