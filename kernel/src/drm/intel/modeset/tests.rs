//! The mode-choice policy and phase 6's verdicts, against a modelled register
//! file.
//!
//! What these tests establish: that the reference's preference and the mode
//! layer's decision reconcile the way the design document says they do, that a
//! refusal is a refusal and not a guessed timing, and that each of §11 phase
//! 6's four checks fails by name with the reading it failed from.  What they
//! cannot establish is anything about a display engine: every register here is
//! a `BTreeMap` entry, and no part of this driver has run on the target.

use alloc::{vec, vec::Vec};
use core::cell::Cell;

use super::*;
use crate::{
    drm::{
        intel::{
            gmbus::tests::FakeClock,
            regs::{
                mock::MockRegisters,
                pipe::{PIPEDSL_A, PIPESTAT_A, PLANE_SURFLIVE_A},
            },
        },
        modes::{
            Constraints, FALLBACK_MODE, ModeFlags, ModeList, SelectionReason, TimingSource,
            advertised_modes, plan_modeset,
        },
    },
    test_support::scheduler_test_context,
};

// ---------------------------------------------------------------------------
// EDID construction
//
// The mode layer's own fixture builders are private to `drm::modes`, and
// reaching into them would mean editing a file three workstreams are building
// against at once.  These builders write the bytes the parser reads; the two
// that mirror existing fixtures produce byte-identical descriptors.
// ---------------------------------------------------------------------------

/// The CEA-861 1080p60 timing, the one §11 phase 3.1 prefers: 148.5 MHz.
fn mode_1080p60() -> Mode {
    Mode::from_blanking(
        148_500,
        1920,
        280,
        88,
        44,
        1080,
        45,
        4,
        5,
        ModeFlags::NONE,
        TimingSource::EdidDtd { index: 0 },
    )
    .with_polarity(true, true)
}

/// One detailed timing descriptor, in the field layout `drm::modes::edid`
/// parses (and `modes::fixtures` writes): active and blanking split across a
/// low byte and a high nibble, porches and sync widths across a low byte and
/// two high bits, and the flags byte last.
fn descriptor(mode: &Mode) -> [u8; 18] {
    let mut data = [0u8; 18];
    let clock = (mode.clock_khz / 10) as u16;
    data[0] = clock as u8;
    data[1] = (clock >> 8) as u8;
    let hblank = mode.hblank();
    let vblank = mode.vblank();
    data[2] = mode.hdisplay as u8;
    data[3] = hblank as u8;
    data[4] = (((mode.hdisplay >> 8) & 0x0f) as u8) << 4 | ((hblank >> 8) & 0x0f) as u8;
    data[5] = mode.vdisplay as u8;
    data[6] = vblank as u8;
    data[7] = (((mode.vdisplay >> 8) & 0x0f) as u8) << 4 | ((vblank >> 8) & 0x0f) as u8;
    let hfront = mode.hsync_start - mode.hdisplay;
    let hsync = mode.hsync_len();
    let vfront = mode.vsync_start - mode.vdisplay;
    let vsync = mode.vsync_len();
    data[8] = hfront as u8;
    data[9] = hsync as u8;
    data[10] = ((vfront as u8) << 4) | (vsync as u8 & 0x0f);
    data[11] = ((((hfront >> 8) & 0x3) << 6)
        | (((hsync >> 8) & 0x3) << 4)
        | (((vfront >> 8) & 0x3) << 2)
        | ((vsync >> 8) & 0x3)) as u8;
    // 509 mm x 286 mm, so the mode layer reads a real image size.
    data[12] = 0xfd;
    data[13] = 0x1e;
    data[14] = 0x11;
    // Digital separate sync, with the mode's own polarity.
    data[17] = 0x18 | u8::from(mode.hsync_positive) << 1 | u8::from(mode.vsync_positive) << 2;
    data
}

