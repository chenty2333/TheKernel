//! Register maps for the VideoCore IV HVS display controller and the HDMI
//! transmitter (BCM2835/2836/2837), plus the firmware mailbox interface.
//!
//! All offsets are relative to the respective ARM-view peripheral base. The
//! HVS lives at `0x3f40_0000`, the HDMI block at `0x3f90_2000`, the DVP at
//! `0x3f80_7000`, and the mailbox at `0x3f00_b880`.

/// Convenience wrapper for VC4-style register field macros.
macro_rules! field {
    ($value:expr, $mask:expr, $shift:expr) => {
        (($value) << ($shift)) & ($mask)
    };
}
pub(crate) use field;

// ---------------------------------------------------------------------------
// HVS (base 0x3f40_0000)
// ---------------------------------------------------------------------------

pub const HVS_BASE: usize = 0x3f40_0000;
/// HVS interrupt (armctrl bank 2, bit 1).
pub const HVS_IRQ: usize = 65;

pub const SCALER_DISPCTRL: usize = 0x00;
pub const SCALER_DISPSTAT: usize = 0x04;
pub const SCALER_DISPLIST0: usize = 0x20;
pub const SCALER_DISPCTRL0: usize = 0x40;
pub const SCALER_DISPBKGND0: usize = 0x44;
pub const SCALER_DISPSTAT0: usize = 0x48;

pub const SCALER_DISPCTRL_ENABLE: u32 = 1 << 31;

pub const SCALER_DISPCTRL0_ENABLE: u32 = 1 << 31;
pub const SCALER_DISPCTRL0_RESET: u32 = 1 << 30;
pub const SCALER_DISPCTRL0_WIDTH_MASK: u32 = 0x0fff_0000;
pub const SCALER_DISPCTRL0_WIDTH_SHIFT: u32 = 12;
pub const SCALER_DISPCTRL0_HEIGHT_MASK: u32 = 0x0000_0fff;
pub const SCALER_DISPCTRL0_HEIGHT_SHIFT: u32 = 0;

pub const SCALER_DISPBKGND_AUTOHS: u32 = 1 << 31;
pub const SCALER_DISPBKGND_GAMMA: u32 = 1 << 29;

// Display list words (VC4 generation, channel 0, single linear plane).
pub const SCALER_CTL0_VALID: u32 = 1 << 30;
pub const SCALER_CTL0_TILING_LINEAR: u32 = 0;
pub const SCALER_CTL0_TILING_SHIFT: u32 = 20;
pub const SCALER_CTL0_ORDER_SHIFT: u32 = 13;
pub const SCALER_CTL0_RGBA_EXPAND_ROUND: u32 = 0b11;
pub const SCALER_CTL0_RGBA_EXPAND_SHIFT: u32 = 11;
pub const SCALER_CTL0_UNITY: u32 = 1 << 4;
pub const SCALER_CTL0_PIXEL_FORMAT_SHIFT: u32 = 0;

pub const SCALER_POS0_FIXED_ALPHA_SHIFT: u32 = 24;
pub const SCALER_POS0_START_Y_SHIFT: u32 = 12;
pub const SCALER_POS0_START_X_SHIFT: u32 = 0;

pub const SCALER_POS2_ALPHA_MODE_FIXED: u32 = 0b01;
pub const SCALER_POS2_ALPHA_MODE_SHIFT: u32 = 30;
pub const SCALER_POS2_HEIGHT_SHIFT: u32 = 16;
pub const SCALER_POS2_WIDTH_SHIFT: u32 = 0;

pub const SCALER_SRC_PITCH_MASK: u32 = 0x0000_ffff;
pub const SCALER_SRC_PITCH_SHIFT: u32 = 0;

/// HVS pixel format encodings.
pub const HVS_PIXEL_FORMAT_RGB565: u32 = 4;
pub const HVS_PIXEL_FORMAT_RGB888: u32 = 5;
pub const HVS_PIXEL_FORMAT_RGBA8888: u32 = 7;

// ---------------------------------------------------------------------------
// Firmware mailbox (base 0x3f00_b880)
// ---------------------------------------------------------------------------

pub const MBOX_BASE: usize = 0x3f00_b880;
pub const MBOX_READ: usize = 0x00;
pub const MBOX_STATUS: usize = 0x18;
pub const MBOX_WRITE: usize = 0x20;

pub const MBOX_STATUS_EMPTY: u32 = 1 << 30;
pub const MBOX_STATUS_FULL: u32 = 1 << 31;

/// Property channel (channel 8) used for firmware requests.
pub const MBOX_CH_PROPERTY: u32 = 8;

