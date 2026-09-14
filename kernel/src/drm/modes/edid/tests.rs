use alloc::{format, vec::Vec};

use super::*;
use crate::drm::modes::{
    dmt,
    fixtures::{
        BaseBlockBuilder, CtaBlockBuilder, DetailedTimingSpec, RangeLimitsSpec, Rng, assemble,
        checksum_byte,
    },
    vic,
};

/// The block every parser test starts from: a digital 1.4 panel called
/// "TEST-PANEL" with one preferred 1920x1080@60 descriptor.
fn panel() -> BaseBlockBuilder {
    BaseBlockBuilder::new()
        .manufacturer(b"ACR")
        .product_code(0x1234)
        .serial_number(0xdead_beef)
        .manufacture(20, 2024)
        .screen_size_cm(34, 19)
        .gamma_byte(120)
        .chromaticity([640, 330, 300, 600, 150, 60, 313, 329])
        .detailed_timing(0, &DetailedTimingSpec::new(148_500, 1920, 1080))
}

fn parse(bytes: &[u8]) -> Edid<'_> {
    Edid::parse(bytes).expect("the fixture must parse strictly")
}

#[test]
fn base_block_fields_are_parsed_as_encoded() {
    let bytes = panel().build();
    let edid = parse(&bytes);
    let vendor = edid.vendor();
    assert_eq!(vendor.manufacturer.as_str(), "ACR");
    assert_eq!(vendor.manufacturer.as_bytes(), b"ACR");
    assert_eq!(vendor.product_code, 0x1234);
    assert_eq!(vendor.serial_number, 0xdead_beef);
    assert_eq!((vendor.week, vendor.year), (20, 2024));
    assert!(!vendor.model_year);
    assert_eq!(edid.version(), (1, 4));
    assert!(edid.warnings().is_clean());

    match edid.video_input() {
        VideoInput::Digital(input) => {
            assert_eq!(input.bit_depth, Some(8));
            assert_eq!(input.interface, DigitalInterface::DisplayPort);
        }
        VideoInput::Analog(_) => panic!("the fixture declares a digital input"),
    }

    let size = edid.screen_size();
    assert_eq!(size.width_mm, Some(340));
    assert_eq!(size.height_mm, Some(190));
    assert_eq!(edid.gamma_millis(), Some(2200));

    let features = edid.features();
    assert!(features.preferred_timing_is_native);
    assert!(features.continuous_frequency);
    assert!(features.ycbcr444, "bit 3 is YCbCr 4:4:4 for a digital sink");
    assert!(!features.ycbcr422);
    assert!(!features.srgb_is_default);
    assert!(!features.standby && !features.suspend && !features.active_off);

    let chromaticity = edid.chromaticity();
    assert_eq!(chromaticity.red_x, 640);
    assert_eq!(chromaticity.white_y, 329);
    assert_eq!(Chromaticity::permille(640), 625);
    assert!(!chromaticity.primaries_unset());
}

#[test]
fn preferred_detailed_timing_is_programmable() {
    let bytes = panel().build();
    let edid = parse(&bytes);
    assert!(edid.has_preferred_timing());
    let mode = edid.preferred_timing().expect("a preferred timing");
    assert_eq!(mode.clock_khz, 148_500);
    assert_eq!(mode.hdisplay, 1920);
    assert_eq!(mode.vdisplay, 1080);
    assert_eq!(mode.htotal, 2200);
    assert_eq!(mode.vtotal, 1125);
    assert_eq!(mode.hsync_start, 2008);
    assert_eq!(mode.hsync_end, 2052);
    assert_eq!(mode.vsync_start, 1084);
    assert_eq!(mode.vsync_end, 1089);
    assert_eq!(mode.refresh_millihz(), 60_000);
    assert_eq!(mode.source, TimingSource::EdidDtd { index: 0 });
    assert!(mode.is_well_formed());
    assert!(!mode.is_interlaced());
    // The descriptor's image size survives.
    let timing = edid.detailed_timings().next().expect("a timing");
    assert_eq!(timing.image_size_mm, Some((344, 194)));
    assert_eq!(timing.sync, SyncType::DigitalSeparate);
    assert_eq!(timing.stereo, StereoMode::None);
}

