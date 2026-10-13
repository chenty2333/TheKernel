// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Spot-checks of generated i915 display register definitions. Expected values
//! are the C macro expansions (kernel-flag `gcc -E`) compiled and run on the host.

#[test]
fn display_register_macros_match_c_expansion() {
    assert_eq!((tk_intel_gt::bxt_dpio_phy_regs_upstream::BXT_PHY_BASE(1)) as u64, 1449984u64, "BXT_PHY_BASE");
    assert_eq!((tk_intel_gt::icl_dsi_regs_upstream::FREE_HEADER_CREDIT_SHIFT) as u64, 8u64, "FREE_HEADER_CREDIT_SHIFT");
    assert_eq!((tk_intel_gt::icl_dsi_regs_upstream::RECEIVED_CRC_WAS_LOST) as u64, 1024u64, "RECEIVED_CRC_WAS_LOST");
    assert_eq!((tk_intel_gt::intel_audio_regs_upstream::AUDIO_CP_READY(1)) as u64, 32u64, "AUDIO_CP_READY");
    assert_eq!((tk_intel_gt::intel_audio_regs_upstream::CPT_AUD_CNTRL_ST2.reg) as u64, 938176u64, "CPT_AUD_CNTRL_ST2");
    assert_eq!((tk_intel_gt::intel_audio_regs_upstream::VLV_AMP_MUTE) as u64, 2u64, "VLV_AMP_MUTE");
    assert_eq!((tk_intel_gt::intel_backlight_regs_upstream::BACKLIGHT_DUTY_CYCLE_MASK) as u64, 65535u64, "BACKLIGHT_DUTY_CYCLE_MASK");
    assert_eq!((tk_intel_gt::intel_backlight_regs_upstream::BLM_PHASE_IN_TIME_BASE_MASK) as u64, 16711680u64, "BLM_PHASE_IN_TIME_BASE_MASK");
    assert_eq!((tk_intel_gt::intel_backlight_regs_upstream::UTIL_PIN_MODE_DATA) as u64, 0u64, "UTIL_PIN_MODE_DATA");
    assert_eq!((tk_intel_gt::intel_casf_regs_upstream::SHRPLUT_INDEX(1).reg) as u64, 428724u64, "SHRPLUT_INDEX");
    assert_eq!((tk_intel_gt::intel_cmtg_regs_upstream::CMTG_CLK_SEL.reg) as u64, 287072u64, "CMTG_CLK_SEL");
    assert_eq!((tk_intel_gt::intel_color_regs_upstream::CGM_PIPE_CSC_COEFF01(1).reg) as u64, 2005248u64, "CGM_PIPE_CSC_COEFF01");
    assert_eq!((tk_intel_gt::intel_color_regs_upstream::PIPE_CSC_OUTPUT_COEFF_BY(1).reg) as u64, 299348u64, "PIPE_CSC_OUTPUT_COEFF_BY");
    assert_eq!((tk_intel_gt::intel_color_regs_upstream::_PAL_PREC_EXT_GC_MAX_C) as u64, 308256u64, "_PAL_PREC_EXT_GC_MAX_C");
    assert_eq!((tk_intel_gt::intel_display_reg_defs_upstream::_MMIO_PLL(1, 2, 0).reg) as u64, 0u64, "_MMIO_PLL");
    assert_eq!((tk_intel_gt::intel_display_reg_defs_upstream::_PIPE(1, 2, 0)) as u64, 0u64, "_PIPE");
    assert_eq!((tk_intel_gt::intel_display_regs_upstream::SPLL_REF_PCH_SSC_BDW) as u64, 536870912u64, "SPLL_REF_PCH_SSC_BDW");
    assert_eq!((tk_intel_gt::intel_dkl_phy_regs_upstream::DKL_CMN_UC_DW27_UC_HEALTH) as u64, 32768u64, "DKL_CMN_UC_DW27_UC_HEALTH");
    assert_eq!((tk_intel_gt::intel_dkl_phy_regs_upstream::DKL_PLL_TDC_FEED_FWD_GAIN(1)) as u64, 1u64, "DKL_PLL_TDC_FEED_FWD_GAIN");
    assert_eq!((tk_intel_gt::intel_dkl_phy_regs_upstream::_DKL_PHY3_BASE) as u64, 1482752u64, "_DKL_PHY3_BASE");
    assert_eq!((tk_intel_gt::intel_dmc_regs_upstream::ADLP_PIPE_MMIO_END) as u64, 393215u64, "ADLP_PIPE_MMIO_END");
    assert_eq!((tk_intel_gt::intel_dp_aux_regs_upstream::DP_AUX_CH_CTL(1).reg) as u64, 409872u64, "DP_AUX_CH_CTL");
    assert_eq!((tk_intel_gt::intel_dsb_regs_upstream::DSBSL_INSTANCE(1, 2)) as u64, 466176u64, "DSBSL_INSTANCE");
    assert_eq!((tk_intel_gt::intel_fdi_regs_upstream::DISPLAY_PORT_PLL_BIOS_0.reg) as u64, 286732u64, "DISPLAY_PORT_PLL_BIOS_0");
    assert_eq!((tk_intel_gt::intel_fdi_regs_upstream::FDI_LINK_TRAIN_PATTERN_IDLE_CPT) as u64, 512u64, "FDI_LINK_TRAIN_PATTERN_IDLE_CPT");
    assert_eq!((tk_intel_gt::intel_fdi_regs_upstream::FDI_RX_PHASE_SYNC_POINTER_EN) as u64, 1u64, "FDI_RX_PHASE_SYNC_POINTER_EN");
    assert_eq!((tk_intel_gt::intel_gmbus_regs_upstream::GEN9_GMBUS_BYTE_COUNT_MAX) as u64, 511u64, "GEN9_GMBUS_BYTE_COUNT_MAX");
    assert_eq!((tk_intel_gt::intel_gmbus_regs_upstream::GMBUS_HW_WAIT_PHASE) as u64, 16384u64, "GMBUS_HW_WAIT_PHASE");
    // C `(1 << 31)` is an int; the register value is its 32-bit pattern, so the
    // expectation is the unsigned register value, not the sign-extended int.
    assert_eq!((tk_intel_gt::intel_gmbus_regs_upstream::GMBUS_SW_CLR_INT) as u64, 2147483648u64, "GMBUS_SW_CLR_INT");
    assert_eq!((tk_intel_gt::intel_hdcp_regs_upstream::TRANS_HDCP_BKSVLO(1).reg) as u64, 419088u64, "TRANS_HDCP_BKSVLO");
    assert_eq!((tk_intel_gt::intel_mg_phy_regs_upstream::CFG_AMI_CK_DIV_OVERRIDE_EN) as u64, 16777216u64, "CFG_AMI_CK_DIV_OVERRIDE_EN");
    assert_eq!((tk_intel_gt::intel_mg_phy_regs_upstream::MG_PLL_BIAS_INIT_DCOAMP_MASK) as u64, 1056964608u64, "MG_PLL_BIAS_INIT_DCOAMP_MASK");
    assert_eq!((tk_intel_gt::intel_mg_phy_regs_upstream::MG_TX_DCC_TX1LN1_PORT2) as u64, 1479952u64, "MG_TX_DCC_TX1LN1_PORT2");
    assert_eq!((tk_intel_gt::intel_pipe_crc_regs_upstream::_PIPE_CRC_EXP_3_B_IVB) as u64, 397400u64, "_PIPE_CRC_EXP_3_B_IVB");
    assert_eq!((tk_intel_gt::intel_sprite_regs_upstream::DVSCNTR(1).reg) as u64, 471424u64, "DVSCNTR");
}

