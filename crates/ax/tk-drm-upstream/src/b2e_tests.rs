// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Host unit tests for batch B2e (HDMI helpers: hdmi, drm_scdc_helper,
//! drm_hdmi_helper, drm_hdmi_state_helper).

use alloc::boxed::Box;
use std::sync::Mutex;

use tk_linux_kapi::config::EINVAL;
use tk_linux_kapi::i2c::{I2cAdapter, I2cAlgorithm, I2cMsg, I2C_FUNC_I2C, I2C_M_RD};

use crate::drm_connector_types::*;
use crate::drm_edid_upstream::*;
use crate::drm_hdmi_helper_upstream::*;
use crate::drm_modes_types::*;
use crate::drm_scdc_helper_upstream::*;
use crate::hdmi_types::*;
use crate::hdmi_upstream::{self, *};
use crate::test_support::TestDrm;
use crate::uapi_drm_consts::*;

// ============================================================================
// 1. Infoframe pack/unpack round-trips for every infoframe type
// ============================================================================

#[test]
fn test_hdmi_avi_infoframe_pack_unpack_roundtrip() {
    let mut avi: HdmiAviInfoframe = unsafe { core::mem::zeroed() };
    unsafe { hdmi_avi_infoframe_init(&mut avi) };
    avi.colorspace = HDMI_COLORSPACE_RGB;
    avi.scan_mode = HDMI_SCAN_MODE_UNDERSCAN;
    avi.colorimetry = HDMI_COLORIMETRY_ITU_709;
    avi.picture_aspect = HDMI_PICTURE_ASPECT_16_9;
    avi.active_aspect = HDMI_ACTIVE_ASPECT_16_9;
    avi.itc = true;
    avi.extended_colorimetry = HDMI_EXTENDED_COLORIMETRY_XV_YCC_709;
    avi.quantization_range = HDMI_QUANTIZATION_RANGE_FULL;
    avi.video_code = 16; // 1080p60
    avi.top_bar = 0;
    avi.bottom_bar = 1080;
    avi.left_bar = 0;
    avi.right_bar = 1920;

    let mut buf = [0u8; HDMI_INFOFRAME_HEADER_SIZE + HDMI_AVI_INFOFRAME_SIZE];
    let pack_len = unsafe { hdmi_avi_infoframe_pack(&mut avi, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(pack_len, (HDMI_INFOFRAME_HEADER_SIZE + HDMI_AVI_INFOFRAME_SIZE) as isize);

    // Verify header byte layout
    assert_eq!(buf[0], HDMI_INFOFRAME_TYPE_AVI as u8);
    assert_eq!(buf[1], 2); // version
    assert_eq!(buf[2], HDMI_AVI_INFOFRAME_SIZE as u8); // length 13

    // Verify checksum: sum of all bytes in buffer modulo 256 should be 0
    let csum: u8 = buf.iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
    assert_eq!(csum, 0, "AVI infoframe checksum byte failure");

    // Unpack round trip using generic hdmi_infoframe_unpack
    let mut frame: HdmiInfoframe = unsafe { core::mem::zeroed() };
    let unpack_ret = unsafe {
        hdmi_infoframe_unpack(&mut frame, buf.as_ptr().cast(), buf.len())
    };
    assert_eq!(unpack_ret, 0);

    let unpacked = unsafe { &frame.avi };
    assert_eq!(unpacked.r#type, HDMI_INFOFRAME_TYPE_AVI);
    assert_eq!(unpacked.version, 2);
    assert_eq!(unpacked.length, HDMI_AVI_INFOFRAME_SIZE as u8);
    assert_eq!(unpacked.colorspace, HDMI_COLORSPACE_RGB);
    assert_eq!(unpacked.scan_mode, HDMI_SCAN_MODE_UNDERSCAN);
    assert_eq!(unpacked.colorimetry, HDMI_COLORIMETRY_ITU_709);
    assert_eq!(unpacked.picture_aspect, HDMI_PICTURE_ASPECT_16_9);
    assert_eq!(unpacked.active_aspect, HDMI_ACTIVE_ASPECT_16_9);
    assert_eq!(unpacked.itc, true);
    assert_eq!(unpacked.extended_colorimetry, HDMI_EXTENDED_COLORIMETRY_XV_YCC_709);
    assert_eq!(unpacked.quantization_range, HDMI_QUANTIZATION_RANGE_FULL);
    assert_eq!(unpacked.video_code, 16);
    assert_eq!(unpacked.top_bar, 0);
    assert_eq!(unpacked.bottom_bar, 1080);
    assert_eq!(unpacked.left_bar, 0);
    assert_eq!(unpacked.right_bar, 1920);
}

#[test]
fn test_hdmi_spd_infoframe_pack_unpack_roundtrip() {
    let mut spd: HdmiSpdInfoframe = unsafe { core::mem::zeroed() };
    let vendor = b"TheKerne\0";
    let product = b"TestMonitor1234\0";
    assert_eq!(
        unsafe {
            hdmi_spd_infoframe_init(
                &mut spd,
                vendor.as_ptr().cast(),
                product.as_ptr().cast(),
            )
        },
        0
    );
    spd.sdi = HDMI_SPD_SDI_PC;

    let mut buf = [0u8; HDMI_INFOFRAME_HEADER_SIZE + HDMI_SPD_INFOFRAME_SIZE];
    let pack_len = unsafe { hdmi_spd_infoframe_pack(&mut spd, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(pack_len, (HDMI_INFOFRAME_HEADER_SIZE + HDMI_SPD_INFOFRAME_SIZE) as isize);

    assert_eq!(buf[0], HDMI_INFOFRAME_TYPE_SPD as u8);
    assert_eq!(buf[1], 1); // version
    assert_eq!(buf[2], HDMI_SPD_INFOFRAME_SIZE as u8); // length 25

    let csum: u8 = buf.iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
    assert_eq!(csum, 0, "SPD infoframe checksum byte failure");

    let mut frame: HdmiInfoframe = unsafe { core::mem::zeroed() };
    let unpack_ret = unsafe {
        hdmi_infoframe_unpack(&mut frame, buf.as_ptr().cast(), buf.len())
    };
    assert_eq!(unpack_ret, 0);

    let unpacked = unsafe { &frame.spd };
    assert_eq!(unpacked.r#type, HDMI_INFOFRAME_TYPE_SPD);
    assert_eq!(unpacked.version, 1);
    assert_eq!(unpacked.length, HDMI_SPD_INFOFRAME_SIZE as u8);
    assert_eq!(unpacked.sdi, HDMI_SPD_SDI_PC);
    assert_eq!(&unpacked.vendor[..8], b"TheKerne".map(|c| c as core::ffi::c_char));
    assert_eq!(&unpacked.product[..15], b"TestMonitor1234".map(|c| c as core::ffi::c_char));
}

#[test]
fn test_hdmi_audio_infoframe_pack_unpack_roundtrip() {
    let mut audio: HdmiAudioInfoframe = unsafe { core::mem::zeroed() };
    assert_eq!(unsafe { hdmi_audio_infoframe_init(&mut audio) }, 0);
    audio.channels = 6;
    audio.coding_type = HDMI_AUDIO_CODING_TYPE_PCM;
    audio.sample_size = HDMI_AUDIO_SAMPLE_SIZE_24;
    audio.sample_frequency = HDMI_AUDIO_SAMPLE_FREQUENCY_48000;
    audio.channel_allocation = 0x0b;
    audio.downmix_inhibit = true;
    audio.level_shift_value = 4;

    let mut buf = [0u8; HDMI_INFOFRAME_HEADER_SIZE + HDMI_AUDIO_INFOFRAME_SIZE];
    let pack_len = unsafe { hdmi_audio_infoframe_pack(&mut audio, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(pack_len, (HDMI_INFOFRAME_HEADER_SIZE + HDMI_AUDIO_INFOFRAME_SIZE) as isize);

    assert_eq!(buf[0], HDMI_INFOFRAME_TYPE_AUDIO as u8);
    assert_eq!(buf[1], 1);
    assert_eq!(buf[2], HDMI_AUDIO_INFOFRAME_SIZE as u8); // 10

    let csum: u8 = buf.iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
    assert_eq!(csum, 0, "Audio infoframe checksum byte failure");

    let mut frame: HdmiInfoframe = unsafe { core::mem::zeroed() };
    let unpack_ret = unsafe {
        hdmi_infoframe_unpack(&mut frame, buf.as_ptr().cast(), buf.len())
    };
    assert_eq!(unpack_ret, 0);

    let unpacked = unsafe { &frame.audio };
    assert_eq!(unpacked.channels, 5); // Linux hdmi_audio_infoframe_unpack unpacks ptr[0] & 7 (which is 6 - 1 = 5)
    assert_eq!(unpacked.coding_type, HDMI_AUDIO_CODING_TYPE_PCM);
    assert_eq!(unpacked.sample_size, HDMI_AUDIO_SAMPLE_SIZE_24);
    assert_eq!(unpacked.sample_frequency, HDMI_AUDIO_SAMPLE_FREQUENCY_48000);
    assert_eq!(unpacked.channel_allocation, 0x0b);
    assert_eq!(unpacked.downmix_inhibit, true);
    assert_eq!(unpacked.level_shift_value, 4);
}

#[test]
fn test_hdmi_vendor_infoframe_pack_unpack_roundtrip() {
    let mut vendor: HdmiVendorInfoframe = unsafe { core::mem::zeroed() };
    assert_eq!(unsafe { hdmi_vendor_infoframe_init(&mut vendor) }, 0);
    vendor.s3d_struct = HDMI_3D_STRUCTURE_SIDE_BY_SIDE_HALF;
    vendor.s3d_ext_data = 1;

    let mut buf = [0u8; HDMI_INFOFRAME_HEADER_SIZE + HDMI_MAX_INFOFRAME_SIZE];
    let pack_len = unsafe { hdmi_vendor_infoframe_pack(&mut vendor, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(pack_len, 10); // 4 header + 6 payload

    assert_eq!(buf[0], HDMI_INFOFRAME_TYPE_VENDOR as u8);
    assert_eq!(buf[1], 1);
    let csum: u8 = buf[..pack_len as usize].iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
    assert_eq!(csum, 0, "Vendor infoframe checksum failure");

    let mut frame: HdmiInfoframe = unsafe { core::mem::zeroed() };
    let unpack_ret = unsafe {
        hdmi_infoframe_unpack(&mut frame, buf.as_ptr().cast(), pack_len as usize)
    };
    assert_eq!(unpack_ret, 0);

    let unpacked = unsafe { &frame.vendor.hdmi };
    assert_eq!(unpacked.oui, hdmi_upstream::HDMI_IEEE_OUI);
    assert_eq!(unpacked.s3d_struct, HDMI_3D_STRUCTURE_SIDE_BY_SIDE_HALF);
    assert_eq!(unpacked.s3d_ext_data, 1);
}

#[test]
fn test_hdmi_drm_infoframe_pack_unpack_roundtrip() {
    let mut drm_frame: HdmiDrmInfoframe = unsafe { core::mem::zeroed() };
    assert_eq!(unsafe { hdmi_drm_infoframe_init(&mut drm_frame) }, 0);
    drm_frame.eotf = HDMI_EOTF_SMPTE_ST2084;
    drm_frame.metadata_type = HDMI_STATIC_METADATA_TYPE1;
    drm_frame.max_cll = 1000;
    drm_frame.max_fall = 400;
    drm_frame.max_display_mastering_luminance = 1000;
    drm_frame.min_display_mastering_luminance = 5;

    let mut buf = [0u8; HDMI_INFOFRAME_HEADER_SIZE + HDMI_DRM_INFOFRAME_SIZE];
    let pack_len = unsafe { hdmi_drm_infoframe_pack(&mut drm_frame, buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(pack_len, (HDMI_INFOFRAME_HEADER_SIZE + HDMI_DRM_INFOFRAME_SIZE) as isize);

    assert_eq!(buf[0], HDMI_INFOFRAME_TYPE_DRM as u8);
    assert_eq!(buf[1], 1);
    assert_eq!(buf[2], HDMI_DRM_INFOFRAME_SIZE as u8); // 26
    let csum: u8 = buf.iter().fold(0u8, |acc, &x| acc.wrapping_add(x));
    assert_eq!(csum, 0, "DRM infoframe checksum failure");

    let mut frame: HdmiInfoframe = unsafe { core::mem::zeroed() };
    let unpack_ret = unsafe {
        hdmi_infoframe_unpack(&mut frame, buf.as_ptr().cast(), buf.len())
    };
    assert_eq!(unpack_ret, 0);

    let unpacked = unsafe { &frame.drm };
    assert_eq!(unpacked.eotf, HDMI_EOTF_SMPTE_ST2084);
    assert_eq!(unpacked.metadata_type, HDMI_STATIC_METADATA_TYPE1);
    assert_eq!(unpacked.max_cll, 1000);
    assert_eq!(unpacked.max_fall, 400);
    assert_eq!(unpacked.max_display_mastering_luminance, 1000);
    assert_eq!(unpacked.min_display_mastering_luminance, 5);
}

// ============================================================================
// 2. AVI infoframe from 1080p60 and 4K60 YCbCr420 modes via drm_edid
// ============================================================================

#[test]
fn test_hdmi_avi_infoframe_from_1080p60_and_4k60_y420() {
    let mut test_drm = TestDrm::new(1 << 1); // DRIVER_MODESET
    test_drm.init_mode_config();
    let dev = test_drm.ptr();

    let mut conn: DrmConnector = unsafe { core::mem::zeroed() };
    // Set up display info for HDMI
    conn.dev = dev;
    conn.display_info.is_hdmi = true;
    conn.display_info.color_formats = (1 << DRM_OUTPUT_COLOR_FORMAT_RGB444)
        | (1 << DRM_OUTPUT_COLOR_FORMAT_YCBCR420);

    // Mode 1: 1080p60 (VIC 16)
    let mut mode_1080p: DrmDisplayMode = unsafe { core::mem::zeroed() };
    mode_1080p.hdisplay = 1920;
    mode_1080p.hsync_start = 2008;
    mode_1080p.hsync_end = 2052;
    mode_1080p.htotal = 2200;
    mode_1080p.vdisplay = 1080;
    mode_1080p.vsync_start = 1084;
    mode_1080p.vsync_end = 1089;
    mode_1080p.vtotal = 1125;
    mode_1080p.clock = 148500;
    mode_1080p.flags = DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC;
    mode_1080p.picture_aspect_ratio = HDMI_PICTURE_ASPECT_16_9;

    let mut avi: HdmiAviInfoframe = unsafe { core::mem::zeroed() };
    let ret = unsafe {
        drm_hdmi_avi_infoframe_from_display_mode(
            &mut avi,
            &mut conn,
            &mode_1080p,
        )
    };
    assert_eq!(ret, 0);
    assert_eq!(avi.video_code, 16, "1080p60 should match VIC 16");
    assert_eq!(avi.picture_aspect, HDMI_PICTURE_ASPECT_16_9);

    // Mode 2: 4K60 (VIC 97: 3840x2160 @ 60Hz)
    let mut mode_4k60: DrmDisplayMode = unsafe { core::mem::zeroed() };
    mode_4k60.hdisplay = 3840;
    mode_4k60.hsync_start = 4016;
    mode_4k60.hsync_end = 4104;
    mode_4k60.htotal = 4400;
    mode_4k60.vdisplay = 2160;
    mode_4k60.vsync_start = 2168;
    mode_4k60.vsync_end = 2178;
    mode_4k60.vtotal = 2250;
    mode_4k60.clock = 594000;
    mode_4k60.flags = DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC;
    mode_4k60.picture_aspect_ratio = HDMI_PICTURE_ASPECT_16_9;

    let mut avi_4k: HdmiAviInfoframe = unsafe { core::mem::zeroed() };
    let ret2 = unsafe {
        drm_hdmi_avi_infoframe_from_display_mode(
            &mut avi_4k,
            &mut conn,
            &mode_4k60,
        )
    };
    assert_eq!(ret2, 0);
    assert_eq!(avi_4k.video_code, 97, "4K60 should match VIC 97");

    test_drm.cleanup_mode_config();
}

// ============================================================================
// 3. Scrambling enable/disable and TMDS ratio writes over mock I2C adapter
// ============================================================================

struct MockScdcAlgo {
    registers: Mutex<[u8; 256]>,
}

impl MockScdcAlgo {
    const fn new() -> Self {
        Self {
            registers: Mutex::new([0u8; 256]),
        }
    }
}

impl I2cAlgorithm for MockScdcAlgo {
    fn master_xfer(&self, _adap: &I2cAdapter<'_>, msgs: &mut [I2cMsg<'_>]) -> i32 {
        if msgs.is_empty() {
            return -EINVAL;
        }
        let mut regs = self.registers.lock().unwrap();

        let mut i = 0;
        while i < msgs.len() {
            let msg = &msgs[i];
            if (msg.flags & I2C_M_RD) == 0 {
                // Write message
                if msg.len < 1 {
                    return -EINVAL;
                }
                let offset = msg.buf[0] as usize;
                if i + 1 < msgs.len() && (msgs[i + 1].flags & I2C_M_RD) != 0 {
                    // Combined write offset + read data
                    let read_msg = &mut msgs[i + 1];
                    for (j, dst) in read_msg.buf[..read_msg.len].iter_mut().enumerate() {
                        *dst = regs[(offset + j) & 0xff];
                    }
                    i += 2;
                } else {
                    for (j, &val) in msg.buf[1..msg.len].iter().enumerate() {
                        regs[(offset + j) & 0xff] = val;
                    }
                    i += 1;
                }
            } else {
                return -EINVAL;
            }
        }

        msgs.len() as i32
    }

    fn functionality(&self) -> u32 {
        I2C_FUNC_I2C
    }
}

static MOCK_ALGO: MockScdcAlgo = MockScdcAlgo::new();

#[test]
fn test_scdc_scrambling_and_tmds_clock_ratio() {
    let adap = I2cAdapter::new("mock_scdc", Some(&MOCK_ALGO));
    let adap_box = Box::new(adap);
    let adap_ptr = Box::into_raw(adap_box);

    let mut conn: DrmConnector = unsafe { core::mem::zeroed() };
    conn.ddc = adap_ptr.cast();

    {
        let mut regs = MOCK_ALGO.registers.lock().unwrap();
        regs.fill(0);
    }

    // 1. Enable scrambling: register 0x20 bit 0 (SCDC_SCRAMBLING_ENABLE)
    let ok = unsafe { drm_scdc_set_scrambling(&mut conn, true) };
    assert!(ok, "drm_scdc_set_scrambling should succeed");
    {
        let regs = MOCK_ALGO.registers.lock().unwrap();
        assert_eq!(regs[0x20] & 0x01, 0x01, "Scrambling bit should be set in reg 0x20");
    }

    // 2. Set high TMDS clock ratio (1/40): register 0x20 bit 1 (SCDC_TMDS_BIT_CLOCK_RATIO_BY_40)
    let ok_clk = unsafe { drm_scdc_set_high_tmds_clock_ratio(&mut conn, true) };
    assert!(ok_clk, "drm_scdc_set_high_tmds_clock_ratio should succeed");
    {
        let regs = MOCK_ALGO.registers.lock().unwrap();
        assert_eq!(
            regs[0x20] & 0x03,
            0x03,
            "Both scrambling and high TMDS clock ratio should be set"
        );
    }

    // 3. Mock status read (reg 0x21 bit 0 = scrambling status)
    {
        let mut regs = MOCK_ALGO.registers.lock().unwrap();
        regs[0x21] = 0x01; // SCDC_SCRAMBLING_STATUS
    }
    let status = unsafe { drm_scdc_get_scrambling_status(&mut conn) };
    assert!(status, "Scrambling status should report true");

    // 4. Disable scrambling
    let ok_off = unsafe { drm_scdc_set_scrambling(&mut conn, false) };
    assert!(ok_off, "Disabling scrambling should succeed");
    {
        let regs = MOCK_ALGO.registers.lock().unwrap();
        assert_eq!(regs[0x20] & 0x01, 0x00, "Scrambling bit should be cleared");
        assert_eq!(
            regs[0x20] & 0x02,
            0x02,
            "TMDS clock ratio bit should remain intact"
        );
    }

    unsafe { drop(Box::from_raw(adap_ptr)) };
}

// ============================================================================
// 4. Mode clock / TMDS character rate computation for 8/10/12 bpc and YCbCr420
// ============================================================================

#[test]
fn test_hdmi_compute_mode_clock_and_tmds_char_rates() {
    let mut mode_1080p: DrmDisplayMode = unsafe { core::mem::zeroed() };
    mode_1080p.clock = 148500; // 148.5 MHz
    let mode_1080p_ptr = &mode_1080p as *const DrmDisplayMode;

    // RGB / YUV444 at 8 bpc -> factor 1.0 -> 148,500,000
    let rate_8bpc = unsafe {
        drm_hdmi_compute_mode_clock(
            mode_1080p_ptr,
            8,
            DRM_OUTPUT_COLOR_FORMAT_RGB444,
        )
    };
    assert_eq!(rate_8bpc, 148500 * 1000);

    // RGB at 10 bpc -> factor 1.25 (10/8) -> 148,500,000 * 5 / 4 = 185,625,000
    let rate_10bpc = unsafe {
        drm_hdmi_compute_mode_clock(
            mode_1080p_ptr,
            10,
            DRM_OUTPUT_COLOR_FORMAT_RGB444,
        )
    };
    assert_eq!(rate_10bpc, (148500u64 * 1000 * 5) / 4);

    // RGB at 12 bpc -> factor 1.50 (12/8) -> 148,500,000 * 3 / 2 = 222,750,000
    let rate_12bpc = unsafe {
        drm_hdmi_compute_mode_clock(
            mode_1080p_ptr,
            12,
            DRM_OUTPUT_COLOR_FORMAT_RGB444,
        )
    };
    assert_eq!(rate_12bpc, (148500u64 * 1000 * 3) / 2);

    // YUV422 at any bpc is always 8bpc TMDS clock rate in HDMI spec -> 148,500,000
    let rate_422_12bpc = unsafe {
        drm_hdmi_compute_mode_clock(
            mode_1080p_ptr,
            12,
            DRM_OUTPUT_COLOR_FORMAT_YCBCR422,
        )
    };
    assert_eq!(rate_422_12bpc, 148500 * 1000);

    // 4K60 mode clock: 594 MHz
    let mut mode_4k60: DrmDisplayMode = unsafe { core::mem::zeroed() };
    mode_4k60.clock = 594000;
    let mode_4k60_ptr = &mode_4k60 as *const DrmDisplayMode;

    // YUV420 at 8 bpc -> clock / 2 -> 297 MHz
    let rate_420_8bpc = unsafe {
        drm_hdmi_compute_mode_clock(
            mode_4k60_ptr,
            8,
            DRM_OUTPUT_COLOR_FORMAT_YCBCR420,
        )
    };
    assert_eq!(rate_420_8bpc, (594000u64 * 1000) / 2);

    // YUV420 at 10 bpc -> (clock / 2) * 5 / 4 = 371.25 MHz
    let rate_420_10bpc = unsafe {
        drm_hdmi_compute_mode_clock(
            mode_4k60_ptr,
            10,
            DRM_OUTPUT_COLOR_FORMAT_YCBCR420,
        )
    };
    assert_eq!(rate_420_10bpc, ((594000u64 * 1000) / 2 * 5) / 4);

    // YUV420 at 12 bpc -> (clock / 2) * 3 / 2 = 445.5 MHz
    let rate_420_12bpc = unsafe {
        drm_hdmi_compute_mode_clock(
            mode_4k60_ptr,
            12,
            DRM_OUTPUT_COLOR_FORMAT_YCBCR420,
        )
    };
    assert_eq!(rate_420_12bpc, ((594000u64 * 1000) / 2 * 3) / 2);
}