#[test]
fn an_edid_that_encodes_only_an_aspect_ratio_is_decoded_that_way() {
    // EDID 1.4 lets a sink state an aspect ratio instead of a size: the
    // width byte holds (ratio * 100) - 99 and the height byte is zero.
    let bytes = BaseBlockBuilder::new()
        .manufacturer(b"ACR")
        .screen_size_cm(79, 0)
        .detailed_timing(0, &DetailedTimingSpec::new(148_500, 1920, 1080))
        .build();
    let edid = parse(&bytes);
    let size = edid.screen_size();
    assert_eq!(size.width_mm, None);
    assert_eq!(size.height_mm, None);
    assert_eq!(size.landscape_aspect_ratio_percent, Some(178));
    assert_eq!(size.portrait_aspect_ratio_percent, None);

    // The portrait form is the mirror image.
    let bytes = BaseBlockBuilder::new()
        .manufacturer(b"ACR")
        .screen_size_cm(0, 79)
        .detailed_timing(0, &DetailedTimingSpec::new(148_500, 1920, 1080))
        .build();
    let edid = parse(&bytes);
    let size = edid.screen_size();
    assert_eq!(size.portrait_aspect_ratio_percent, Some(178));
    assert_eq!(size.landscape_aspect_ratio_percent, None);
}

#[test]
fn monitor_name_and_range_limits_descriptors_are_parsed() {
    let bytes = panel()
        .range_limits(1, &RangeLimitsSpec::cvt_panel())
        .monitor_name(2, b"TEST-PANEL")
        .build();
    let edid = parse(&bytes);
    let name = edid.monitor_name().expect("a monitor name");
    assert_eq!(name.as_str(), Some("TEST-PANEL"));
    assert_eq!(name.as_bytes(), b"TEST-PANEL");

    let limits = edid.range_limits().expect("a range limits descriptor");
    assert_eq!(limits.min_vertical_hz, 48);
    assert_eq!(limits.max_vertical_hz, 75);
    assert_eq!(limits.min_horizontal_khz, 30);
    assert_eq!(limits.max_horizontal_khz, 90);
    assert_eq!(limits.max_pixel_clock_khz, 170_000);
    match limits.kind {
        RangeLimitsKind::Cvt(cvt) => {
            assert_eq!((cvt.version, cvt.revision), (1, 1));
            assert!(cvt.standard_blanking);
            assert!(cvt.reduced_blanking);
            assert_eq!(cvt.preferred_refresh_hz, 60);
            assert_eq!(cvt.preferred_aspect, Some(CvtAspectRatio::Ratio16To10));
        }
        other => panic!("expected CVT range limits, got {other:?}"),
    }
    // The range admits the fixture's own preferred timing and rejects
    // something far outside it.
    let preferred = edid.preferred_timing().expect("preferred");
    assert!(limits.admits(&preferred));
    let too_fast = Mode {
        clock_khz: 600_000,
        ..preferred
    };
    assert!(!limits.admits(&too_fast));
    let too_slow = Mode {
        clock_khz: 10_000,
        ..preferred
    };
    assert!(!limits.admits(&too_slow));
}

#[test]
fn established_and_standard_timings_are_decoded() {
    // Bit 5 of byte 0x23 is 640x480@60, whose standard timing code is
    // 0x3140 - the "use DMT" escape the standard defines.
    let bytes = panel()
        .established_i_ii([0x20, 0x00, 0x00])
        .standard_timing(1, 640, 0b01, 60)
        .standard_timing_code(2, [0x01, 0x01])
        .standard_timing_code(3, [0x00, 0x55])
        .build();
    let edid = parse(&bytes);
    let established = edid.established_timings();
    assert_eq!(established.i_ii, [0x20, 0x00, 0x00]);
    assert_eq!(established.iii, None);

    let timings = edid.standard_timings();
    assert!(matches!(timings[0], StandardTiming::Unused { .. }));
    match timings[1] {
        StandardTiming::Named {
            code,
            hdisplay,
            vdisplay,
            refresh_hz,
            aspect,
        } => {
            assert_eq!(code, [0x31, 0x40]);
            assert_eq!(hdisplay, 640);
            assert_eq!(vdisplay, 480);
            assert_eq!(refresh_hz, 60);
            assert_eq!(aspect, AspectRatio::Ratio4To3);
            // The escape resolves through the DMT table's own identifier,
            // which must be the row whose blanking the sink means.
            let entry = dmt::by_std_id(u16::from_be_bytes(code)).expect("DMT 640x480@60");
            assert_eq!(entry.code, 0x04);
            assert_eq!(entry.mode.clock_khz, 25_175);
            assert_eq!(entry.mode.htotal, 800);
        }
        other => panic!("expected a named timing, got {other:?}"),
    }
    assert!(matches!(timings[2], StandardTiming::Unused { code } if code == [0x01, 0x01]));
    assert!(matches!(timings[3], StandardTiming::Reserved { code } if code == [0x00, 0x55]));
}