#[test]
fn test_adlp_indexed_register_macros() {
    use tk_intel_gt::intel_display_core_types_upstream::IntelDisplay;
    use tk_intel_gt::intel_display_device_types_upstream::IntelDisplayDeviceInfo;
    use tk_intel_gt::intel_display_limits_types_upstream::{
        PIPE_A, PIPE_B, PIPE_C, PIPE_D, _PIPE_EDP,
        TRANSCODER_A, TRANSCODER_B, TRANSCODER_C, TRANSCODER_D, TRANSCODER_EDP,
    };
    use tk_intel_gt::intel_display_regs_upstream::{
        TRANSCONF, TRANS_DDI_FUNC_CTL, PIPESTAT, PIPE_DATA_M1, TRANS_HTOTAL,
    };
    use tk_intel_gt::intel_cursor_regs_upstream::CURCNTR;
    use tk_intel_gt::i9xx_plane_regs_upstream::DSPCNTR;
    use tk_intel_gt::intel_psr_regs_upstream::TRANS_PSR_IMR;
    use tk_intel_gt::intel_pipe_crc_regs_upstream::PIPE_CRC_CTL;
    use tk_intel_gt::intel_pps_regs_upstream::PP_STATUS;

    let mut device_info: IntelDisplayDeviceInfo = unsafe { core::mem::zeroed() };
    device_info.mmio_offset = 0;
    device_info.pipe_offsets[PIPE_A as usize] = 0x70000;
    device_info.pipe_offsets[PIPE_B as usize] = 0x71000;
    device_info.pipe_offsets[PIPE_C as usize] = 0x72000;
    device_info.pipe_offsets[PIPE_D as usize] = 0x73000;
    device_info.pipe_offsets[_PIPE_EDP as usize] = 0x7f000;

    device_info.trans_offsets[TRANSCODER_A as usize] = 0x60000;
    device_info.trans_offsets[TRANSCODER_B as usize] = 0x61000;
    device_info.trans_offsets[TRANSCODER_C as usize] = 0x62000;
    device_info.trans_offsets[TRANSCODER_D as usize] = 0x63000;
    device_info.trans_offsets[TRANSCODER_EDP as usize] = 0x6f000;

    device_info.cursor_offsets[PIPE_A as usize] = 0x70080;
    device_info.cursor_offsets[PIPE_B as usize] = 0x71080;
    device_info.cursor_offsets[PIPE_C as usize] = 0x72080;
    device_info.cursor_offsets[PIPE_D as usize] = 0x73080;

    let mut display_box = std::boxed::Box::new(unsafe { core::mem::zeroed::<IntelDisplay>() });
    display_box.info.__device_info = &mut device_info;
    display_box.info.__runtime_info.ip.ver = 13;
    display_box.pps.mmio_base = 0x61200;
    let display = &*display_box as *const IntelDisplay;

    unsafe {
        // TRANSCONF (pipe offsets relative to PIPE_A + 0x70008)
        assert_eq!(TRANSCONF(display, TRANSCODER_A as u32).reg, 0x70008);
        assert_eq!(TRANSCONF(display, TRANSCODER_B as u32).reg, 0x71008);
        assert_eq!(TRANSCONF(display, TRANSCODER_C as u32).reg, 0x72008);
        assert_eq!(TRANSCONF(display, TRANSCODER_D as u32).reg, 0x73008);
        assert_eq!(TRANSCONF(display, TRANSCODER_EDP as u32).reg, 0x7f008);

        // TRANS_DDI_FUNC_CTL (trans offsets relative to TRANSCODER_A + 0x60400)
        assert_eq!(TRANS_DDI_FUNC_CTL(display, TRANSCODER_A as u32).reg, 0x60400);
        assert_eq!(TRANS_DDI_FUNC_CTL(display, TRANSCODER_B as u32).reg, 0x61400);
        assert_eq!(TRANS_DDI_FUNC_CTL(display, TRANSCODER_C as u32).reg, 0x62400);
        assert_eq!(TRANS_DDI_FUNC_CTL(display, TRANSCODER_D as u32).reg, 0x63400);
        assert_eq!(TRANS_DDI_FUNC_CTL(display, TRANSCODER_EDP as u32).reg, 0x6f400);

        // PIPESTAT (pipe offsets relative to PIPE_A + 0x70024)
        assert_eq!(PIPESTAT(display, PIPE_A as u32).reg, 0x70024);
        assert_eq!(PIPESTAT(display, PIPE_B as u32).reg, 0x71024);
        assert_eq!(PIPESTAT(display, PIPE_C as u32).reg, 0x72024);
        assert_eq!(PIPESTAT(display, PIPE_D as u32).reg, 0x73024);

        // CURCNTR (cursor offsets relative to PIPE_A + 0x70080)
        assert_eq!(CURCNTR(display, PIPE_A as u32).reg, 0x70080);
        assert_eq!(CURCNTR(display, PIPE_B as u32).reg, 0x71080);
        assert_eq!(CURCNTR(display, PIPE_C as u32).reg, 0x72080);
        assert_eq!(CURCNTR(display, PIPE_D as u32).reg, 0x73080);

        // PIPE_DATA_M1 (trans offsets relative to TRANSCODER_A + 0x60030)
        assert_eq!(PIPE_DATA_M1(display, TRANSCODER_A as u32).reg, 0x60030);
        assert_eq!(PIPE_DATA_M1(display, TRANSCODER_B as u32).reg, 0x61030);
        assert_eq!(PIPE_DATA_M1(display, TRANSCODER_C as u32).reg, 0x62030);
        assert_eq!(PIPE_DATA_M1(display, TRANSCODER_D as u32).reg, 0x63030);
        assert_eq!(PIPE_DATA_M1(display, TRANSCODER_EDP as u32).reg, 0x6f030);

        // DSPCNTR (pipe offsets relative to PIPE_A + 0x70180)
        assert_eq!(DSPCNTR(display, PIPE_A as u32).reg, 0x70180);
        assert_eq!(DSPCNTR(display, PIPE_B as u32).reg, 0x71180);
        assert_eq!(DSPCNTR(display, PIPE_C as u32).reg, 0x72180);
        assert_eq!(DSPCNTR(display, PIPE_D as u32).reg, 0x73180);

        // TRANS_HTOTAL (trans offsets relative to TRANSCODER_A + 0x60000)
        assert_eq!(TRANS_HTOTAL(display, TRANSCODER_A as u32).reg, 0x60000);
        assert_eq!(TRANS_HTOTAL(display, TRANSCODER_B as u32).reg, 0x61000);
        assert_eq!(TRANS_HTOTAL(display, TRANSCODER_C as u32).reg, 0x62000);
        assert_eq!(TRANS_HTOTAL(display, TRANSCODER_D as u32).reg, 0x63000);
        assert_eq!(TRANS_HTOTAL(display, TRANSCODER_EDP as u32).reg, 0x6f000);

        // TRANS_PSR_IMR (trans offsets relative to TRANSCODER_A + 0x60814)
        assert_eq!(TRANS_PSR_IMR(display, TRANSCODER_A as u32).reg, 0x60814);
        assert_eq!(TRANS_PSR_IMR(display, TRANSCODER_B as u32).reg, 0x61814);
        assert_eq!(TRANS_PSR_IMR(display, TRANSCODER_C as u32).reg, 0x62814);
        assert_eq!(TRANS_PSR_IMR(display, TRANSCODER_D as u32).reg, 0x63814);
        assert_eq!(TRANS_PSR_IMR(display, TRANSCODER_EDP as u32).reg, 0x6f814);

        // PIPE_CRC_CTL (trans offsets relative to TRANSCODER_A + 0x60050)
        assert_eq!(PIPE_CRC_CTL(display, PIPE_A as u32).reg, 0x60050);
        assert_eq!(PIPE_CRC_CTL(display, PIPE_B as u32).reg, 0x61050);
        assert_eq!(PIPE_CRC_CTL(display, PIPE_C as u32).reg, 0x62050);
        assert_eq!(PIPE_CRC_CTL(display, PIPE_D as u32).reg, 0x63050);

        // PP_STATUS (mmio_base 0x61200 + pps_idx * 0x100)
        assert_eq!(PP_STATUS(display, 0).reg, 0x61200);
        assert_eq!(PP_STATUS(display, 1).reg, 0x61300);
    }
}