/// A structurally valid EDID base block, with `preferred` in the first
/// detailed timing slot and `extension_count` extension blocks declared.
fn base_block(preferred: Option<&Mode>, extension_count: u8) -> Vec<u8> {
    let mut block = vec![0u8; 128];
    block[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
    block[8] = 0x04; // "ACR"
    block[9] = 0x21;
    block[0x12] = 0x01; // EDID 1.3
    block[0x13] = 0x03;
    block[0x14] = 0x80; // digital input
    // Byte 0x18 bit 1: "the first detailed timing descriptor is the preferred
    // timing".  A revision-1.3 block without it has no preferred timing at all,
    // and the mode layer then classifies the descriptor as an ordinary detailed
    // timing -- the first version of this fixture omitted it and
    // `the_mode_layers_own_choice_stands_when_the_sink_prefers_it` failed with
    // `DetailedTiming` where it expected `SinkPreferred`.
    block[0x18] = 0x02;
    if let Some(mode) = preferred {
        block[0x36..0x36 + 18].copy_from_slice(&descriptor(mode));
    }
    block[0x7e] = extension_count;
    let checksum = block[..127]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        .wrapping_neg();
    block[127] = checksum;
    block
}

/// A CTA-861 revision 3 extension block with one video data block of plain VIC
/// codes and no detailed timings.
fn cta_block(vics: &[u8]) -> Vec<u8> {
    let mut block = vec![0u8; 128];
    block[0] = 0x02; // CTA-861
    block[1] = 0x03; // revision 3
    block[2] = (5 + vics.len()) as u8; // detailed timings start past the data blocks
    block[4] = (2 << 5) | vics.len() as u8; // video data block
    block[5..5 + vics.len()].copy_from_slice(vics);
    let checksum = block[..127]
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte))
        .wrapping_neg();
    block[127] = checksum;
    block
}

/// A base block and its extensions, concatenated.
fn edid(base: Vec<u8>, extensions: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = base;
    for extension in extensions {
        bytes.extend_from_slice(extension);
    }
    bytes
}

/// The CDCLK the power step found on the target machine's power-on default,
/// near enough for a policy test: the i3-N305's firmware leaves 172.8 MHz.
const CDCLK_KHZ: u32 = 172_800;

// ---------------------------------------------------------------------------
// Which mode
// ---------------------------------------------------------------------------

#[test]
fn the_mode_layers_own_choice_stands_when_the_sink_prefers_it() {
    let _guard = scheduler_test_context();
    let bytes = base_block(Some(&mode_1080p60()), 0);
    let plan = plan_modeset(&bytes, &Constraints::unlimited());
    assert!(plan.used_edid());
    let choice = choose_mode(&plan, &bytes, EngineLimits::at_cdclk(CDCLK_KHZ));
    match choice {
        ModeChoice::ModeLayer { mode, reason } => {
            assert_eq!(reason, SelectionReason::SinkPreferred);
            assert_eq!(mode.clock_khz, 148_500);
            assert_eq!(mode.hdisplay, 1920);
            assert_eq!(mode.vdisplay, 1080);
            assert_eq!(mode.refresh_millihz(), 60_000);
        }
        other => panic!("expected the mode layer's own choice, got {other:?}"),
    }
    assert!(!choice.overrode_the_mode_layer());
    assert_eq!(
        mode_line_rate_hz(&choice.mode().expect("a mode")),
        Some(67_500)
    );
}

