//! The mode-choice policy, the whole §11 phase 3.2-to-6 sequence, and phase
//! 6's verdicts, against a modelled register file.
//!
//! What these tests establish: that the reference's preference and the mode
//! layer's decision reconcile the way the design document says they do, that a
//! refusal is a refusal and not a guessed timing; that the sequence writes the
//! registers in the reference's order, computes before it writes, paints the
//! pattern before it arms the plane, and writes nothing after the proof starts;
//! that each named failure leaves the state the design document's failure table
//! claims; and that each of §11 phase 6's four checks fails by name with the
//! reading it failed from.
//!
//! What they cannot establish is anything about a display engine: every
//! register here is a `BTreeMap` entry, the framebuffer is host memory, and no
//! part of this driver has run on the target.

use alloc::{boxed::Box, rc::Rc, vec, vec::Vec};
use core::cell::Cell;

use super::*;
use crate::{
    drm::{
        intel::{
            fb,
            gmbus::tests::FakeClock,
            gtt, power,
            regs::{
                self,
                ddi::{DDI_BUF_CTL_A, DDI_BUF_CTL_B},
                mock::MockRegisters,
                pipe::{PIPEDSL_A, PIPESTAT_A, PLANE_CTL_A, PLANE_SURF_A, PLANE_SURFLIVE_A},
            },
            timing::TimingRegister,
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
fn assemble_edid(base: Vec<u8>, extensions: &[Vec<u8>]) -> Vec<u8> {
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
        mode_line_rate_hz(&choice.into_mode().expect("a mode")),
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
    let bytes = assemble_edid(base_block(Some(&four_k), 1), &[cta_block(&[16, 97])]);
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
    let bytes = assemble_edid(base_block(Some(&native), 1), &[cta_block(&[16])]);
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
    assert!(
        choice.into_mode().is_err(),
        "nothing may be programmed here"
    );
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
    let bytes = assemble_edid(base_block(None, 1), &[cta_block(&[16])]);
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

/// The surface address a test programs, 4 KiB aligned as §11 phase 3.2 wants.
const SURFACE: u32 = 0x1234_5000;

/// The pipe program the prove-it tests read back, for the target's mode.
fn prove_program() -> PipeProgram {
    pipe::compute(
        Pipe::A,
        &mode_1080p60(),
        pipe::PlaneSurface {
            ggtt_address: u64::from(SURFACE),
            stride_bytes: 1920 * 4,
        },
    )
    .expect("the target mode is what the reference works through")
}

/// The target phase 6 reads: pipe A, port A, and the line rate 1080p60 implies.
fn prove_target() -> ProveTarget {
    ProveTarget::port(Ddi::A, Some(67_500)).expect("DDI A has a DDI_BUF_CTL in the table")
}

/// What phase 6's three registers said before a healthy modeset: the underrun
/// bit clear, the plane not yet armed on our surface, and the DDI still idle.
fn pre_sample() -> PreSample {
    PreSample {
        pipestat: Some(0),
        plane_surflive: Some(0),
        ddi_buf_ctl: Some(0x0000_0080),
    }
}

/// A register file whose pipe counts lines, whose plane is armed on `program`'s
/// surface, whose DDI is out of idle, and whose `PIPESTAT` is clear.
///
/// `lines_per_read` is how far `PIPEDSL` advances between reads: at the
/// sampler's 1 ms interval, 67 lines per read is the 67.5 kHz a 1080p60 mode
/// runs at, near enough to agree with it inside the tolerance.
fn prove_mock(program: &PipeProgram, lines_per_read: u32) -> MockRegisters {
    let regs = MockRegisters::new();
    regs.set(PLANE_SURFLIVE_A, program.plane.surf);
    regs.set(PIPESTAT_A, 0);
    regs.set(DDI_BUF_CTL_A, 0x8200_0016);
    let reads = Cell::new(0u32);
    regs.on_read(PIPEDSL_A, move |value| {
        reads.set(reads.get() + 1);
        value + reads.get() * lines_per_read
    });
    regs
}

fn read_prove(regs: &MockRegisters) -> ProveReport {
    prove_it(
        regs,
        &FakeClock::new(),
        &prove_program(),
        &prove_target(),
        &pre_sample(),
    )
}

#[test]
fn a_clean_device_passes_every_check_with_the_readings_it_passed_on() {
    let _guard = scheduler_test_context();
    let program = prove_program();
    let regs = prove_mock(&program, 67);
    let report = read_prove(&regs);
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    // The one value the console handover tests.  It is only `ScanningOut` when
    // all four checks agreed, and it carries no failures when it is.
    let verdict = report.verdict();
    assert_eq!(verdict, ScanoutVerdict::ScanningOut);
    assert!(verdict.failures().is_empty());
    assert_eq!(verdict.unavailable_reason(), None);
    assert!(verdict.describe().contains("scanning out"), "{verdict:?}");

    let scan = report.scan.as_ref().expect("6.1 passed");
    assert_eq!(
        scan.values.len(),
        pipe::SCANLINE_SAMPLES,
        "the sampler takes its whole window when it has to"
    );
    // The timestamps are `pipe`'s now, and they are what the elapsed time and
    // the rate come from.
    assert_eq!(scan.samples.len(), pipe::SCANLINE_SAMPLES);
    assert_eq!(
        scan.elapsed_micros(),
        (pipe::SCANLINE_SAMPLES as u64 - 1) * pipe::SCANLINE_INTERVAL_MICROS
    );
    assert_eq!(
        scan.values,
        [67, 134, 201, 268],
        "PIPEDSL advanced per read"
    );
    // 67 lines in 1 ms is 67 kHz against the mode's 67.5 kHz.
    assert_eq!(scan.observed_line_rate_hz, Some(67_000));
    assert_eq!(scan.expected_line_rate_hz, Some(67_500));
    assert_eq!(scan.rate_agrees(), Some(true));

    let surface = report.surface.as_ref().expect("6.2 passed");
    assert_eq!(surface.expected, program.plane.surf);
    assert_eq!(surface.live, program.plane.surf);
    assert!(!surface.pre_matching, "nothing named it before the write");

    assert_eq!(
        report.ddi.as_ref().expect("6.3 passed").ddi_buf_ctl,
        0x8200_0016
    );
    assert_eq!(report.underrun.as_ref().expect("6.4 passed").pipestat, 0);
    // The reading `scanout::Verdict::Scanning` wants, straight from the report.
    assert_eq!(
        report.surflive(),
        Some(u64::from(program.plane.surf)),
        "the address field is the graphics address"
    );

    let text = report.render();
    for step in ["6.1", "6.2", "6.3", "6.4"] {
        assert!(text.contains(step), "{step} missing from:\n{text}");
    }
    assert!(text.contains("prove it"), "{text}");
}

#[test]
fn a_line_rate_far_from_the_mode_s_is_reported_without_failing_the_check() {
    let _guard = scheduler_test_context();
    // The pipe scans, but 100 lines per millisecond is 100 kHz where the mode
    // implies 67.5 kHz: a PLL that locked to the wrong frequency looks like
    // this, and phase 6.1 is not the check that fails for it.
    let regs = prove_mock(&prove_program(), 100);
    let report = read_prove(&regs);
    assert!(
        report.verdict().is_scanning_out(),
        "a wrong rate is evidence, not 6.1's failure"
    );
    let scan = report.scan.as_ref().expect("6.1 passed");
    assert_eq!(scan.observed_line_rate_hz, Some(100_000));
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
    let regs = prove_mock(&prove_program(), 0);
    regs.set(PIPEDSL_A, 0x0000_1234);
    let report = read_prove(&regs);
    assert!(!report.verdict().is_scanning_out());
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::Scanning);
    match failure {
        ProveFailure::NotScanning { samples } => {
            assert_eq!(samples.len(), pipe::SCANLINE_SAMPLES);
            assert!(samples.iter().all(|sample| sample.line == 0x0000_1234));
            // The sampler still spaced them out, which is what makes "the same
            // line over three milliseconds" a measurement rather than one read
            // repeated.
            assert_eq!(
                samples.last().expect("a sample").micros,
                (pipe::SCANLINE_SAMPLES as u64 - 1) * pipe::SCANLINE_INTERVAL_MICROS
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
fn a_plane_that_has_not_latched_is_a_named_failure_and_is_not_a_wrong_address() {
    let _guard = scheduler_test_context();
    let regs = prove_mock(&prove_program(), 67);
    regs.set(PLANE_SURFLIVE_A, 0);
    let report = read_prove(&regs);
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::SurfaceLive, "6.1 passed, so 6.2 is first");
    match failure {
        ProveFailure::SurfaceNotLatched {
            expected,
            waited_micros,
            pre_matching,
        } => {
            assert_eq!(*expected, prove_program().plane.surf);
            // The wait is `pipe`'s: about two frame times, computed from the
            // mode -- 2200 x 1125 over 148.5 MHz is 16 666 us a frame.
            let frame = 2200u64 * 1125 * 1000 / 148_500;
            assert!(
                *waited_micros >= 2 * frame,
                "waited {waited_micros} us, less than two frame times"
            );
            assert!(
                *waited_micros < 2 * frame + 100,
                "waited {waited_micros} us, more than the deadline allows"
            );
            assert!(!*pre_matching);
        }
        other => panic!("expected SurfaceNotLatched, got {other:?}"),
    }
    // "Not latched yet" is not evidence that the address was rejected, and
    // neither the name nor the advice may say it was.
    let text = describe_failure(failure);
    assert!(text.contains("has not latched"), "{text}");
    assert!(text.contains("Read the 6.1 verdict first"), "{text}");
    assert!(!text.contains("rejected"), "{text}");
    assert!(check.advice().contains("4.3"), "{}", check.advice());
    // A zero read-back is not an address, so there is nothing to hand WS-1.
    assert_eq!(report.surflive(), None);
}

#[test]
fn a_plane_that_arms_on_a_later_poll_is_not_a_failure() {
    let _guard = scheduler_test_context();
    let program = prove_program();
    let regs = prove_mock(&program, 67);
    let polls = Rc::new(Cell::new(0u32));
    let counted = Rc::clone(&polls);
    let field = program.plane.surf;
    regs.on_read(PLANE_SURFLIVE_A, move |_| {
        counted.set(counted.get() + 1);
        if counted.get() < 3 { 0 } else { field }
    });
    let report = read_prove(&regs);
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    // The latch arrived on the third read, well inside `pipe`'s two-frame
    // window: a late latch is not a failure, which is the whole reason the
    // poll exists.
    assert_eq!(polls.get(), 3);
    assert_eq!(
        report.surface.as_ref().expect("6.2 passed").live,
        program.plane.surf
    );
}

#[test]
fn the_surface_comparison_ignores_the_bits_that_are_not_address() {
    let _guard = scheduler_test_context();
    let program = prove_program();
    let regs = prove_mock(&program, 67);
    // Bits below 12 are not address, and bit 2 is the decrypt flag, so a live
    // value that differs only there is the same surface.
    regs.set(PLANE_SURFLIVE_A, program.plane.surf | 0xfff);
    let report = read_prove(&regs);
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    assert_eq!(report.surflive(), Some(u64::from(program.plane.surf)));

    // A difference in the address itself is not, and it is a *wrong address*
    // rather than a late latch: `pipe` distinguishes the two, because only one
    // of them is worth waiting for.
    let regs = prove_mock(&prove_program(), 67);
    regs.set(PLANE_SURFLIVE_A, program.plane.surf + 0x1000);
    let report = read_prove(&regs);
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::SurfaceLive);
    assert!(
        matches!(failure, ProveFailure::SurfaceWrongAddress { .. }),
        "expected a wrong address, got {failure:?}"
    );
    assert_eq!(
        report.surflive(),
        Some(u64::from(program.plane.surf + 0x1000)),
        "a wrong but real address is still evidence"
    );
}

#[test]
fn a_surface_that_was_already_live_says_the_read_back_proves_nothing() {
    let _guard = scheduler_test_context();
    let program = prove_program();
    let regs = prove_mock(&program, 67);
    let pre = PreSample {
        plane_surflive: Some(program.plane.surf),
        ..pre_sample()
    };
    let report = prove_it(&regs, &FakeClock::new(), &program, &prove_target(), &pre);
    assert!(report.verdict().is_scanning_out());
    assert!(
        report.surface.as_ref().expect("6.2 passed").pre_matching,
        "the pre-sample named this surface, so the pass is not attributable"
    );
    assert!(
        report.render().contains("does not attribute"),
        "{}",
        report.render()
    );
}

#[test]
fn a_ddi_that_is_still_idle_is_a_named_failure() {
    let _guard = scheduler_test_context();
    let regs = prove_mock(&prove_program(), 67);
    regs.set(DDI_BUF_CTL_A, 0x8000_0080); // ENABLE | IS_IDLE
    let report = read_prove(&regs);
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::DdiActive);
    assert_eq!(
        failure,
        &ProveFailure::DdiIdle {
            ddi_buf_ctl: 0x8000_0080,
            pre_active: false
        }
    );
    assert!(check.advice().contains("5.2"), "{}", check.advice());
    assert!(
        describe_failure(failure).contains("never came up"),
        "{}",
        describe_failure(failure)
    );

    // A DDI that was already out of idle and is idle now is a different story,
    // and the verdict says which one it is.
    let pre = PreSample {
        ddi_buf_ctl: Some(0x8000_0016),
        ..pre_sample()
    };
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &prove_program(),
        &prove_target(),
        &pre,
    );
    let failure = report.ddi.as_ref().expect_err("still idle");
    assert!(describe_failure(failure).contains("went back to idle"));
}

#[test]
fn an_underrun_points_at_the_watermarks() {
    let _guard = scheduler_test_context();
    let regs = prove_mock(&prove_program(), 67);
    regs.set(PIPESTAT_A, pipe::PIPE_FIFO_UNDERRUN_STATUS | 0x0000_0002);
    let report = read_prove(&regs);
    assert!(!report.verdict().is_scanning_out());
    let (check, failure) = report.failure().expect("a failure");
    assert_eq!(check, CheckId::FifoUnderrun, "6.1 to 6.3 passed");
    assert_eq!(
        failure,
        &ProveFailure::FifoUnderrun {
            pipestat: pipe::PIPE_FIFO_UNDERRUN_STATUS | 0x0000_0002,
            pre_existing: false,
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
fn an_underrun_bit_that_was_already_set_says_so() {
    let _guard = scheduler_test_context();
    // The pre-sample saw the sticky bit set, so the verdict must not blame this
    // modeset for it: the register table declares PIPESTAT read-only and
    // nothing can clear it before the plane is armed.
    let regs = prove_mock(&prove_program(), 67);
    regs.set(PIPESTAT_A, pipe::PIPE_FIFO_UNDERRUN_STATUS);
    let pre = PreSample {
        pipestat: Some(pipe::PIPE_FIFO_UNDERRUN_STATUS),
        ..pre_sample()
    };
    let report = prove_it(
        &regs,
        &FakeClock::new(),
        &prove_program(),
        &prove_target(),
        &pre,
    );
    let failure = report.underrun.as_ref().expect_err("the bit is set");
    assert_eq!(
        failure,
        &ProveFailure::FifoUnderrun {
            pipestat: pipe::PIPE_FIFO_UNDERRUN_STATUS,
            pre_existing: true,
        }
    );
    let text = describe_failure(failure);
    assert!(text.contains("already set before this modeset"), "{text}");
    assert!(text.contains("sticky"), "{text}");
    // The reason the console stays put carries the attribution too.
    let reason = report.verdict().unavailable_reason().expect("a reason");
    assert!(
        reason.contains("already set before this modeset"),
        "{reason}"
    );
}

#[test]
fn a_register_outside_the_window_names_itself() {
    let _guard = scheduler_test_context();
    let regs = prove_mock(&prove_program(), 67);
    regs.hide(PIPEDSL_A);
    regs.hide(PLANE_SURFLIVE_A);
    let report = read_prove(&regs);
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
    assert_eq!(report.surflive(), None, "nothing readable to report");
}

#[test]
fn a_verdict_that_is_not_scanning_out_carries_every_failure_as_a_reason() {
    let _guard = scheduler_test_context();
    // A device that fails all four checks at once: the pipe never moves, the
    // plane never arms, the DDI stays idle and an underrun is latched.  This is
    // the case a single boolean would erase: the reason has to name all four,
    // because they point at different phases.
    let program = prove_program();
    let regs = prove_mock(&program, 0);
    regs.set(PIPEDSL_A, 0x0000_0001);
    regs.set(PLANE_SURFLIVE_A, 0);
    regs.set(DDI_BUF_CTL_A, 0x0000_0080);
    regs.set(PIPESTAT_A, pipe::PIPE_FIFO_UNDERRUN_STATUS);
    let report = read_prove(&regs);
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
    // And it is a `String`, ready to hand to `scanout::Verdict::NotScanning`.
    assert!(report.render().contains("verdict:"), "{}", report.render());
}

#[test]
fn one_failure_gives_one_reason_and_not_a_general_warning() {
    let _guard = scheduler_test_context();
    let program = prove_program();
    let regs = prove_mock(&program, 67);
    regs.set(PIPESTAT_A, pipe::PIPE_FIFO_UNDERRUN_STATUS);
    let report = read_prove(&regs);
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

#[test]
fn a_pipe_that_starts_late_still_passes() {
    let _guard = scheduler_test_context();
    // The counter is still on its first line for three reads and then moves,
    // which is what a transcoder enabled microseconds ago looks like.
    let regs = prove_mock(&prove_program(), 0);
    let reads = Cell::new(0u32);
    regs.on_read(PIPEDSL_A, move |_| {
        reads.set(reads.get() + 1);
        if reads.get() < 4 { 0x10 } else { 0x11 }
    });
    let report = read_prove(&regs);
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    assert_eq!(
        report.scan.as_ref().expect("6.1 passed").values.len(),
        pipe::SCANLINE_SAMPLES
    );
}

#[test]
fn the_second_pipe_reads_the_second_pipe_s_registers() {
    let _guard = scheduler_test_context();
    // The program carries its pipe, so a mode set on pipe B is proved against
    // pipe B's registers with the same code.
    let program = pipe::compute(
        Pipe::B,
        &mode_1080p60(),
        pipe::PlaneSurface {
            ggtt_address: u64::from(SURFACE),
            stride_bytes: 1920 * 4,
        },
    )
    .expect("pipe B can carry the same mode");
    let regs = MockRegisters::new();
    let reads = Cell::new(0u32);
    regs.on_read(pipe::Pipe::B.pipedsl(), move |value| {
        reads.set(reads.get() + 1);
        value + reads.get()
    });
    regs.set(program.pipe.plane_surflive(), program.plane.surf);
    regs.set(program.pipe.pipestat(), 0);
    regs.set(DDI_BUF_CTL_B, 0x8200_0016);
    let target = ProveTarget::port(Ddi::B, Some(67_500)).expect("DDI B has a DDI_BUF_CTL");
    let report = prove_it(&regs, &FakeClock::new(), &program, &target, &pre_sample());
    assert!(report.verdict().is_scanning_out(), "{}", report.render());
    // Pipe A's registers were never read, so a target wired to the wrong pipe
    // would have failed rather than passed.
    assert!(regs.writes().is_empty());
}

// ---------------------------------------------------------------------------
// The sequence: reference §11 phases 3.2 to 6
// ---------------------------------------------------------------------------

/// The 19.2 MHz reference strap, which the platform's CDCLK table has rows for.
const STRAP_19_2: u32 = 1 << 29;

/// A page table with enough entries for a 1080p surface's run.
fn test_gtt() -> gtt::Gtt {
    gtt::Gtt::over(Box::new(gtt::mock::MockPageTable::new(4096)))
        .expect("a page table with room for the surface")
}

/// The framebuffer the sequence scans out: 1920x1080 XRGB8888, which is the
/// mode every test here uses.
fn test_surface() -> fb::Surface {
    fb::Surface::allocate(&test_gtt(), 1920, 1080, fb::Format::Xrgb8888)
        .expect("the host page arena can hold an 8 MiB surface")
}

/// §8.5's HDMI translation values are a `[GAP]`; these are a test fixture with
/// a `source` string that says so, exactly as `output`'s own tests use.
fn test_swing() -> SwingProgram {
    SwingProgram {
        level: 2,
        dw2: [0x0C; 4],
        dw4: [0x30, 0x31, 0x31, 0x31],
        dw5_training_disabled: 0x0000_0000,
        dw5_training_enabled: 0x0002_0000,
        dw7: [0x0071; 4],
        source: "test fixture, not sourced from the reference",
    }
}

/// A mock of a machine whose display engine works end to end: CDCLK is up and
/// legal, the port PLL locks, the DDI leaves idle, the plane arms when
/// `PLANE_SURF` is written, and the pipe counts lines.
///
/// The returned cell is the value `PLANE_SURF` was written with, which is what
/// `PLANE_SURFLIVE` reads back; a test that wants to watch the proof's reads
/// replaces that hook and must return the same cell, because the mock keeps one
/// hook per register.
///
/// One hook per register, which is the mock's rule: a second `derive` on the
/// same register replaces the first.
fn working_device(program_lines_per_read: u32) -> (MockRegisters, Rc<Cell<u32>>) {
    let regs = MockRegisters::new();
    // §11 phase 1.4's outcome: 19.2 MHz reference, ratio 27, divide by 1.5, so
    // 172.8 MHz -- the first row of the platform's CDCLK table.
    regs.set(regs::SKL_DSSM, STRAP_19_2);
    regs.set(regs::CDCLK_PLL_ENABLE, (1 << 31) | (1 << 30) | 27);
    regs.set(regs::CDCLK_CTL, 1 << 22);
    regs.derive(regs::dpll::DPLL0_ENABLE, |value| {
        let mut stored = value;
        if value & (1 << 27) != 0 {
            stored |= 1 << 26; // POWER_STATE
        }
        if value & (1 << 31) != 0 {
            stored |= 1 << 30; // LOCK
        }
        stored
    });
    regs.derive(DDI_BUF_CTL_A, |value| {
        if value & (1 << 31) != 0 {
            value & !DDI_BUF_CTL_IS_IDLE
        } else {
            value | DDI_BUF_CTL_IS_IDLE
        }
    });
    // The DDI-IO power well reports its state once it is requested.
    regs.derive(regs::ICL_PWR_WELL_CTL_DDI2, |value| {
        value | power::well_state(power::DDI_IO_A.index)
    });
    // The plane arms when `PLANE_SURF` is written: `PLANE_SURFLIVE` reads back
    // whatever was written, which is what §5.6's "PLANE_SURF is the commit"
    // means for a mock.
    let armed = Rc::new(Cell::new(0u32));
    let stored = Rc::clone(&armed);
    regs.derive(PLANE_SURF_A, move |value| {
        stored.set(value);
        value
    });
    let live = Rc::clone(&armed);
    regs.on_read(PLANE_SURFLIVE_A, move |_| live.get());
    let reads = Cell::new(0u32);
    regs.on_read(PIPEDSL_A, move |value| {
        reads.set(reads.get() + 1);
        value + reads.get() * program_lines_per_read
    });
    regs.set(PIPESTAT_A, 0);
    (regs, armed)
}

/// The pixel at the top-right corner of a surface: outside frame 0's marker
/// (which sits at the origin) and the last bar's near-black grey, which is not
/// the zero the allocator leaves.  Reading it is how a test says "the pattern
/// was painted" without depending on where the marker is.
fn top_right_pixel(surface: &fb::Surface) -> u32 {
    let mut bytes = [0u8; 4];
    surface
        .read_bytes((surface.width() as usize - 1) * 4, &mut bytes)
        .expect("the last pixel of the first row");
    u32::from_le_bytes(bytes)
}

/// The colour the top-right pixel must have if the pattern is there.
fn top_right_expected() -> u32 {
    pattern::BAR_COLORS[pattern::BAR_COUNT - 1]
}

/// The EDID whose preferred timing is 1080p60, and the plan the mode layer
/// makes from it.
fn plan_1080p60() -> (Vec<u8>, ModePlan) {
    let bytes = base_block(Some(&mode_1080p60()), 0);
    let plan = plan_modeset(&bytes, &Constraints::unlimited());
    (bytes, plan)
}

/// The request every end-to-end test makes: pipe A, port A, the reference's
/// 1080p60 plan, the framebuffer, and the swing values without which
/// `output::plan` refuses.
fn request<'a>(plan: &'a ModePlan, edid: &'a [u8], surface: &'a fb::Surface) -> ModeRequest<'a> {
    ModeRequest::new(
        Ddi::A,
        Pipe::A,
        plan,
        edid,
        surface,
        PllFieldEncoding::Named,
    )
    .with_swing(test_swing())
}

#[test]
fn the_whole_sequence_writes_in_the_reference_s_order_and_stops_before_the_reads() {
    let _guard = scheduler_test_context();
    let (edid, plan) = plan_1080p60();
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    let regs = Rc::new(regs);
    // What the write log held when phase 6's first `PIPEDSL` read happened, so
    // that "nothing is written after the proof starts" is a measurement rather
    // than a reading of the code.  `PIPEDSL` is read by nothing but phase 6 --
    // the pre-sample reads the other three registers -- so a write after this
    // point would be a write during the proof.
    let writes_at_first_proof_read = Rc::new(Cell::new(usize::MAX));
    {
        let count = Rc::clone(&writes_at_first_proof_read);
        let mock = Rc::clone(&regs);
        let reads = Cell::new(0u32);
        regs.on_read(PIPEDSL_A, move |value| {
            if count.get() == usize::MAX {
                count.set(mock.writes().len());
            }
            reads.set(reads.get() + 1);
            value + reads.get() * 67
        });
    }

    let outcome = set_mode(&*regs, &FakeClock::new(), &request(&plan, &edid, &surface))
        .expect("the mock models a working device");
    assert!(outcome.verdict().is_scanning_out(), "{}", outcome.render());
    assert_eq!(outcome.mode.clock_khz, 148_500);

    let writes = regs.writes();
    let names: Vec<&'static str> = writes.iter().map(|(name, _)| *name).collect();

    // 1. The shadow group comes first and is exactly the plan's own list --
    //    not a copy of it kept in this test -- and it contains no `PLANE_SURF`:
    //    the arm is a separate step now.
    let planned: Vec<&'static str> = outcome
        .pipe_state
        .writes
        .iter()
        .map(|write| write.register.name())
        .collect();
    assert!(!planned.is_empty(), "the pipe program writes something");
    assert_eq!(
        &names[..planned.len()],
        &planned[..],
        "the first writes are the plan's, in the plan's order"
    );
    assert!(
        !planned.contains(&PLANE_SURF_A.name()),
        "the shadow half does not arm the plane: {planned:?}"
    );
    let arm_names: Vec<&'static str> = outcome
        .arm
        .writes
        .iter()
        .map(|write| write.register.name())
        .collect();

    // 3. The timing registers precede the DDB, the watermarks and the plane.
    let timing_names: Vec<&'static str> = [
        TimingRegister::Htotal,
        TimingRegister::Hblank,
        TimingRegister::Hsync,
        TimingRegister::Vtotal,
        TimingRegister::Vblank,
        TimingRegister::Vsync,
    ]
    .iter()
    .map(|register| outcome.pipe.pipe.timing(*register).name())
    .collect();
    let last_timing = timing_names
        .iter()
        .filter_map(|name| names.iter().position(|candidate| candidate == name))
        .max()
        .expect("the timings were written");
    let first_plane_group = names
        .iter()
        .position(|name| {
            *name == outcome.pipe.pipe.plane_buf_cfg().name() || name.starts_with("PLANE_")
        })
        .expect("the plane group was written");
    assert!(
        last_timing < first_plane_group,
        "every timing register precedes the DDB, the watermarks and the plane: {names:?}"
    );

    // 4. The whole output sequence follows the whole shadow group, and rewrites
    //    none of it.  "The output" is what lies between the shadow group and
    //    the arm pair, which is the order the arm split created.
    let pipe_owned: Vec<&'static str> = planned.clone();
    let output_names = &names[planned.len()..names.len() - arm_names.len()];
    assert!(!output_names.is_empty(), "the output sequence writes");
    assert!(
        !output_names.iter().any(|name| pipe_owned.contains(name)),
        "the output does not rewrite the pipe's registers: {output_names:?}"
    );

    // 5. The DDI-to-PLL mapping is two separate writes, and the DDI buffer is
    //    the last the output stage writes -- the arm pair comes after it now,
    //    and assertion 6 is where that belongs.
    let dpclka = output_names
        .iter()
        .filter(|name| **name == "ICL_DPCLKA_CFGCR0")
        .count();
    assert_eq!(dpclka, 2, "the mapping, then the clock-off clear");
    assert_eq!(
        *output_names.last().expect("the output writes"),
        DDI_BUF_CTL_A.name(),
        "DDI_BUF_CTL is the output's last write, after the IS_IDLE poll"
    );
    // And the transcoder is enabled before the DDI buffer that consumes it.
    let transconf = output_names
        .iter()
        .position(|name| *name == "PIPECONF_A")
        .expect("TRANSCONF is written");
    let ddi_buf = output_names
        .iter()
        .position(|name| *name == DDI_BUF_CTL_A.name())
        .expect("DDI_BUF_CTL is written");
    assert!(transconf < ddi_buf);

    // 6. The arm pair is last: `PLANE_CTL` then `PLANE_SURF`, adjacent, and
    //    nothing follows them but the proof's reads.  This is the order the
    //    arm split exists for -- the arm latches at a vblank, and the vblank
    //    only exists once the output above is enabled.
    assert_eq!(
        arm_names,
        [PLANE_CTL_A.name(), PLANE_SURF_A.name()],
        "the arm is PLANE_CTL then PLANE_SURF"
    );
    assert_eq!(
        &names[names.len() - arm_names.len()..],
        &arm_names[..],
        "the arm pair is the last two writes of the modeset: {names:?}"
    );
    assert_eq!(
        names.len() - arm_names.len(),
        planned.len() + output_names.len(),
        "the sequence is exactly the shadow group, the output and the arm"
    );

    // 7. Nothing is written after phase 6's first read.
    assert_ne!(
        writes_at_first_proof_read.get(),
        usize::MAX,
        "phase 6 read something"
    );
    assert_eq!(
        writes.len(),
        writes_at_first_proof_read.get(),
        "the proof wrote nothing: {:?}",
        &names[writes_at_first_proof_read.get()..]
    );

    // 8. And the framebuffer holds the pattern, because §11 6.5 was painted
    //    before the plane could scan it.
    assert_eq!(outcome.pattern.height, 1080);
    assert_eq!(
        top_right_pixel(&surface),
        top_right_expected(),
        "the pattern is painted, not the black the allocator left"
    );
}

#[test]
fn a_refused_arm_leaves_everything_else_programmed_and_the_pattern_in_the_framebuffer() {
    let _guard = scheduler_test_context();
    // §3.1's ordering claim, as a measurement: the fill happens before the
    // first register write, so a failure at the arm leaves a framebuffer that
    // is already the pattern rather than a black one.
    let (edid, plan) = plan_1080p60();
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    regs.refuse(PLANE_SURF_A);
    let result = set_mode(&regs, &FakeClock::new(), &request(&plan, &edid, &surface));
    let Err(error) = result else {
        panic!("PLANE_SURF refuses the write, so the sequence must fail")
    };
    assert!(
        matches!(error, ModesetError::Arm(_)),
        "the arm is its own step now: {error:?}"
    );
    assert!(
        error.describe().contains("PLANE_SURF"),
        "{}",
        error.describe()
    );
    assert_eq!(
        top_right_pixel(&surface),
        top_right_expected(),
        "the fill happened before the register write that failed"
    );
    // The arm runs *after* the output now, so everything before it happened:
    // `PLANE_CTL` landed, the PLL came up, the DDI is enabled -- and
    // `PLANE_SURF`, the commit, did not.  That is a worse state than a
    // half-programmed pipe, and it is exactly what the sequence reports
    // instead of papering over: the mode is up and nothing is scanned out.
    let names: Vec<&str> = regs.writes().iter().map(|(name, _)| *name).collect();
    assert_eq!(names.last(), Some(&PLANE_CTL_A.name()));
    assert!(names.contains(&"DPLL0_ENABLE"));
    assert!(names.contains(&DDI_BUF_CTL_A.name()));
    assert!(!names.contains(&PLANE_SURF_A.name()));
}

#[test]
fn a_failure_partway_through_phase_five_stops_before_the_proof() {
    let _guard = scheduler_test_context();
    // §11 phase 5.7's real-hardware failure: the DDI never leaves idle.  The
    // pipe is already programmed, so this is exactly the row of the design's
    // failure table that leaves a pipe running into an output that is not up.
    let (edid, plan) = plan_1080p60();
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    regs.derive(DDI_BUF_CTL_A, |_| DDI_BUF_CTL_IS_IDLE);
    let reads = Rc::new(Cell::new(0u32));
    let counted = Rc::clone(&reads);
    regs.on_read(PIPEDSL_A, move |value| {
        counted.set(counted.get() + 1);
        value
    });
    let result = set_mode(&regs, &FakeClock::new(), &request(&plan, &edid, &surface));
    let Err(error) = result else {
        panic!("the DDI never leaves idle, so the sequence must fail")
    };
    let text = error.describe();
    assert!(text.contains("IS_IDLE"), "{text}");
    assert!(text.contains("DDI"), "{text}");
    // The shadow half was programmed...
    let names: Vec<&str> = regs.writes().iter().map(|(name, _)| *name).collect();
    assert!(names.contains(&"PLANE_STRIDE_A"));
    assert_eq!(names.last(), Some(&"DDI_BUF_CTL(A)"));
    // ...and because the arm now runs *after* the output, a phase 5 failure
    // means the plane was never armed at all: no `PLANE_CTL`, no `PLANE_SURF`
    // and no phase 6.  Under the old order the plane was committed before the
    // output came up, so the same failure left a pipe scanning into an output
    // that was not there; now it leaves a configured but unarmed plane, which
    // is the state the arm split exists to make reachable.
    assert!(
        !names.contains(&PLANE_CTL_A.name()),
        "the arm must not have run: {names:?}"
    );
    assert!(!names.contains(&PLANE_SURF_A.name()));
    assert_eq!(reads.get(), 0, "the sequence stopped before the proof");
}

#[test]
fn no_usable_edid_refuses_before_anything_is_written() {
    let _guard = scheduler_test_context();
    let plan = plan_modeset(&[], &Constraints::unlimited());
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    let result = set_mode(&regs, &FakeClock::new(), &request(&plan, &[], &surface));
    let Err(error) = result else {
        panic!("there is no timing to program, so the sequence must refuse")
    };
    assert!(
        matches!(
            error,
            ModesetError::Refused(ModeRefusal::NoAdvertisedMode { .. })
        ),
        "{error:?}"
    );
    assert!(
        error
            .describe()
            .contains("firmware framebuffer is left alone")
    );
    assert!(regs.writes().is_empty(), "nothing was programmed");
    // And the firmware's framebuffer is untouched: not one pixel was painted.
    let mut first = [0xffu8; 4];
    surface.read_bytes(0, &mut first).expect("the first pixel");
    assert_eq!(first, [0, 0, 0, 0], "the surface is still as allocated");
}

#[test]
fn a_display_without_a_usable_cdclk_is_refused_by_name() {
    let _guard = scheduler_test_context();
    let (edid, plan) = plan_1080p60();
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    // The PLL is off, so `observe` reports the bypass clock and no table row.
    regs.set(regs::CDCLK_PLL_ENABLE, 0);
    let result = set_mode(&regs, &FakeClock::new(), &request(&plan, &edid, &surface));
    let Err(error) = result else {
        panic!("there is no pixel clock, so the sequence must refuse")
    };
    assert!(matches!(error, ModesetError::NoCdclk { .. }), "{error:?}");
    assert!(error.describe().contains("CDCLK"), "{}", error.describe());
    assert!(regs.writes().is_empty());
}

#[test]
fn a_mode_above_the_cdclk_ceiling_gives_way_to_the_reference_timing() {
    let _guard = scheduler_test_context();
    // The sink prefers 2560x1440@60 (241.5 MHz) and also offers 1080p60.  On a
    // 172.8 MHz CDCLK the preferred timing cannot be clocked at all, so the
    // sequence programs the reference's 1080p60 and says what it set aside.
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
    let bytes = assemble_edid(base_block(Some(&native), 1), &[cta_block(&[16])]);
    let plan = plan_modeset(&bytes, &Constraints::unlimited());
    assert_eq!(plan.selection.mode.clock_khz, 241_500);
    // The surface has to match the mode that is actually programmed.
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    let outcome = set_mode(&regs, &FakeClock::new(), &request(&plan, &bytes, &surface))
        .expect("1080p60 is what the sequence falls back to");
    assert_eq!(outcome.mode.clock_khz, 148_500);
    assert_eq!(outcome.pattern.width, 1920, "the pattern matches the mode");
    assert!(outcome.choice.overrode_the_mode_layer());
    assert!(
        outcome.render().contains("2560x1440"),
        "the log names what was set aside"
    );
}

#[test]
fn without_swing_values_the_sequence_refuses_before_any_write() {
    let _guard = scheduler_test_context();
    // §8.5's HDMI translation values are a `[GAP]`; `output::plan` refuses
    // rather than inventing them, and the refusal happens before the first
    // write because the whole program is computed first.
    let (edid, plan) = plan_1080p60();
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    let bare = ModeRequest::new(
        Ddi::A,
        Pipe::A,
        &plan,
        &edid,
        &surface,
        PllFieldEncoding::Named,
    );
    let result = set_mode(&regs, &FakeClock::new(), &bare);
    let Err(error) = result else {
        panic!("without swing values the output cannot be planned")
    };
    assert!(matches!(error, ModesetError::Output(_)), "{error:?}");
    assert!(regs.writes().is_empty(), "computing first means no writes");
    // The framebuffer is already the pattern, though: the fill precedes the
    // computation, and a caller that fixes the gap and runs again gets the
    // same frame rather than a stale one.
    assert_eq!(top_right_pixel(&surface), top_right_expected());
}

#[test]
fn a_port_the_table_has_no_ddi_buf_ctl_for_is_refused_by_name() {
    let _guard = scheduler_test_context();
    let (edid, plan) = plan_1080p60();
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    let request = ModeRequest::new(
        Ddi::C,
        Pipe::A,
        &plan,
        &edid,
        &surface,
        PllFieldEncoding::Named,
    )
    .with_swing(test_swing());
    let result = set_mode(&regs, &FakeClock::new(), &request);
    let Err(error) = result else {
        panic!("DDI C is not a combo-PHY port")
    };
    assert!(
        matches!(error, ModesetError::UnsupportedPort { ddi: Ddi::C }),
        "{error:?}"
    );
    assert!(error.describe().contains("Type-C"), "{}", error.describe());
    assert!(regs.writes().is_empty());
}

#[test]
fn a_surface_too_narrow_for_the_pattern_is_refused_by_name() {
    let _guard = scheduler_test_context();
    let (edid, plan) = plan_1080p60();
    // Four columns cannot carry eight bars, and the sequence says so rather
    // than indexing a bar that does not exist.
    let gtt = test_gtt();
    let surface = fb::Surface::allocate(&gtt, 4, 4, fb::Format::Xrgb8888)
        .expect("a four-pixel surface allocates");
    let (regs, _armed) = working_device(67);
    let result = set_mode(&regs, &FakeClock::new(), &request(&plan, &edid, &surface));
    let Err(error) = result else {
        panic!("four columns cannot carry eight bars")
    };
    assert_eq!(
        error,
        ModesetError::Pattern(PatternError::TooNarrow { width: 4 })
    );
    assert!(regs.writes().is_empty());
}

#[test]
fn a_verdict_that_fails_still_returns_the_outcome_for_the_console_gate() {
    let _guard = scheduler_test_context();
    // Phase 6 failing is not an error of the sequence: the mode *was*
    // programmed, and the caller needs the outcome -- the verdict, the
    // `PLANE_SURFLIVE` reading and the geometry -- to hand to `scanout`.
    let (edid, plan) = plan_1080p60();
    let surface = test_surface();
    let (regs, _armed) = working_device(0);
    regs.set(PIPEDSL_A, 0x0000_0010);
    let outcome = set_mode(&regs, &FakeClock::new(), &request(&plan, &edid, &surface))
        .expect("the mode was programmed");
    let verdict = outcome.verdict();
    assert!(!verdict.is_scanning_out());
    assert_eq!(verdict.failures().len(), 1);
    assert_eq!(verdict.failures()[0].0, CheckId::Scanning);
    // The `surflive` the console verdict wants is still there, because the
    // plane armed even though the pipe is not counting.
    assert_eq!(
        outcome.surflive(),
        Some(u64::from(outcome.pipe.plane.surf)),
        "the reading phase 6.2 made travels with the outcome"
    );
    assert!(
        outcome.render().contains("not scanning out"),
        "{}",
        outcome.render()
    );
}

#[test]
fn a_repaint_writes_a_different_frame_into_the_same_surface() {
    let _guard = scheduler_test_context();
    let (edid, plan) = plan_1080p60();
    let surface = test_surface();
    let (regs, _armed) = working_device(67);
    let first = set_mode(&regs, &FakeClock::new(), &request(&plan, &edid, &surface))
        .expect("the first frame");
    assert_eq!(first.pattern.frame, 0);
    let mut before = [0u8; 4];
    surface
        .read_bytes(
            first.pattern.marker.y * first.pattern.stride + first.pattern.marker.x * 4,
            &mut before,
        )
        .expect("the marker's first pixel");

    let second = set_mode(
        &regs,
        &FakeClock::new(),
        &request(&plan, &edid, &surface).with_frame(1),
    )
    .expect("the second frame");
    assert_eq!(second.pattern.frame, 1);
    assert_ne!(first.pattern.marker, second.pattern.marker);
    // The same pixel, after the second fill: frame 0's marker has moved away,
    // so the bar underneath is what is there now.  Reading where the *marker*
    // is in each frame would compare two marker pixels and find them equal,
    // which is what the first version of this test did.
    let mut after = [0u8; 4];
    surface
        .read_bytes(
            first.pattern.marker.y * first.pattern.stride + first.pattern.marker.x * 4,
            &mut after,
        )
        .expect("the marker's first pixel");
    assert_ne!(before, after, "the marker moved, so the bytes changed");
    assert_eq!(
        u32::from_le_bytes(before),
        pattern::marker_colour(pattern::BAR_COLORS[0]),
        "frame 0's marker sits on the white bar"
    );
    assert_eq!(
        u32::from_le_bytes(after),
        pattern::BAR_COLORS[0],
        "and the bar is back once it has moved on"
    );
}