#[test]
fn a_block_without_a_preferred_timing_says_so() {
    // All four descriptors are dummies: the sink advertises nothing.
    let bytes = BaseBlockBuilder::new().manufacturer(b"XXX").build();
    let edid = parse(&bytes);
    assert!(!edid.has_preferred_timing());
    assert_eq!(edid.preferred_timing(), None);
    assert_eq!(edid.detailed_timings().count(), 0);
}

#[test]
fn interlaced_detailed_timings_are_converted_to_frame_lines() {
    let spec = DetailedTimingSpec {
        clock_khz: 74_250,
        hdisplay: 1920,
        hblank: 280,
        hfront: 88,
        hsync: 44,
        vdisplay: 540,
        vblank: 22,
        vfront: 2,
        vsync: 5,
        hsync_positive: true,
        vsync_positive: true,
        interlaced: true,
        width_mm: 0,
        height_mm: 0,
        hborder: 0,
        vborder: 0,
    };
    let bytes = BaseBlockBuilder::new().detailed_timing(0, &spec).build();
    let edid = parse(&bytes);
    let mode = edid.preferred_timing().expect("a preferred timing");
    assert!(mode.is_interlaced());
    assert_eq!(mode.vdisplay, 1080);
    assert_eq!(mode.vtotal, 1125);
    assert_eq!(mode.vsync_start, 1084);
    assert_eq!(mode.vsync_end, 1094);
    assert_eq!(mode.refresh_millihz(), 60_000);
    assert_eq!(edid.detailed_timings().next().unwrap().image_size_mm, None);
}

#[test]
fn checksum_and_header_failures_are_rejected() {
    let good = panel().build();
    assert!(Edid::parse(&good).is_ok());

    let bad_checksum = panel().build_with_bad_checksum();
    let error = Edid::parse(&bad_checksum).expect_err("a wrong checksum must be rejected");
    assert!(
        matches!(error, EdidError::BadBaseChecksum { .. }),
        "{error:?}"
    );
    // The lenient path cannot rescue a base block: without it there is
    // nothing to trust.
    assert!(Edid::parse_lossy(&bad_checksum).is_err());

    let mut bad_header = good;
    bad_header[0] = 0x01;
    bad_header[CHECKSUM_OFFSET] = 0;
    bad_header[CHECKSUM_OFFSET] = checksum_byte(&bad_header);
    assert!(matches!(
        Edid::parse(&bad_header),
        Err(EdidError::BadHeader)
    ));
}

#[test]
fn truncated_buffers_are_rejected_and_missing_extensions_are_typed() {
    let base = panel().extension_count(1).build();
    // One byte short of a whole block.
    let error = Edid::parse(&base[..BLOCK_LEN - 1]).expect_err("a partial block");
    assert!(matches!(error, EdidError::NotBlockAligned { len } if len == 127));
    assert!(matches!(Edid::parse(&[]), Err(EdidError::Empty)));
    assert!(matches!(
        Edid::parse(&base[..64]),
        Err(EdidError::NotBlockAligned { len: 64 })
    ));
    // The base block claims an extension block that is not in the buffer.
    let error = Edid::parse(&base).expect_err("a missing extension block");
    assert!(
        matches!(
            error,
            EdidError::MissingExtensionBlocks {
                declared: 1,
                present: 0
            }
        ),
        "{error:?}"
    );
    // The lenient path accepts it and says what is missing.
    let edid = Edid::parse_lossy(&base).expect("the base block itself is intact");
    assert_eq!(edid.declared_extension_blocks(), 1);
    assert_eq!(edid.present_extension_blocks(), 0);
    assert_eq!(edid.warnings().missing_extension_blocks, 1);
    assert!(!edid.warnings().is_clean());
    assert_eq!(edid.extensions().count(), 0);
    assert_eq!(edid.preferred_timing().map(|m| m.hdisplay), Some(1920));
}

