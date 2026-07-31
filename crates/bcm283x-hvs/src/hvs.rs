//! VideoCore IV HVS display controller.
//!
//! The HVS is a compositor: it reads a display list (`dlist`) of plane
//! descriptors from memory and blends them into the channel FIFO consumed by
//! the downstream pixel valve (HDMI/DSI). This driver programs channel 0 with
//! a single full-screen linear framebuffer plane.

use core::ptr::{read_volatile, write_volatile};

use crate::regs::{field, *};

/// Number of 32-bit words in the single-plane display list.
pub const DLIST_WORDS: usize = 8;

/// Builds the display list for one full-screen linear plane.
///
/// Layout (VC4 HVS4):
/// ```text
/// [0] control: VALID | RGBA_EXPAND_ROUND | ORDER | PIXEL_FORMAT | UNITY
/// [1] position 0: alpha | START_X | START_Y
/// [2] position 2: ALPHA_MODE_FIXED | HEIGHT | WIDTH   (unity skips pos 1)
/// [3] context (HVS-owned)
/// [4] pointer: framebuffer bus address
/// [5] pointer context (HVS-owned)
/// [6] pitch: SRC_PITCH | 0
/// [7] 0 (padding/end)
/// ```
pub fn build_dlist(
    dlist: &mut [u32; DLIST_WORDS],
    fb_bus_addr: u32,
    width: u32,
    height: u32,
    stride: u32,
    pixel_format: u32,
) {
    dlist[0] = SCALER_CTL0_VALID
        | field!(
            SCALER_CTL0_RGBA_EXPAND_ROUND,
            SCALER_CTL0_RGBA_EXPAND_ROUND,
            SCALER_CTL0_RGBA_EXPAND_SHIFT
        )
        | (pixel_format << SCALER_CTL0_PIXEL_FORMAT_SHIFT)
        | SCALER_CTL0_UNITY
        | field!(0, SCALER_CTL0_TILING_LINEAR, SCALER_CTL0_TILING_SHIFT);
    dlist[1] = 0xff << SCALER_POS0_FIXED_ALPHA_SHIFT; // opaque
    dlist[2] = field!(
        SCALER_POS2_ALPHA_MODE_FIXED,
        SCALER_POS2_ALPHA_MODE_FIXED,
        SCALER_POS2_ALPHA_MODE_SHIFT
    ) | (height << SCALER_POS2_HEIGHT_SHIFT)
        | (width << SCALER_POS2_WIDTH_SHIFT);
    dlist[3] = 0xc0c0_c0c0; // HVS-owned context
    dlist[4] = fb_bus_addr;
    dlist[5] = 0xc0c0_c0c0; // HVS-owned context
    dlist[6] = stride & SCALER_SRC_PITCH_MASK;
    dlist[7] = 0;
}

/// HVS channel 0 controller.
pub struct Hvs {
    base: usize,
}

impl Hvs {
    /// `base` is the ARM-view physical address of the HVS (`0x3f40_0000`).
    pub fn new(base: usize) -> Self {
        Self { base }
    }

    fn read(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base + offset) as *const u32) }
    }

    fn write(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base + offset) as *mut u32, value) }
    }

    /// Enables the HVS global scaler.
    pub fn enable(&self) {
        self.write(
            SCALER_DISPCTRL,
            self.read(SCALER_DISPCTRL) | SCALER_DISPCTRL_ENABLE,
        );
    }

    /// Programs channel 0 for the given output mode (mirrors
    /// `vc4_hvs_init_channel` for VC4_GEN_4).
    pub fn init_channel(&self, mode: &ModeTiming) {
        // Reset the channel.
        self.write(SCALER_DISPCTRL0, 0);
        self.write(SCALER_DISPCTRL0, SCALER_DISPCTRL0_RESET);
        self.write(SCALER_DISPCTRL0, 0);

        // Enable the scaler; it waits for VSTART to start compositing.
        let dispctrl = SCALER_DISPCTRL0_ENABLE
            | field!(
                mode.hdisplay,
                SCALER_DISPCTRL0_WIDTH_MASK,
                SCALER_DISPCTRL0_WIDTH_SHIFT
            )
            | field!(
                mode.vdisplay,
                SCALER_DISPCTRL0_HEIGHT_MASK,
                SCALER_DISPCTRL0_HEIGHT_SHIFT
            );
        self.write(SCALER_DISPCTRL0, dispctrl);

        let mut dispbkgnd = self.read(SCALER_DISPBKGND0);
        dispbkgnd |= SCALER_DISPBKGND_AUTOHS;
        dispbkgnd &= !SCALER_DISPBKGND_GAMMA;
        self.write(SCALER_DISPBKGND0, dispbkgnd | SCALER_DISPBKGND_GAMMA);
    }

    /// Installs a display list for channel 0; the hardware reads it at the
    /// next frame boundary.
    pub fn install_dlist(&self, dlist_bus_addr: u32) {
        self.write(SCALER_DISPLIST0, dlist_bus_addr);
    }
}