#[test]
fn a_choice_that_needs_scrambling_gives_way_to_the_references_1080p60() {
    let _guard = scheduler_test_context();
    // The sink's preferred timing is 4K60 (594 MHz) and it also advertises
    // 1080p60 as CTA-861 VIC 16 -- the shape of a television or a 4K monitor.
    let four_k = Mode::from_blanking(
        594_000,
        3840,
        560,
        176,
        88,
        2160,
        90,
        8,
        10,
        ModeFlags::NONE,
        TimingSource::EdidDtd { index: 0 },
    );
    let bytes = edid(base_block(Some(&four_k), 1), &[cta_block(&[16, 97])]);
    let plan = plan_modeset(&bytes, &Constraints::unlimited());
    assert_eq!(
        plan.selection.mode.clock_khz, 594_000,
        "the mode layer prefers the sink's preferred timing"
    );

    // Two different reasons the 594 MHz choice is unusable, and the same
    // answer either way: the reference's 1080p60.
    for cdclk in [CDCLK_KHZ, 648_000] {
        let limits = EngineLimits::at_cdclk(cdclk);
        let choice = choose_mode(&plan, &bytes, limits);
        match choice {
            ModeChoice::ReferencePreference {
                mode,
                replaced,
                because,
            } => {
                assert_eq!(mode.clock_khz, 148_500);
                assert_eq!(mode.hdisplay, 1920);
                assert!(is_reference_timing(&mode));
                assert_eq!(replaced.clock_khz, 594_000);
                assert!(matches!(because, NotProgrammable::ClockTooHigh { .. }));
            }
            other => panic!("cdclk {cdclk}: expected the reference's preference, got {other:?}"),
        }
        assert!(choice.overrode_the_mode_layer());
        assert!(
            choice.describe().contains("594000"),
            "the log must name what was set aside: {}",
            choice.describe()
        );
    }

    // With the engine's limit out of the way the link's limit is what bites:
    // 594 MHz needs scrambling whether or not CDCLK could carry it.
    assert_eq!(
        programmable(&four_k, EngineLimits::at_cdclk(648_000)),
        Err(NotProgrammable::ClockTooHigh {
            clock_khz: 594_000,
            ceiling_khz: HDMI_NO_SCRAMBLING_MAX_CLOCK_KHZ,
        })
    );
}

#[test]
fn a_safe_preferred_timing_is_not_second_guessed() {
    let _guard = scheduler_test_context();
    // 2560x1440@60 at 241.5 MHz: the panel's own mode, under the 300 MHz
    // no-scrambling ceiling.  The reference's mention of 1080p60 is a
    // robustness argument, not a rule that the smaller mode wins.
    let native = Mode::from_blanking(
        241_500,
        2560,
        160,
        48,
        32,
        1440,
        41,
        3,
        5,
        ModeFlags::NONE,
        TimingSource::EdidDtd { index: 0 },
    );
    let bytes = edid(base_block(Some(&native), 1), &[cta_block(&[16])]);
    let plan = plan_modeset(&bytes, &Constraints::unlimited());
    assert_eq!(plan.selection.mode.hdisplay, 2560);

    let choice = choose_mode(&plan, &bytes, EngineLimits::at_cdclk(400_000));
    match choice {
        ModeChoice::ModeLayer { mode, .. } => {
            assert_eq!(mode.hdisplay, 2560);
            assert_eq!(mode.clock_khz, 241_500);
        }
        other => panic!("a programmable native mode must stand, got {other:?}"),
    }

    // The engine's own limit is the other reason to fall back: at the
    // power-on CDCLK this mode cannot be clocked at all, so the sink's
    // 1080p60 is programmed instead.
    let choice = choose_mode(&plan, &bytes, EngineLimits::at_cdclk(CDCLK_KHZ));
    match choice {
        ModeChoice::ReferencePreference { mode, because, .. } => {
            assert_eq!(mode.clock_khz, 148_500);
            assert_eq!(
                because,
                NotProgrammable::ClockTooHigh {
                    clock_khz: 241_500,
                    ceiling_khz: CDCLK_KHZ,
                }
            );
        }
        other => panic!("expected the reference's preference, got {other:?}"),
    }
}