#[test]
fn unsupported_version_is_rejected() {
    let bytes = panel().version(2, 0).build();
    let error = Edid::parse(&bytes).expect_err("version 2 is not EDID");
    assert!(matches!(
        error,
        EdidError::UnsupportedVersion {
            major: 2,
            revision: 0
        }
    ));
    assert!(Edid::parse_lossy(&bytes).is_err());
}

#[test]
fn extension_checksums_are_enforced_in_both_directions() {
    let base = panel().extension_count(1).build();
    let good = CtaBlockBuilder::new(3).vics(&[16, 4]).build();
    let bytes = assemble(base, &[good]);
    let edid = parse(&bytes);
    assert_eq!(edid.present_extension_blocks(), 1);
    assert_eq!(edid.extensions().count(), 1);

    let broken = CtaBlockBuilder::new(3)
        .vics(&[16, 4])
        .build_with_bad_checksum();
    let bytes = assemble(base, &[broken]);
    let error = Edid::parse(&bytes).expect_err("a bad extension checksum must be rejected");
    assert!(
        matches!(error, EdidError::BadExtensionChecksum { block: 1, .. }),
        "{error:?}"
    );
    let edid = Edid::parse_lossy(&bytes).expect("the base block is intact");
    assert_eq!(edid.warnings().bad_extension_checksums, 1);
    assert_eq!(edid.warnings().missing_extension_blocks, 0);
    // The damaged block is skipped rather than half-read.
    assert_eq!(edid.extensions().count(), 0);
    assert_eq!(edid.cta_extensions().count(), 0);
}

#[test]
fn a_cta_extension_is_parsed_into_modes() {
    let base = panel().extension_count(2).build();
    let cta = CtaBlockBuilder::new(3)
        .underscan(true)
        .basic_audio(true)
        .ycbcr444(true)
        .ycbcr422(true)
        .native_dtd_count(1)
        .lpcm_audio()
        .vics(&[16, 4, 97])
        .detailed_timing(&DetailedTimingSpec {
            clock_khz: 241_500,
            hdisplay: 2560,
            hblank: 160,
            hfront: 48,
            hsync: 32,
            vdisplay: 1440,
            vblank: 41,
            vfront: 3,
            vsync: 5,
            hsync_positive: true,
            vsync_positive: false,
            interlaced: false,
            width_mm: 0,
            height_mm: 0,
            hborder: 0,
            vborder: 0,
        })
        .build();
    // An extension this parser does not decode, with a valid checksum.
    let mut displayid = [0u8; BLOCK_LEN];
    displayid[0] = 0x70;
    displayid[1] = 0x13;
    displayid[CHECKSUM_OFFSET] = checksum_byte(&displayid);
    let bytes = assemble(base, &[cta, displayid]);
    let edid = parse(&bytes);

    assert_eq!(edid.warnings(), EdidWarnings::default());
    let cta = edid.first_cta_extension().expect("a CTA-861 extension");
    assert_eq!(cta.revision(), 3);
    assert_eq!(cta.block_index(), 1);
    assert!(cta.underscan() && cta.basic_audio() && cta.ycbcr444() && cta.ycbcr422());
    assert_eq!(cta.native_dtd_count(), 1);

    let blocks: Vec<_> = cta.data_blocks().collect();
    assert_eq!(blocks.len(), 2, "an audio block and a video block");
    assert_eq!(blocks[0].tag, DataBlockTag::Audio);
    assert_eq!(blocks[1].tag, DataBlockTag::Video);

    let svds: Vec<_> = cta.short_video_descriptors().collect();
    assert_eq!(svds.len(), 3);
    assert_eq!(svds[0].vic, 16);
    assert!(!svds[0].native, "the fixture marks no code native");
    assert_eq!(svds[2].vic, 97);

    let timings: Vec<_> = cta.detailed_timings().collect();
    assert_eq!(timings.len(), 1);
    let descriptor = timings[0].1;
    match descriptor {
        Descriptor::DetailedTiming(timing) => {
            assert_eq!(timing.mode.hdisplay, 2560);
            assert_eq!(timing.mode.clock_khz, 241_500);
            assert_eq!(timing.mode.source, TimingSource::CtaDtd { index: 0 });
            assert!(!timing.mode.vsync_positive);
            assert!(timing.mode.is_well_formed());
        }
        other => panic!("expected a detailed timing, got {other:?}"),
    }

    // The second extension is validated and then ignored.
    let extensions: Vec<_> = edid.extensions().collect();
    assert_eq!(extensions.len(), 2);
    assert!(extensions[0].as_cta861().is_some());
    assert!(extensions[1].as_cta861().is_none());
    assert_eq!(extensions[1].tag(), 0x70);
    assert_eq!(
        extensions[1].as_unknown().map(|unknown| unknown.tag()),
        Some(0x70)
    );
}

