//! Firmware-framebuffer DRM device on the upstream core (Linux `efidrm`).
//!
//! The bootloader's framebuffer description becomes the `struct screen_info`
//! Linux's EFI stub would have filled in, and the translated `efidrm` driver
//! registers a fixed-mode KMS device over it. This file only supplies the
//! platform services efidrm takes from outside DRM.

use alloc::sync::Arc;
use core::ffi::c_void;

use axhal::{boot::BootFramebuffer, mem::PhysAddr};
use tk_drm_upstream::{
    drm_sysfb_helper_types::{ScreenInfo, VIDEO_CAPABILITY_64BIT_BASE, VIDEO_TYPE_EFI},
    efidrm_upstream::{
        EfiMemoryDesc, SysfbMapType, SysfbPlatformOps, efidrm_probe_screen_info,
        install_sysfb_platform_ops,
    },
};

/// Linux names the platform device `efi-framebuffer.0` (sysfb_init()).
const PLATFORM_NAME: &str = "efi-framebuffer.0";
const ENXIO: i32 = 6;

/// The aperture holds no other driver's resources: i915's native modeset
/// takes it over through its own path, not through aperture removal.
unsafe fn acquire_aperture(_base: u64, _size: u64) -> i32 {
    0
}

unsafe fn request_mem_region(_base: u64, _size: u64, _name: &str) -> bool {
    true
}

/// `axmm::iomap` maps device memory uncached; there is no write-combining
/// mapping, so every requested type gets that one (correct, only slower).
unsafe fn map_io(base: u64, size: u64, _kind: SysfbMapType) -> Result<*mut c_void, i32> {
    let base = usize::try_from(base).map_err(|_| -ENXIO)?;
    let size = usize::try_from(size).map_err(|_| -ENXIO)?;
    axmm::iomap(PhysAddr::from_usize(base), size)
        .map(|virt| virt.as_mut_ptr().cast())
        .map_err(|_| -ENXIO)
}

/// The kernel keeps no EFI memory map, so efidrm falls back to the memory
/// type its probe uses when `EFI_MEMMAP` is unavailable.
fn efi_memmap_enabled() -> bool {
    false
}

fn efi_mem_desc_lookup(_phys: u64) -> Option<EfiMemoryDesc> {
    None
}

static PLATFORM_OPS: SysfbPlatformOps = SysfbPlatformOps {
    acquire_aperture,
    request_mem_region,
    map_io,
    efi_memmap_enabled,
    efi_mem_desc_lookup,
};

/// The `screen_info` an EFI boot would hand Linux for this framebuffer.
fn screen_info(framebuffer: &BootFramebuffer) -> Option<ScreenInfo> {
    let size = u32::try_from(framebuffer.byte_len()?).ok()?;
    // SAFETY: ScreenInfo is plain integer data; all-zero means "no display".
    let mut si: ScreenInfo = unsafe { core::mem::zeroed() };
    si.orig_video_isVGA = VIDEO_TYPE_EFI;
    si.lfb_width = u16::try_from(framebuffer.width).ok()?;
    si.lfb_height = u16::try_from(framebuffer.height).ok()?;
    si.lfb_depth = u16::from(framebuffer.bpp);
    si.lfb_linelength = u16::try_from(framebuffer.pitch).ok()?;
    si.lfb_base = framebuffer.address as u32;
    let high = (framebuffer.address >> 32) as u32;
    if high != 0 {
        si.ext_lfb_base = high;
        si.capabilities |= VIDEO_CAPABILITY_64BIT_BASE;
    }
    si.lfb_size = size;
    si.red_size = framebuffer.red.size;
    si.red_pos = framebuffer.red.position;
    si.green_size = framebuffer.green.size;
    si.green_pos = framebuffer.green.position;
    si.blue_size = framebuffer.blue.size;
    si.blue_pos = framebuffer.blue.position;
    if framebuffer.bpp == 32 {
        // The channel left over in a 32-bit pixel, as EFI GOP reports it.
        si.rsvd_size = 8;
        si.rsvd_pos = 24;
    }
    Some(si)
}

/// The registered device is owned by the DRM core for the kernel's lifetime.
struct EfidrmOwner;

/// Register `efidrm` over the bootloader framebuffer. Returns whether a DRM
/// device now drives it.
pub(crate) fn init_firmware() -> bool {
    let Some(framebuffer) = axhal::boot::framebuffer() else {
        return false;
    };
    let Some(si) = screen_info(&framebuffer) else {
        axlog::warn!("drm: firmware framebuffer does not fit screen_info; no efidrm device");
        return false;
    };
    if let Err(error) = install_sysfb_platform_ops(&PLATFORM_OPS) {
        axlog::warn!("drm: {error}");
    }
    // SAFETY: `si` describes the firmware framebuffer the platform ops map.
    let dev = match unsafe { efidrm_probe_screen_info(&si, None) } {
        Ok(dev) => dev,
        Err(errno) => {
            axlog::warn!("drm: efidrm probe failed with errno {errno}; firmware console retained");
            return false;
        }
    };
    let owner: Arc<dyn core::any::Any + Send + Sync> = match Arc::try_new(EfidrmOwner) {
        Ok(owner) => owner,
        Err(_) => return false,
    };
    match super::upstream::install(
        dev,
        owner,
        (framebuffer.width, framebuffer.height),
        None,
        PLATFORM_NAME,
        None,
    ) {
        Ok(device) => {
            axlog::info!(
                "drm: efidrm card{} {}x{} over the firmware framebuffer",
                device.primary_index().unwrap_or(0),
                framebuffer.width,
                framebuffer.height
            );
            true
        }
        Err(error) => {
            axlog::warn!("drm: efidrm publication failed: {error}");
            false
        }
    }
}