/// Firmware clock ids for `CLOCK_SET_RATE`.
pub const CLK_PIXEL: u32 = 9;
pub const CLK_PIXEL_BVB: u32 = 14;

pub const TAG_CLOCK_SET_RATE: u32 = 0x0003_8002;
pub const TAG_END: u32 = 0;

// ---------------------------------------------------------------------------
// HDMI (base 0x3f90_2000)
// ---------------------------------------------------------------------------

pub const HDMI_BASE: usize = 0x3f90_2000;

pub const HDMI_CSC_CTL: usize = 0x20;
pub const HDMI_CSC_12_11: usize = 0x24;
pub const HDMI_CSC_14_13: usize = 0x28;
pub const HDMI_CSC_22_21: usize = 0x2c;
pub const HDMI_CSC_24_23: usize = 0x30;
pub const HDMI_CSC_32_31: usize = 0x34;
pub const HDMI_CSC_34_33: usize = 0x38;
pub const HDMI_HORZA: usize = 0x44;
pub const HDMI_HORZB: usize = 0x48;
pub const HDMI_VERTA0: usize = 0x4c;
pub const HDMI_VERTB0: usize = 0x50;
pub const HDMI_VERTA1: usize = 0x54;
pub const HDMI_VERTB1: usize = 0x58;
pub const HDMI_M_CTL: usize = 0x74;
pub const HDMI_MAI_CTL: usize = 0x78;
pub const HDMI_MAI_THR: usize = 0x7c;
pub const HDMI_MAI_FMT: usize = 0x80;
pub const HDMI_MAI_DATA: usize = 0x84;
pub const HDMI_MAI_CONFIG: usize = 0x88;
pub const HDMI_MAI_CHANNEL_MAP: usize = 0x8c;
pub const HDMI_RAM_PACKET_CONFIG: usize = 0xa0;
pub const HDMI_RAM_PACKET_STATUS: usize = 0xa4;
pub const HDMI_RAM_PACKET_START: usize = 0xa8;
pub const HDMI_SW_RESET_CONTROL: usize = 0xac;
pub const HDMI_SCHEDULER_CONTROL: usize = 0xb4;
pub const HDMI_CEC_CNTRL_1: usize = 0xc8;
pub const HDMI_CLOCK_STOP: usize = 0xd4;
pub const HDMI_FIFO_CTL: usize = 0xe0;
pub const HDMI_HOTPLUG: usize = 0xec;
pub const HDMI_VID_CTL: usize = 0xf0;
pub const HDMI_DVP_CTL: usize = 0xf4;
pub const HDMI_GCP_CONFIG: usize = 0xf8;
pub const HDMI_GCP_WORD_1: usize = 0xfc;
pub const HDMI_MISC_CONTROL: usize = 0x100;
pub const HDMI_MAI_SMP: usize = 0x104;
pub const HDMI_SCRAMBLER_CTL: usize = 0x108;
pub const HDMI_CORE_REV: usize = 0x130;
pub const HDMI_TX_PHY_RESET_CTL: usize = 0x2c0;
pub const HDMI_TX_PHY_POWERDOWN_CTL: usize = 0x2c4;
pub const HDMI_TX_PHY_POWERUP_CTL: usize = 0x2c8;
pub const HDMI_TX_PHY_PLL_RESET_CTL: usize = 0x2e0;
pub const HDMI_TX_PHY_PLL_POWERUP_CTL: usize = 0x2e4;
pub const HDMI_TX_PHY_PLL_POWERDOWN_CTL: usize = 0x2e8;
pub const HDMI_TX_PHY_PLL_CTL_0: usize = 0x2ec;
pub const HDMI_TX_PHY_PLL_CTL_1: usize = 0x2f0;
pub const HDMI_TX_PHY_PLL_CFG: usize = 0x2f4;
pub const HDMI_TX_PHY_PLL_CFG_PDIV: usize = 0x2f8;
pub const HDMI_TX_PHY_PLL_POST_KDIV: usize = 0x2fc;
pub const HDMI_TX_PHY_PLL_VCOCLK_DIV: usize = 0x300;
pub const HDMI_TX_PHY_PLL_REFCLK: usize = 0x304;
pub const HDMI_TX_PHY_PLL_MISC_0: usize = 0x308;
pub const HDMI_TX_PHY_PLL_MISC_1: usize = 0x30c;
pub const HDMI_TX_PHY_PLL_MISC_2: usize = 0x310;
pub const HDMI_TX_PHY_PLL_MISC_3: usize = 0x314;
pub const HDMI_TX_PHY_PLL_MISC_4: usize = 0x318;
pub const HDMI_TX_PHY_PLL_MISC_5: usize = 0x31c;
pub const HDMI_TX_PHY_PLL_MISC_6: usize = 0x320;
pub const HDMI_TX_PHY_PLL_MISC_7: usize = 0x324;
pub const HDMI_TX_PHY_PLL_MISC_8: usize = 0x328;
pub const HDMI_TX_PHY_PLL_CALIBRATION_CONFIG_1: usize = 0x32c;
pub const HDMI_TX_PHY_PLL_CALIBRATION_CONFIG_2: usize = 0x330;
pub const HDMI_TX_PHY_PLL_CALIBRATION_CONFIG_4: usize = 0x338;
pub const HDMI_TX_PHY_CLK_DIV: usize = 0x348;
pub const HDMI_TX_PHY_CHANNEL_SWAP: usize = 0x34c;