#[test]
fn native_short_video_descriptors_carry_their_flag() {
    let base = panel().extension_count(1).build();
    let cta = CtaBlockBuilder::new(3)
        .native_vics(&[16])
        .vics(&[4])
        .build();
    let bytes = assemble(base, &[cta]);
    let edid = parse(&bytes);
    let svds: Vec<_> = edid
        .first_cta_extension()
        .expect("CTA")
        .short_video_descriptors()
        .collect();
    assert_eq!(svds.len(), 2);
    assert!(svds[0].native);
    assert_eq!(svds[0].vic, 16);
    assert!(!svds[1].native);
}

#[test]
fn a_malformed_cta_data_block_collection_is_rejected() {
    let base = panel().extension_count(1).build();
    let mut cta = [0u8; BLOCK_LEN];
    cta[0] = EXT_TAG_CTA861;
    cta[1] = 3; // revision
    cta[2] = 6; // detailed timings start at 6
    cta[3] = 0; // flags
    // A video data block claiming 31 bytes of payload in a collection that
    // only has room for one.
    cta[4] = 2 << 5 | 31;
    cta[5] = 16;
    cta[CHECKSUM_OFFSET] = checksum_byte(&cta);
    let bytes = assemble(base, &[cta]);
    let error = Edid::parse(&bytes).expect_err("a truncated data block");
    assert!(
        matches!(error, EdidError::MalformedCtaDataBlocks { block: 1 }),
        "{error:?}"
    );
    let edid = Edid::parse_lossy(&bytes).expect("lenient parse");
    assert_eq!(edid.warnings().malformed_cta_blocks, 1);
    // Enumeration stops at the malformed block instead of running away.
    assert_eq!(
        edid.first_cta_extension()
            .expect("CTA")
            .short_video_descriptors()
            .count(),
        0
    );
}

#[test]
fn a_cta_block_with_no_dtds_reports_no_timings() {
    let base = panel().extension_count(1).build();
    // A CTA block with a data block collection but no detailed timings.
    let cta = CtaBlockBuilder::new(3).vics(&[4]).build();
    let bytes = assemble(base, &[cta]);
    let edid = parse(&bytes);
    let cta = edid.first_cta_extension().expect("CTA");
    // The offset points just past the data block collection (byte 4 plus
    // the one two-byte video data block), which is how a sink with data
    // blocks and no timings writes it.
    assert_eq!(cta.dtd_offset(), 6);
    assert_eq!(cta.detailed_timings().count(), 0);
    assert_eq!(cta.short_video_descriptors().count(), 1);
}

#[test]
fn an_older_cta_revision_is_validated_but_not_decoded() {
    let base = panel().extension_count(1).build();
    let mut old = [0u8; BLOCK_LEN];
    old[0] = EXT_TAG_CTA861;
    old[1] = 1; // revision 1: no data block collection
    old[CHECKSUM_OFFSET] = checksum_byte(&old);
    let bytes = assemble(base, &[old]);
    let edid = parse(&bytes);
    assert_eq!(edid.extensions().count(), 1);
    assert_eq!(edid.cta_extensions().count(), 0);
    let unknown = edid
        .extensions()
        .next()
        .and_then(|extension| extension.as_unknown())
        .expect("an unknown extension");
    assert_eq!(unknown.tag(), EXT_TAG_CTA861);
}