#[test]
fn no_edid_is_a_refusal_and_not_a_guessed_timing() {
    let _guard = scheduler_test_context();
    let plan = plan_modeset(&[], &Constraints::unlimited());
    assert!(!plan.used_edid());
    let choice = choose_mode(&plan, &[], EngineLimits::at_cdclk(CDCLK_KHZ));
    assert_eq!(choice.mode(), None, "nothing may be programmed here");
    match choice {
        ModeChoice::Refused(ModeRefusal::NoAdvertisedMode { because, fallback }) => {
            assert_eq!(because, crate::drm::modes::FallbackReason::NoEdid);
            assert_eq!(fallback, FALLBACK_MODE);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let text = choice.describe();
    assert!(
        text.contains("firmware framebuffer is left alone"),
        "{text}"
    );
}

#[test]
fn an_edid_the_mode_layer_could_not_use_is_also_a_refusal() {
    let _guard = scheduler_test_context();
    // A block that parses and advertises nothing: no detailed timing
    // descriptor, no established timings, no extension.
    let bytes = base_block(None, 0);
    let plan = plan_modeset(&bytes, &Constraints::unlimited());
    assert!(!plan.used_edid());
    let choice = choose_mode(&plan, &bytes, EngineLimits::at_cdclk(CDCLK_KHZ));
    match choice {
        ModeChoice::Refused(ModeRefusal::NoAdvertisedMode { because, fallback }) => {
            assert_eq!(because, crate::drm::modes::FallbackReason::NoUsableMode);
            assert_eq!(fallback, FALLBACK_MODE);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn the_ceiling_is_the_lower_of_the_engine_and_the_link() {
    assert_eq!(EngineLimits::at_cdclk(CDCLK_KHZ).ceiling_khz(), CDCLK_KHZ);
    assert_eq!(
        EngineLimits::at_cdclk(648_000).ceiling_khz(),
        HDMI_NO_SCRAMBLING_MAX_CLOCK_KHZ
    );
    assert_eq!(
        EngineLimits::at_cdclk(HDMI_NO_SCRAMBLING_MAX_CLOCK_KHZ).ceiling_khz(),
        HDMI_NO_SCRAMBLING_MAX_CLOCK_KHZ,
        "a clock exactly at the ceiling is the engine's, not the link's"
    );
}

#[test]
fn an_unprogrammable_timing_is_named_rather_than_programmed() {
    let limits = EngineLimits::at_cdclk(648_000);
    let good = mode_1080p60();
    assert_eq!(programmable(&good, limits), Ok(()));

    let malformed = Mode { htotal: 0, ..good };
    assert_eq!(
        programmable(&malformed, limits),
        Err(NotProgrammable::Malformed)
    );

    let interlaced = good.with_flags(ModeFlags::INTERLACE);
    assert_eq!(
        programmable(&interlaced, limits),
        Err(NotProgrammable::Interlaced)
    );

    let doubled = good.with_doubled_clock();
    // 297 MHz is *under* the 300 MHz ceiling, so the ceiling is not what rules
    // a pixel-repeated mode out: the missing pixel-repetition programming is.
    // The first version of this test asserted ClockTooHigh here and was simply
    // wrong about the arithmetic.
    assert_eq!(
        programmable(&doubled, limits),
        Err(NotProgrammable::PixelRepeated)
    );

    let four_k = Mode::from_blanking(
        594_000,
        3840,
        560,
        176,
        88,
        2160,
        90,
        8,
        10,
        ModeFlags::NONE,
        TimingSource::CtaVic(97),
    );
    assert_eq!(
        programmable(&four_k, limits),
        Err(NotProgrammable::ClockTooHigh {
            clock_khz: 594_000,
            ceiling_khz: 300_000,
        })
    );

    for why in [
        NotProgrammable::Malformed,
        NotProgrammable::Interlaced,
        NotProgrammable::PixelRepeated,
        NotProgrammable::ClockTooHigh {
            clock_khz: 594_000,
            ceiling_khz: 300_000,
        },
    ] {
        assert!(!why.describe().is_empty());
    }
}

#[test]
fn the_line_rate_the_mode_implies_is_the_clock_over_the_total() {
    assert_eq!(mode_line_rate_hz(&mode_1080p60()), Some(67_500));
    // 594 MHz over a 4400-pixel line.
    let four_k = Mode::from_blanking(
        594_000,
        3840,
        560,
        176,
        88,
        2160,
        90,
        8,
        10,
        ModeFlags::NONE,
        TimingSource::CtaVic(97),
    );
    assert_eq!(mode_line_rate_hz(&four_k), Some(135_000));
    assert_eq!(
        mode_line_rate_hz(&Mode {
            htotal: 0,
            ..four_k
        }),
        None
    );
}

#[test]
fn the_sink_advertised_1080p60_is_found_in_a_cta_extension() {
    let _guard = scheduler_test_context();
    let bytes = edid(base_block(None, 1), &[cta_block(&[16])]);
    let mut list = ModeList::new();
    let report = advertised_modes(&bytes, &mut list).expect("a valid EDID");
    assert!(
        report.added >= 1,
        "the CTA video data block must yield modes"
    );
    let reference: Vec<Mode> = list.modes().copied().filter(is_reference_timing).collect();
    assert_eq!(reference.len(), 1, "exactly one 1080p60 timing");
    assert_eq!(reference[0].clock_khz, 148_500);
}

// ---------------------------------------------------------------------------
// Prove it
// ---------------------------------------------------------------------------

/// A register file whose pipe is scanning: `PIPEDSL` advances by `lines` on
/// every read, exactly as a running counter does.
///
/// The mock keeps **one read hook per register**, so a test that installs a
/// second hook on `PIPEDSL_A` replaces this one rather than adding to it.  The
/// tests below build their register file in one place for that reason.
fn scanning_pipe(lines: u32) -> MockRegisters {
    let regs = MockRegisters::new();
    let reads = Cell::new(0u32);
    regs.on_read(PIPEDSL_A, move |value| {
        reads.set(reads.get() + 1);
        value + reads.get() * lines
    });
    regs
}

/// The surface address a test programs, 4 KiB aligned as §11 phase 3.2 wants.
const SURFACE: u32 = 0x1234_5000;

/// Every register a healthy device answers: the pipe scans, the plane is live,
/// the DDI is not idle, and no underrun is latched.
fn clean_device(lines: u32) -> MockRegisters {
    let regs = scanning_pipe(lines);
    healthy_peripherals(&regs);
    regs
}

/// The registers 6.2, 6.3 and 6.4 read, set as a working device would hold
/// them.  Separate from [`clean_device`] so that a test can install its own
/// `PIPEDSL` hook without replacing the one that makes the pipe scan.
fn healthy_peripherals(regs: &MockRegisters) {
    regs.set(PLANE_SURFLIVE_A, SURFACE);
    regs.set(DDI_BUF_CTL_A, 0x8000_0000); // ENABLE, and IS_IDLE clear
    regs.set(PIPESTAT_A, 0);
}

#[test]
fn a_clean_device_passes_every_check_with_the_readings_it_passed_on() {
    let _guard = scheduler_test_context();
    let regs = clean_device(337);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, Some(67_500)),
    );
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    // The one value the console handover tests.  It is only `ScanningOut` when
    // all four checks agreed, and it carries no failures when it is.
    let verdict = report.verdict();
    assert_eq!(verdict, ScanoutVerdict::ScanningOut);
    assert!(verdict.failures().is_empty());
    assert_eq!(verdict.unavailable_reason(), None);
    assert!(verdict.describe().contains("scanning out"), "{verdict:?}");

    let scan = report.scan.as_ref().expect("6.1 passed");
    assert_eq!(scan.samples.len(), 2, "the check stops at the first change");
    assert_eq!(scan.samples[0].micros, 0);
    assert_eq!(
        scan.samples[1].micros, SCAN_SAMPLE_INTERVAL_MICROS,
        "the samples are the interval apart"
    );
    assert_eq!(scan.samples[0].value, 337);
    assert_eq!(scan.samples[1].value, 674);
    // 337 lines in 5000 us.  The mock advances a whole number of lines per
    // sample because that is what an integer counter can do; the point is that
    // the arithmetic that turns it into a rate is exercised.
    assert_eq!(scan.observed_line_rate_hz, Some(67_400));
    assert_eq!(scan.expected_line_rate_hz, Some(67_500));
    assert_eq!(scan.rate_agrees(), Some(true));
    assert!(report.render().contains("agrees"), "{}", report.render());

    let surface = report.surface.as_ref().expect("6.2 passed");
    assert_eq!(surface.expected, SURFACE);
    assert_eq!(surface.live, SURFACE);
    assert_eq!(surface.polls, 1, "the address was already live");
    assert_eq!(surface.waited_micros, 0);

    assert_eq!(
        report.ddi.as_ref().expect("6.3 passed").ddi_buf_ctl,
        0x8000_0000
    );
    assert_eq!(report.underrun.as_ref().expect("6.4 passed").pipestat, 0);

    let text = report.render();
    for step in ["6.1", "6.2", "6.3", "6.4"] {
        assert!(text.contains(step), "{step} missing from:\n{text}");
    }
    assert!(text.contains("prove it"), "{text}");
}

#[test]
fn a_line_rate_far_from_the_mode_s_is_reported_without_failing_the_check() {
    let _guard = scheduler_test_context();
    // The pipe scans, but 100 lines per 5 ms is 20 kHz where the mode implies
    // 67.5 kHz: a PLL that locked to the wrong frequency looks like this.
    let regs = clean_device(100);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, Some(67_500)),
    );
    assert!(
        report.verdict().is_scanning_out(),
        "6.1 asks whether the pipe scans, and it does; a wrong rate is evidence, not this check's \
         failure"
    );
    let scan = report.scan.as_ref().expect("6.1 passed");
    assert_eq!(scan.observed_line_rate_hz, Some(20_000));
    assert_eq!(scan.rate_agrees(), Some(false));
    assert!(
        report.render().contains("DOES NOT AGREE"),
        "{}",
        report.render()
    );
}

#[test]
fn a_pipe_that_does_not_scan_is_a_named_failure() {
    let _guard = scheduler_test_context();
    let regs = clean_device(0);
    regs.set(PIPEDSL_A, 0x0000_1234);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    assert!(!report.verdict().is_scanning_out());
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::Scanning);
    match failure {
        ProveFailure::NotScanning { samples } => {
            assert_eq!(samples.len(), SCAN_SAMPLES, "the window is fully sampled");
            assert!(samples.iter().all(|sample| sample.value == 0x0000_1234));
            assert_eq!(
                samples.last().expect("a sample").micros,
                (SCAN_SAMPLES as u64 - 1) * SCAN_SAMPLE_INTERVAL_MICROS,
                "four samples span three intervals"
            );
        }
        other => panic!("expected NotScanning, got {other:?}"),
    }
    assert!(check.advice().contains("5.1"), "{}", check.advice());
    // Phase 6 reads and programs nothing, so a failure in one check does not
    // hide the others: the plane, DDI and underrun readings are all here.
    assert!(report.surface.is_ok());
    assert!(report.ddi.is_ok());
    assert!(report.underrun.is_ok());
    assert!(report.render().contains("first failure: 6.1"));
}