pub const VC4_HD_M_SW_RST: u32 = 1 << 2;
pub const VC4_HD_M_ENABLE: u32 = 1 << 0;
pub const VC4_HD_M_HOTPLUG: u32 = 1 << 1;
pub const VC4_HD_M_RX_CEC: u32 = 1 << 6;
pub const VC4_HD_M_CEC: u32 = 1 << 7;

pub const VC4_HDMI_HORZA_VPOS: u32 = 1 << 31;
pub const VC4_HDMI_HORZA_HPOS: u32 = 1 << 30;
pub const VC4_HDMI_HORZA_HAP_SHIFT: u32 = 0;

pub const VC4_HDMI_HORZB_HBP_SHIFT: u32 = 0;
pub const VC4_HDMI_HORZB_HSP_SHIFT: u32 = 16;
pub const VC4_HDMI_HORZB_HFP_SHIFT: u32 = 24;

pub const VC4_HDMI_VERTA_VSP_SHIFT: u32 = 0;
pub const VC4_HDMI_VERTA_VFP_SHIFT: u32 = 12;
pub const VC4_HDMI_VERTA_VAL_SHIFT: u32 = 20;

pub const VC4_HDMI_VERTB_VSPO_SHIFT: u32 = 0;
pub const VC4_HDMI_VERTB_VBP_SHIFT: u32 = 12;

pub const VC4_HDMI_SCHEDULER_CONTROL_MANUAL_FORMAT: u32 = 1 << 12;
pub const VC4_HDMI_SCHEDULER_CONTROL_IGNORE_VSYNC_PREDICTS: u32 = 1 << 5;

pub const VC4_HDMI_FIFO_CTL_MASTER_SLAVE_N: u32 = 1 << 14;

pub const VC4_HDMI_VID_CTL_CLRRGB: u32 = 1 << 9;
pub const VC4_HDMI_VID_CTL_BLANKPIX: u32 = 1 << 1;
pub const VC4_HDMI_VID_CTL_ENABLE: u32 = 1 << 0;

pub const VC4_HDMI_MISC_CONTROL_PIXEL_REP_SHIFT: u32 = 0;
pub const VC4_HDMI_MISC_CONTROL_PIXEL_REP_MASK: u32 = 0x3;

pub const VC4_DVP_HT_CLOCK_STOP_PIXEL: u32 = 1 << 1;

pub const VC4_HDMI_SW_RESET_FORMAT_DETECT: u32 = 1 << 1;
pub const VC4_HDMI_SW_RESET_HDMI: u32 = 1 << 0;

pub const VC4_HDMI_HOTPLUG_CONNECTED: u32 = 1 << 0;

/// 1080p60 CEA-861 mode timings.
pub const MODE_1080P60: ModeTiming = ModeTiming {
    hdisplay: 1920,
    hsync_start: 2008,
    hsync_end: 2052,
    htotal: 2200,
    vdisplay: 1080,
    vsync_start: 1084,
    vsync_end: 1089,
    vtotal: 1125,
    pixel_clock_khz: 148_500,
    hsync_positive: true,
    vsync_positive: true,
};

/// CEA-861 mode timing description used for the fixed HDMI output.
#[derive(Clone, Copy, Debug)]
pub struct ModeTiming {
    pub hdisplay: u32,
    pub hsync_start: u32,
    pub hsync_end: u32,
    pub htotal: u32,
    pub vdisplay: u32,
    pub vsync_start: u32,
    pub vsync_end: u32,
    pub vtotal: u32,
    pub pixel_clock_khz: u32,
    pub hsync_positive: bool,
    pub vsync_positive: bool,
}

#[allow(dead_code)]
const _: () = {
    // Verify the field macro compiles in this module context.
    let _ = field!(0, SCALER_DISPCTRL0_WIDTH_MASK, SCALER_DISPCTRL0_WIDTH_SHIFT);
};