#[test]
fn malformed_detailed_timings_are_rejected_strictly_and_skipped_leniently() {
    // A descriptor whose horizontal total is smaller than its sync end.
    let spec = DetailedTimingSpec {
        clock_khz: 148_500,
        hdisplay: 1920,
        hblank: 10,
        hfront: 88,
        hsync: 44,
        vdisplay: 1080,
        vblank: 45,
        vfront: 4,
        vsync: 5,
        hsync_positive: true,
        vsync_positive: true,
        interlaced: false,
        width_mm: 344,
        height_mm: 194,
        hborder: 0,
        vborder: 0,
    };
    let bytes = panel().detailed_timing(1, &spec).build();
    let error = Edid::parse(&bytes).expect_err("an unprogrammable timing");
    assert!(
        matches!(error, EdidError::InvalidDetailedTiming { index: 1 }),
        "{error:?}"
    );
    let edid = Edid::parse_lossy(&bytes).expect("lenient parse");
    assert_eq!(edid.warnings().invalid_descriptors, 1);
    assert!(matches!(edid.descriptor(1), Some(Descriptor::Invalid)));
    // The sink's preferred timing survives.
    assert_eq!(edid.preferred_timing().map(|mode| mode.htotal), Some(2200));
    assert_eq!(edid.detailed_timings().count(), 1);
}

#[test]
fn inconsistent_range_limits_are_rejected() {
    let spec = RangeLimitsSpec {
        min_vertical_hz: 90,
        max_vertical_hz: 50, // reversed
        ..RangeLimitsSpec::gtf()
    };
    let bytes = panel().range_limits(1, &spec).build();
    let error = Edid::parse(&bytes).expect_err("reversed limits");
    assert!(
        matches!(error, EdidError::InvalidDisplayDescriptor { index: 1 }),
        "{error:?}"
    );
    assert_eq!(
        Edid::parse_lossy(&bytes)
            .expect("lenient parse")
            .warnings()
            .invalid_descriptors,
        1
    );
}

#[test]
fn standard_timing_ids_descriptor_is_exposed() {
    let bytes = panel()
        .standard_timing_ids_descriptor(
            1,
            [
                [0x31, 0x40],
                [0x45, 0x40],
                [0x61, 0x40],
                [0x01, 0x01],
                [0x01, 0x01],
                [0x01, 0x01],
            ],
        )
        .build();
    let edid = parse(&bytes);
    assert_eq!(
        edid.standard_timing_ids(),
        Some([0x31, 0x40, 0x45, 0x40, 0x61, 0x40])
    );
    match edid.descriptor(1) {
        Some(Descriptor::StandardTimingIds(ids)) => assert_eq!(ids[0..2], [0x31, 0x40]),
        other => panic!("expected standard timing IDs, got {other:?}"),
    }
}

#[test]
fn established_timings_iii_and_cvt_codes_are_parsed() {
    let bytes = panel()
        .established_timings_iii(1, [0x01, 0x00, 0x00, 0x00, 0x00, 0x00])
        // 1080 lines per field ((raw + 1) * 2 with raw = 539), 16:9
        // aspect (bits 3..2 = 01), preferred rate 60 Hz (bits 6..5 = 01),
        // 60 Hz standard blanking (bit 3) and 60 Hz reduced (bit 0).
        .cvt_timing_codes(2, 1, [[0x1b, 0x24, 0x29], [0, 0, 0], [0, 0, 0], [0, 0, 0]])
        .build();
    let edid = parse(&bytes);
    let established = edid.established_timings();
    assert_eq!(established.iii, Some([0x01, 0, 0, 0, 0, 0]));
    let codes = edid.cvt_timing_codes().expect("CVT codes");
    assert_eq!(codes.version, 1);
    assert_eq!(codes.len, 1);
    let code = codes.codes()[0];
    assert_eq!(code.lines_per_field, 1080);
    assert_eq!(code.aspect_bits, 1);
    assert!(code.refresh.hz60_standard);
    assert!(code.refresh.hz60_reduced);
    assert_eq!(code.preferred_refresh_millihz(), 60_000);
    assert!(code.supports_preferred_refresh());
    // Standard blanking is supported at the preferred rate, so the sink
    // does not need reduced blanking.
    assert!(!code.prefers_reduced_blanking());
}

#[test]
fn a_display_descriptor_with_an_unknown_sub_tag_is_kept_as_opaque() {
    let mut data = [0u8; 18];
    data[3] = 0xf9; // colour management data: not decoded
    data[5] = 0x11;
    let bytes = panel().descriptor(1, data).build();
    let edid = parse(&bytes);
    assert!(matches!(
        edid.descriptor(1),
        Some(Descriptor::Undecoded { sub_tag: 0xf9 })
    ));
}