#[test]
fn a_pipe_that_starts_late_still_passes() {
    let _guard = scheduler_test_context();
    // The counter is still on its first line for three reads and then moves,
    // which is what a transcoder enabled microseconds ago looks like.  This
    // hook is the only one on PIPEDSL_A: the mock replaces rather than adds.
    let regs = MockRegisters::new();
    let reads = Cell::new(0u32);
    regs.on_read(PIPEDSL_A, move |_| {
        reads.set(reads.get() + 1);
        if reads.get() < 4 { 0x10 } else { 0x11 }
    });
    healthy_peripherals(&regs);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    assert_eq!(report.scan.as_ref().expect("6.1 passed").samples.len(), 4);
}

#[test]
fn a_plane_that_never_arms_is_a_named_failure() {
    let _guard = scheduler_test_context();
    let regs = clean_device(337);
    regs.set(PLANE_SURFLIVE_A, 0);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::SurfaceLive, "6.1 passed, so 6.2 is first");
    match failure {
        ProveFailure::SurfaceNotLive {
            expected,
            live,
            polls,
            waited_micros,
        } => {
            assert_eq!(*expected, SURFACE);
            assert_eq!(*live, 0);
            assert_eq!(*waited_micros, SURFACE_ARM_TIMEOUT_MICROS);
            // One poll at every step of the fake clock's 25 us, plus the first
            // one at zero: the bound is the timeout, not the poll count.
            assert_eq!(*polls, (SURFACE_ARM_TIMEOUT_MICROS / 25) as u32 + 1);
        }
        other => panic!("expected SurfaceNotLive, got {other:?}"),
    }
    assert!(check.advice().contains("4.3"), "{}", check.advice());
}

