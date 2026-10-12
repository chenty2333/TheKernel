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
