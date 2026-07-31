//! BCM2835/6/7 HDMI transmitter (VC4 pixel valve + HDMI block).
//!
//! The HDMI block at `0x3f90_2000` consumes pixels from the HVS channel 0
//! FIFO and serializes them over the DVI/HDMI PHY. This driver programs a
//! fixed CEA-861 1080p60 mode: timing registers, RGB CSC passthrough, the
//! digital PHY, and the scheduler/packet engine.
//!
//! The pixel clock itself is owned by the GPU firmware; the caller must have
//! requested `CLK_PIXEL = 148.5 MHz` (and `CLK_PIXEL_BVB`) through the
//! mailbox before calling [`Hdmi::configure`].

use core::ptr::{read_volatile, write_volatile};

use crate::regs::{field, *};

pub struct Hdmi {
    base: usize,
}

impl Hdmi {
    /// `base` is the ARM-view physical address of the HDMI block
    /// (`0x3f90_2000`).
    pub fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    /// Resets the HDMI core and stops the pixel clock until configured
    /// (mirrors `vc4_hdmi_reset` and the disable path).
    pub fn reset(&self) {
        self.write(HDMI_M_CTL, VC4_HD_M_SW_RST);
        // ~1 us pause; busy-wait via a volatile read.
        for _ in 0..1000 {
            let _ = self.read(HDMI_CORE_REV);
        }
        self.write(HDMI_M_CTL, 0);
        self.write(HDMI_SW_RESET_CONTROL, VC4_HDMI_SW_RESET_FORMAT_DETECT);
        for _ in 0..1000 {
            let _ = self.read(HDMI_CORE_REV);
        }
        self.write(HDMI_SW_RESET_CONTROL, 0);

        self.write(HDMI_DVP_CTL, 0);
        self.write(
            HDMI_CLOCK_STOP,
            self.read(HDMI_CLOCK_STOP) | VC4_DVP_HT_CLOCK_STOP_PIXEL,
        );
    }

    /// Programs the fixed 1080p60 timing (mirrors `vc4_hdmi_set_timings`)
    /// plus RGB CSC passthrough and the digital PHY reset.
    pub fn configure(&self, mode: &ModeTiming) {
        let hsync_pos = mode.hsync_positive;
        let vsync_pos = mode.vsync_positive;
        let verta = field!(
            mode.vsync_end - mode.vsync_start,
            !0u32,
            VC4_HDMI_VERTA_VSP_SHIFT
        ) | field!(
            mode.vsync_start - mode.vdisplay,
            !0u32,
            VC4_HDMI_VERTA_VFP_SHIFT
        ) | field!(mode.vdisplay, !0u32, VC4_HDMI_VERTA_VAL_SHIFT);
        let vertb = field!(0, !0u32, VC4_HDMI_VERTB_VSPO_SHIFT)
            | field!(
                mode.vtotal - mode.vsync_end,
                !0u32,
                VC4_HDMI_VERTB_VBP_SHIFT
            );

        self.write(
            HDMI_HORZA,
            (if vsync_pos { VC4_HDMI_HORZA_VPOS } else { 0 })
                | (if hsync_pos { VC4_HDMI_HORZA_HPOS } else { 0 })
                | field!(mode.hdisplay, !0u32, VC4_HDMI_HORZA_HAP_SHIFT),
        );
        self.write(
            HDMI_HORZB,
            field!(
                mode.htotal - mode.hsync_end,
                !0u32,
                VC4_HDMI_HORZB_HBP_SHIFT
            ) | field!(
                mode.hsync_end - mode.hsync_start,
                !0u32,
                VC4_HDMI_HORZB_HSP_SHIFT
            ) | field!(
                mode.hsync_start - mode.hdisplay,
                !0u32,
                VC4_HDMI_HORZB_HFP_SHIFT
            ),
        );
        self.write(HDMI_VERTA0, verta);
        self.write(HDMI_VERTA1, verta);
        self.write(HDMI_VERTB0, vertb);
        self.write(HDMI_VERTB1, vertb);

        let misc = self.read(HDMI_MISC_CONTROL) & !VC4_HDMI_MISC_CONTROL_PIXEL_REP_MASK;
        self.write(HDMI_MISC_CONTROL, misc);

        // RGB passthrough: no CSC coefficients needed, keep the CSC disabled.
        self.write(HDMI_CSC_CTL, 0);
        for reg in [
            HDMI_CSC_12_11,
            HDMI_CSC_14_13,
            HDMI_CSC_22_21,
            HDMI_CSC_24_23,
            HDMI_CSC_32_31,
            HDMI_CSC_34_33,
        ] {
            self.write(reg, 0);
        }

        // Reset the digital PHY (mirrors `vc4_hdmi_phy_init`).
        self.write(HDMI_TX_PHY_RESET_CTL, 0xf << 16);
        self.write(HDMI_TX_PHY_RESET_CTL, 0);

        // Scheduler: manual format, ignore vsync predicts (mirrors the
        // encoder enable path).
        self.write(
            HDMI_SCHEDULER_CONTROL,
            self.read(HDMI_SCHEDULER_CONTROL)
                | VC4_HDMI_SCHEDULER_CONTROL_MANUAL_FORMAT
                | VC4_HDMI_SCHEDULER_CONTROL_IGNORE_VSYNC_PREDICTS,
        );
        self.write(HDMI_FIFO_CTL, VC4_HDMI_FIFO_CTL_MASTER_SLAVE_N);
    }

    /// Starts pixel output: restarts the pixel clock and enables the video
    /// path.
    pub fn enable(&self) {
        self.write(
            HDMI_VID_CTL,
            self.read(HDMI_VID_CTL) & !VC4_HDMI_VID_CTL_BLANKPIX,
        );
        self.write(
            HDMI_CLOCK_STOP,
            self.read(HDMI_CLOCK_STOP) & !VC4_DVP_HT_CLOCK_STOP_PIXEL,
        );
        self.write(HDMI_M_CTL, VC4_HD_M_ENABLE);
        self.write(
            HDMI_VID_CTL,
            self.read(HDMI_VID_CTL) | VC4_HDMI_VID_CTL_ENABLE,
        );
    }

    /// Stops pixel output.
    pub fn disable(&self) {
        self.write(
            HDMI_VID_CTL,
            self.read(HDMI_VID_CTL) & !VC4_HDMI_VID_CTL_ENABLE,
        );
        self.write(
            HDMI_VID_CTL,
            self.read(HDMI_VID_CTL) | VC4_HDMI_VID_CTL_BLANKPIX,
        );
        self.write(
            HDMI_CLOCK_STOP,
            self.read(HDMI_CLOCK_STOP) | VC4_DVP_HT_CLOCK_STOP_PIXEL,
        );
    }
}