#[test]
fn a_plane_that_arms_on_a_later_frame_is_not_a_failure() {
    let _guard = scheduler_test_context();
    let regs = clean_device(337);
    let polls = Cell::new(0u32);
    regs.on_read(PLANE_SURFLIVE_A, move |_| {
        polls.set(polls.get() + 1);
        if polls.get() < 3 { 0 } else { SURFACE }
    });
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    let surface = report.surface.as_ref().expect("6.2 passed");
    assert_eq!(surface.polls, 3);
    assert_eq!(surface.waited_micros, 50, "two 25 us waits");
}

#[test]
fn the_surface_comparison_ignores_the_bits_that_are_not_address() {
    let _guard = scheduler_test_context();
    let regs = clean_device(337);
    // Bits below 12 are not address, and bit 2 is the decrypt flag, so a live
    // value that differs only there is the same surface.
    regs.set(PLANE_SURFLIVE_A, SURFACE | 0xfff);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    assert!(report.verdict().is_scanning_out(), "{}", report.render());

    // A difference in the address itself is not.
    let regs = clean_device(337);
    regs.set(PLANE_SURFLIVE_A, SURFACE + 0x1000);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    let (check, _) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::SurfaceLive);
}

#[test]
fn a_ddi_that_is_still_idle_is_a_named_failure() {
    let _guard = scheduler_test_context();
    let regs = clean_device(337);
    regs.set(DDI_BUF_CTL_A, 0x8000_0080); // ENABLE | IS_IDLE
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::DdiActive);
    assert_eq!(
        failure,
        &ProveFailure::DdiIdle {
            ddi_buf_ctl: 0x8000_0080
        }
    );
    assert!(check.advice().contains("5.2"), "{}", check.advice());
}