#[test]
fn an_analog_sink_is_typed_as_analog() {
    // 0x7f: analog, 1.0/0.4 levels, separate sync, composite sync.
    let bytes = panel().analog_input(0x7f).build();
    let edid = parse(&bytes);
    match edid.video_input() {
        VideoInput::Analog(analog) => {
            assert_eq!(analog.level, 3);
            assert!(analog.separate_sync && analog.composite_sync);
            assert!(analog.sync_on_green && analog.serration);
            assert!(analog.video_setup);
        }
        VideoInput::Digital(_) => panic!("expected an analog input"),
    }
    // Feature bits that only mean something for a digital sink stay clear.
    assert!(!edid.features().ycbcr444 && !edid.features().ycbcr422);
}

/// The property test: many random-but-structured inputs must never panic,
/// and anything that parses must stay well formed.
///
/// The mutations are structured on purpose - byte flips inside real
/// blocks, random extension counts, random truncation - so the inputs
/// reach the parser's branches instead of being uniformly random noise
/// that fails at the header.
#[test]
fn random_but_structured_edids_never_panic_and_stay_well_formed() {
    let mut rng = Rng::new(0x5eed_1234);
    let mut accepted = 0u32;
    let mut rejected = 0u32;
    let mut modes_seen = 0u32;
    let mut vics_seen = 0u32;
    for iteration in 0..4000 {
        let mut bytes = Vec::new();
        let established = [rng.byte(), rng.byte(), rng.byte()];
        let first_code = [rng.byte(), rng.byte()];
        let second_code = [rng.byte(), rng.byte()];
        let declared_by_the_block = rng.below(4) as u8;
        let mut base = panel()
            .established_i_ii(established)
            .standard_timing_code(0, first_code)
            .standard_timing_code(1, second_code)
            .extension_count(declared_by_the_block);
        if rng.below(2) == 0 {
            base = base.range_limits(1, &RangeLimitsSpec::cvt_panel());
        }
        if rng.below(3) == 0 {
            base = base.detailed_timing(
                2,
                &DetailedTimingSpec {
                    clock_khz: (rng.below(600) as u32 + 20) * 1000,
                    hdisplay: (rng.below(4000) + 16) as u16,
                    ..DetailedTimingSpec::new(148_500, 1920, 1080)
                },
            );
        }
        let mut block = base.build();
        // Flip a few bytes anywhere in the base block.
        for _ in 0..rng.below(4) {
            let index = rng.below(CHECKSUM_OFFSET);
            block[index] ^= 1 << rng.below(8);
        }
        if rng.below(2) == 0 {
            // Sometimes keep the checksum valid, sometimes do not.
            block[CHECKSUM_OFFSET] = 0;
            block[CHECKSUM_OFFSET] = checksum_byte(&block);
        }
        bytes.extend_from_slice(&block);

        // Most iterations supply exactly the number of extension blocks
        // the base block declares, so strict parses stay common; a quarter
        // of them disagree on purpose.
        let extensions = if rng.below(4) == 0 {
            rng.below(3)
        } else {
            usize::from(declared_by_the_block)
        };
        for _ in 0..extensions {
            let mut extension = [0u8; BLOCK_LEN];
            for byte in extension.iter_mut() {
                *byte = rng.byte();
            }
            match rng.below(3) {
                0 => {
                    // A well-formed CTA-861 block with random codes, so
                    // the short video descriptor path is really exercised.
                    let count = 1 + rng.below(6);
                    let mut vics = Vec::new();
                    for _ in 0..count {
                        vics.push(rng.byte() & 0x7f);
                    }
                    extension = CtaBlockBuilder::new(3)
                        .native_dtd_count(rng.below(4) as u8)
                        .vics(&vics)
                        .build();
                    if rng.below(2) == 0 {
                        // ... and sometimes with a detailed timing too.
                        extension = CtaBlockBuilder::new(3)
                            .vics(&vics)
                            .detailed_timing(&DetailedTimingSpec::new(
                                (rng.below(400) as u32 + 20) * 1000,
                                (rng.below(2000) + 640) as u16,
                                (rng.below(1200) + 480) as u16,
                            ))
                            .build();
                    }
                }
                1 => {
                    // A CTA-861 header over random collection bytes: often
                    // malformed, which is the point.
                    extension[0] = EXT_TAG_CTA861;
                    extension[1] = 3;
                    extension[2] = (4 + rng.below(40)) as u8;
                    extension[3] = rng.byte() & 0x7f;
                    extension[CHECKSUM_OFFSET] = 0;
                    extension[CHECKSUM_OFFSET] = checksum_byte(&extension);
                }
                _ => {
                    // Random bytes with a valid checksum: usually not a
                    // block this parser decodes at all.
                    extension[CHECKSUM_OFFSET] = 0;
                    extension[CHECKSUM_OFFSET] = checksum_byte(&extension);
                }
            }
            bytes.extend_from_slice(&extension);
        }
        let declared = bytes[0x7e] as usize;
        let present = bytes.len() / BLOCK_LEN - 1;
        if declared > present && rng.below(2) == 0 {
            // Truncate below what the block declares.
            let keep = 1 + rng.below(present + 1);
            bytes.truncate(keep * BLOCK_LEN);
        }
        if rng.below(8) == 0 {
            bytes.truncate(rng.below(bytes.len() + 1));
        }

        let edid = match Edid::parse(&bytes) {
            Ok(edid) => edid,
            Err(error) => {
                rejected += 1;
                // Every error must be printable; a silent failure would be
                // indistinguishable from a rejected EDID in the log.
                let text = format!("{error}");
                assert!(text.len() > 8, "{text}");
                continue;
            }
        };
        accepted += 1;
        // Every fact a driver would read must be safe to read.
        let _ = format!("{edid:?}");
        let _ = edid.vendor();
        let _ = edid.video_input();
        let _ = edid.screen_size();
        let _ = edid.features();
        let _ = edid.chromaticity();
        let _ = edid.established_timings();
        let _ = edid.monitor_name().and_then(EdidText::as_str);
        let _ = edid.range_limits();
        let _ = edid.cvt_timing_codes();
        let _ = edid.standard_timing_ids();
        for timing in edid.standard_timings() {
            if let StandardTiming::Named {
                hdisplay,
                vdisplay,
                refresh_hz,
                ..
            } = timing
            {
                assert!(*hdisplay >= 256, "a named timing has a real width");
                assert!(*vdisplay > 0);
                assert!((60..=123).contains(refresh_hz));
            }
        }
        for timing in edid.detailed_timings() {
            assert!(timing.mode.is_well_formed(), "iteration {iteration}");
            assert!(timing.mode.htotal > 0 && timing.mode.vtotal > 0);
            modes_seen += 1;
        }
        if let Some(mode) = edid.preferred_timing() {
            assert!(mode.is_well_formed());
            modes_seen += 1;
        }
        if let Some(limits) = edid.range_limits() {
            // A range check must never panic on an extreme mode.
            let _ = limits.admits(&Mode {
                clock_khz: u32::MAX,
                ..edid.preferred_timing().unwrap_or(Mode::from_fields(
                    1,
                    1,
                    1,
                    1,
                    1,
                    1,
                    1,
                    1,
                    1,
                    ModeFlags::NONE,
                    TimingSource::Builtin,
                ))
            });
        }
        for extension in edid.extensions() {
            if let Some(cta) = extension.as_cta861() {
                let _ = cta.data_blocks().count();
                for svd in cta.short_video_descriptors() {
                    assert!(svd.vic <= 127);
                    // Every code a short video descriptor can carry and
                    // that is not zero must resolve in the VIC table.
                    if svd.vic != 0 {
                        assert!(vic::by_vic(svd.vic).is_some(), "VIC {}", svd.vic);
                        vics_seen += 1;
                    }
                }
                for (_, descriptor) in cta.detailed_timings() {
                    if let Descriptor::DetailedTiming(timing) = descriptor {
                        assert!(timing.mode.is_well_formed());
                        modes_seen += 1;
                    }
                }
            }
        }
    }
    // The corpus must actually exercise both outcomes, otherwise this test
    // would pass on a parser that rejects or accepts everything.
    assert!(accepted > 200, "only {accepted} inputs parsed");
    assert!(rejected > 200, "only {rejected} inputs were rejected");
    assert!(modes_seen > 200, "only {modes_seen} modes were produced");
    assert!(vics_seen > 5, "only {vics_seen} short video descriptors");
}
