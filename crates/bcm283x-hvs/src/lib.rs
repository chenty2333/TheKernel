//! Low-level driver for the Broadcom VideoCore IV HVS display controller
//! (Raspberry Pi 0-3 and Zero 2W).
//!
//! `#![no_std]`, OS-independent. The crate drives the HVS compositor and the
//! downstream HDMI transmitter with a fixed CEA-861 1080p60 output, and
//! exposes a [`rdif_display::Interface`] framebuffer implementation. The
//! framebuffer and display-list memory are supplied by the caller (OS glue)
//! as contiguous DMA memory; the pixel clock is requested from the GPU
//! firmware through the mailbox.

#![cfg_attr(not(test), no_std)]

use core::fmt;

use rdif_base::DriverGeneric;
use rdif_display::{DisplayError, DisplayInfo, FrameBuffer, Interface, PixelFormat};

pub mod hdmi;
pub mod hvs;
pub mod mailbox;
pub mod regs;

use hvs::{DLIST_WORDS, Hvs};
use regs::*;

/// Resources supplied by the OS glue: physical/virtual addresses of the
/// framebuffer and display list, plus the hardware bases.
#[derive(Clone, Copy, Debug)]
pub struct HvsResources {
    /// ARM-view physical address of the HVS (`0x3f40_0000`).
    pub hvs_base: usize,
    /// ARM-view physical address of the HDMI block (`0x3f90_2000`).
    pub hdmi_base: usize,
    /// Bus (physical) address of the framebuffer; must be 32-byte aligned
    /// and writable by the GPU.
    pub fb_bus_addr: u32,
    /// Virtual address of the framebuffer for CPU access.
    pub fb_vaddr: usize,
    /// Size of the framebuffer in bytes.
    pub fb_size: usize,
    /// Bus (physical) address of the display list; 32-byte aligned.
    pub dlist_bus_addr: u32,
    /// Virtual address of the display list.
    pub dlist_vaddr: usize,
}

/// Display configuration: framebuffer geometry and format.
#[derive(Clone, Copy, Debug)]
pub struct HvsDisplayConfig {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: PixelFormat,
}

impl HvsDisplayConfig {
    pub const fn rgb888(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            stride: width * 3,
            format: PixelFormat::Rgb888,
        }
    }

    pub const fn xrgb8888(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Xrgb8888,
        }
    }

    pub const fn rgb565(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            stride: width * 2,
            format: PixelFormat::Rgb565,
        }
    }

    pub const fn fb_size(&self) -> usize {
        (self.stride as usize) * (self.height as usize)
    }
}

/// A framebuffer-backed HVS + HDMI display.
pub struct HvsDisplay {
    resources: HvsResources,
    config: HvsDisplayConfig,
    hvs: Hvs,
    /// Kept for lifecycle/debug (the HDMI block is configured at construction).
    #[allow(dead_code)]
    hdmi: hdmi::Hdmi,
}

impl fmt::Debug for HvsDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HvsDisplay")
            .field("resources", &self.resources)
            .field("config", &self.config)
            .finish()
    }
}

impl HvsDisplay {
    /// Brings up the display: requests the pixel clock from the firmware,
    /// resets and configures the HDMI transmitter, initializes the HVS
    /// channel, and installs the initial display list.
    pub fn new(resources: HvsResources, config: HvsDisplayConfig) -> Result<Self, DisplayError> {
        if resources.fb_size < config.fb_size() {
            return Err(DisplayError::InvalidFramebuffer);
        }
        let hvs = Hvs::new(resources.hvs_base);
        let hdmi = hdmi::Hdmi::new(resources.hdmi_base);

        // Ask the firmware for the 1080p60 pixel clock.
        let pixel_rate = MODE_1080P60.pixel_clock_khz * 1000;
        if !mailbox::set_clock_rate(CLK_PIXEL, pixel_rate) {
            log::warn!("bcm283x-hvs: firmware rejected pixel clock request; output may fail");
        }
        // The BVB clock must be >= 100 MHz for 1080p60.
        if !mailbox::set_clock_rate(CLK_PIXEL_BVB, 150_000_000) {
            log::warn!("bcm283x-hvs: firmware rejected pixel-bvb clock request");
        }

        hdmi.reset();
        hdmi.configure(&MODE_1080P60);

        hvs.enable();
        hvs.init_channel(&MODE_1080P60);

        let dlist = unsafe { &mut *((resources.dlist_vaddr) as *mut [u32; DLIST_WORDS]) };
        Self::write_dlist(dlist, resources, config);
        hvs.install_dlist(resources.dlist_bus_addr);

        hdmi.enable();

        log::info!(
            "bcm283x-hvs: HVS+HDMI up at {}x{} (stride {}, fb {} bytes)",
            config.width,
            config.height,
            config.stride,
            config.fb_size()
        );

        Ok(Self {
            resources,
            config,
            hvs,
            hdmi,
        })
    }

    fn write_dlist(
        dlist: &mut [u32; DLIST_WORDS],
        resources: HvsResources,
        config: HvsDisplayConfig,
    ) {
        let format = match config.format {
            PixelFormat::Rgb565 => HVS_PIXEL_FORMAT_RGB565,
            PixelFormat::Rgb888 => HVS_PIXEL_FORMAT_RGB888,
            _ => HVS_PIXEL_FORMAT_RGBA8888,
        };
        hvs::build_dlist(
            dlist,
            resources.fb_bus_addr,
            config.width,
            config.height,
            config.stride,
            format,
        );
    }

    /// Re-points the display list at the current framebuffer and asks the
    /// HVS to pick it up. Call after writing new frame data.
    pub fn flush(&mut self) {
        let dlist = unsafe { &mut *((self.resources.dlist_vaddr) as *mut [u32; DLIST_WORDS]) };
        Self::write_dlist(dlist, self.resources, self.config);
        self.hvs.install_dlist(self.resources.dlist_bus_addr);
    }

    pub fn framebuffer_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(
                self.resources.fb_vaddr as *mut u8,
                self.resources.fb_size,
            )
        }
    }

    pub fn resources(&self) -> &HvsResources {
        &self.resources
    }
}

impl DriverGeneric for HvsDisplay {
    fn name(&self) -> &str {
        "bcm283x-hvs"
    }
}

impl Interface for HvsDisplay {
    fn info(&self) -> DisplayInfo {
        DisplayInfo {
            width: self.config.width,
            height: self.config.height,
            stride: self.config.stride as usize,
            format: self.config.format,
            fb_size: self.config.fb_size(),
        }
    }

    fn framebuffer(&mut self) -> Result<FrameBuffer<'_>, DisplayError> {
        let fb = self.framebuffer_mut();
        Ok(FrameBuffer::from_slice(fb))
    }

    fn need_flush(&self) -> bool {
        true
    }

    fn flush(&mut self) -> Result<(), DisplayError> {
        self.flush();
        Ok(())
    }

    fn irq_num(&self) -> Option<usize> {
        // The HVS IRQ (65 on BCM283x) is only used for underrun reporting;
        // framebuffer updates are fire-and-forget.
        Some(HVS_IRQ)
    }
}