#[test]
fn an_underrun_points_at_the_watermarks() {
    let _guard = scheduler_test_context();
    let regs = clean_device(337);
    regs.set(PIPESTAT_A, PIPESTAT_FIFO_UNDERRUN | 0x0000_0002);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    assert!(!report.verdict().is_scanning_out());
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::FifoUnderrun, "6.1 to 6.3 passed");
    assert_eq!(
        failure,
        &ProveFailure::FifoUnderrun {
            pipestat: PIPESTAT_FIFO_UNDERRUN | 0x0000_0002
        }
    );
    // §11 phase 6.4's own advice: go back to the watermarks before changing
    // anything else.  The verdict has to say so, not just "failed".
    assert!(check.advice().contains("4.2"), "{}", check.advice());
    assert!(check.advice().contains("watermark"), "{}", check.advice());
    let text = report.render();
    assert!(text.contains("PIPE_FIFO_UNDERRUN_STATUS"), "{text}");
    assert!(text.contains("first failure: 6.4"), "{text}");
}

#[test]
fn a_register_outside_the_window_names_itself() {
    let _guard = scheduler_test_context();
    let regs = clean_device(337);
    regs.hide(PIPEDSL_A);
    regs.hide(PLANE_SURFLIVE_A);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, None),
    );
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::Scanning);
    assert_eq!(
        failure,
        &ProveFailure::Unreadable {
            check: CheckId::Scanning,
            register: "PIPEDSL_A"
        }
    );
    assert_eq!(
        report.surface,
        Err(ProveFailure::Unreadable {
            check: CheckId::SurfaceLive,
            register: "PLANE_SURFLIVE_A"
        })
    );
    assert!(describe_failure(failure).contains("PIPEDSL_A"));
}

#[test]
fn the_second_pipe_reads_the_second_pipe_s_registers() {
    let _guard = scheduler_test_context();
    // ProveTarget carries registers rather than a pipe index precisely so that
    // a mode set on pipe B is provable with the same code.
    let target = ProveTarget {
        surface: SURFACE,
        line_rate_hz: None,
        pipedsl: crate::drm::intel::regs::pipe::PIPEDSL_B,
        plane_surflive: crate::drm::intel::regs::pipe::PLANE_SURFLIVE_B,
        ddi_buf_ctl: crate::drm::intel::regs::ddi::DDI_BUF_CTL_B,
        pipestat: crate::drm::intel::regs::pipe::PIPESTAT_B,
    };
    let regs = MockRegisters::new();
    let reads = Cell::new(0u32);
    regs.on_read(target.pipedsl, move |value| {
        reads.set(reads.get() + 1);
        value + reads.get()
    });
    regs.set(target.plane_surflive, SURFACE);
    regs.set(target.ddi_buf_ctl, 0x8000_0000);
    let report = prove_it(&regs, &FakeClock::new(), &target);
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    // Pipe A's registers were never touched, so a target that had been wired
    // to the wrong pipe would have failed rather than passed.
    assert!(regs.writes().is_empty());
}

#[test]
fn a_verdict_that_is_not_scanning_out_carries_every_failure_as_a_reason() {
    let _guard = scheduler_test_context();
    // A device that fails all four checks at once: the pipe never moves, the
    // plane never arms, the DDI stays idle and an underrun is latched.  This is
    // the case a single boolean would erase: the reason has to name all four,
    // because they point at different phases.
    let regs = MockRegisters::new();
    regs.set(PIPEDSL_A, 0x0000_0001);
    regs.set(PLANE_SURFLIVE_A, 0);
    regs.set(DDI_BUF_CTL_A, 0x0000_0080); // ENABLE absent, IS_IDLE set
    regs.set(PIPESTAT_A, PIPESTAT_FIFO_UNDERRUN);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, Some(67_500)),
    );
    let verdict = report.verdict();
    assert!(!verdict.is_scanning_out());
    let checks: Vec<CheckId> = verdict.failures().iter().map(|(check, _)| *check).collect();
    assert_eq!(checks, CheckId::ALL.to_vec());
    let reason = verdict.unavailable_reason().expect("a reason to log");
    assert!(reason.contains("not scanning out"), "{reason}");
    for step in ["6.1", "6.2", "6.3", "6.4"] {
        assert!(reason.contains(step), "{step} missing from: {reason}");
    }
    // The advice attached is the first failure's, which is §11's order: a pipe
    // that is not scanning is the thing to fix before reading anything else.
    assert!(reason.contains("5.1"), "{reason}");
    // And it is a `String`, ready to hand to `screen::Unavailable::Failed`.
    assert!(report.render().contains("verdict:"), "{}", report.render());
}

#[test]
fn one_failure_gives_one_reason_and_not_a_general_warning() {
    let _guard = scheduler_test_context();
    let regs = clean_device(337);
    regs.set(PIPESTAT_A, PIPESTAT_FIFO_UNDERRUN);
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &ProveTarget::pipe_a(SURFACE, Some(67_500)),
    );
    let verdict = report.verdict();
    assert_eq!(verdict.failures().len(), 1);
    assert_eq!(verdict.failures()[0].0, CheckId::FifoUnderrun);
    let reason = verdict.unavailable_reason().expect("a reason to log");
    assert!(reason.contains("6.4"), "{reason}");
    assert!(reason.contains("PIPE_FIFO_UNDERRUN_STATUS"), "{reason}");
    assert!(reason.contains("watermark"), "{reason}");
    // The three checks that passed are not in the reason: a verdict that listed
    // everything would bury the one line that matters.
    for step in ["6.1", "6.2", "6.3"] {
        assert!(!reason.contains(step), "{step} in: {reason}");
    }
}
