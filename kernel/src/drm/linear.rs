//! Fixed-mode KMS over an already scanning linear surface.
//!
//! Like Linux sysfb/efidrm.c and drm_sysfb_modeset.c this never programs a
//! controller: dumb buffers are ordinary RAM and presentation copies pixels
//! into the boot aperture. Completion is software-timed, not hardware vblank.
use alloc::{sync::Arc, vec::Vec};

use axhal::{
    mem::{PAGE_SIZE_4K, PhysAddr, phys_to_virt},
    paging::PageSize,
};
use spin::Mutex;

use super::{
    DisplayAdapter, DrmDevice, DrmError, DrmResult, DumbRequest, GemBacking, Mode, Scanout,
    fence::Fence,
};
use crate::{
    mm::SharedPages,
    pseudofs::dev::{
        bootfb::BootFb,
        scanout::{PixelLayout, ScanoutSurface},
    },
};

struct RamBacking {
    pages: Arc<SharedPages>,
}
impl GemBacking for RamBacking {
    fn shared_pages(&self) -> DrmResult<Arc<SharedPages>> {
        Ok(self.pages.clone())
    }
}

struct LinearAdapter {
    surface: Arc<dyn ScanoutSurface>,
    name: &'static str,
    copy: Mutex<()>,
}

/// Validate everything before touching the screen, including a crop's last row.
fn source_range(scanout: &Scanout, mode: Mode) -> DrmResult<core::ops::Range<usize>> {
    if !scanout.offset.is_multiple_of(4)
        || scanout.mode != mode
        || scanout.width != mode.width
        || scanout.height != mode.height
        || scanout.bpp != 32
        || !matches!(scanout.format, 0x3432_5258 | 0x3432_5241)
        || scanout.pitch < mode.width.checked_mul(4).ok_or(DrmError::Overflow)?
        || !scanout.pitch.is_multiple_of(4)
        || mode.height == 0
    {
        return Err(DrmError::Invalid);
    }
    let end = scanout
        .offset
        .checked_add(u64::from(mode.height - 1) * u64::from(scanout.pitch))
        .and_then(|x| x.checked_add(u64::from(mode.width) * 4))
        .ok_or(DrmError::Overflow)?;
    if end > scanout.backing_size {
        return Err(DrmError::Invalid);
    }
    Ok(
        usize::try_from(scanout.offset).map_err(|_| DrmError::Overflow)?
            ..usize::try_from(end).map_err(|_| DrmError::Overflow)?,
    )
}

/// Convert in RAM, then copy a bounded span into the aperture. In particular,
/// do not turn a UHD frame into eight million virtual calls and tiny UC writes.
/// The caller validates/resolves all source pages before the first write.
fn copy_rows(
    scanout: &Scanout,
    layout: PixelLayout,
    destination_pitch: usize,
    mut read_pixel: impl FnMut(usize) -> u32,
    mut write: impl FnMut(usize, &[u8]) -> DrmResult<()>,
) -> DrmResult<()> {
    if !layout.is_drawable() {
        return Err(DrmError::Invalid);
    }
    let pixel_bytes = layout.bytes_per_pixel() as usize;
    let mut batch = [0u8; 2048];
    let pixels_per_batch = batch.len() / pixel_bytes;
    for y in 0..scanout.height as usize {
        let source = scanout.offset as usize + y * scanout.pitch as usize;
        let mut x = 0;
        while x < scanout.width as usize {
            let count = (scanout.width as usize - x).min(pixels_per_batch);
            for pixel in 0..count {
                let color = read_pixel(source + (x + pixel) * 4);
                layout.encode_into(color, &mut batch[pixel * pixel_bytes..][..pixel_bytes]);
            }
            write(
                y * destination_pitch + x * pixel_bytes,
                &batch[..count * pixel_bytes],
            )?;
            x += count;
        }
    }
    Ok(())
}

impl DisplayAdapter for LinearAdapter {
    fn driver_name(&self) -> &'static str {
        self.name
    }
    fn platform_name(&self) -> Option<&'static str> {
        Some(if self.name == "simpledrm" {
            "simple-framebuffer.0"
        } else {
            "intel-linear-framebuffer.0"
        })
    }
    fn fixed_mode(&self) -> Option<Mode> {
        Some(self.preferred_mode())
    }
    fn supports_cursor(&self) -> bool {
        false
    }
    fn preferred_mode(&self) -> Mode {
        // Multiboot supplies geometry, not refresh. 60 Hz is a software event
        // cadence and is not a measurement of the firmware's actual timings.
        Mode {
            width: self.surface.width(),
            height: self.surface.height(),
            refresh_millihz: 60_000,
        }
    }
    fn create_dumb(
        &self,
        request: DumbRequest,
        pitch: u32,
        size: u64,
        owner: Arc<dyn Send + Sync>,
    ) -> DrmResult<Arc<dyn GemBacking>> {
        if request.bpp != 32 || pitch < request.width.checked_mul(4).ok_or(DrmError::Overflow)? {
            return Err(DrmError::Unsupported);
        }
        let size = usize::try_from(size).map_err(|_| DrmError::Overflow)?;
        let aligned = size
            .checked_add(PAGE_SIZE_4K - 1)
            .ok_or(DrmError::Overflow)?
            & !(PAGE_SIZE_4K - 1);
        let pages =
            SharedPages::new_fixed(aligned, PageSize::Size4K).map_err(|_| DrmError::NoMemory)?;
        pages
            .retain_allocation_owner(owner)
            .map_err(|_| DrmError::NoMemory)?;
        let pages = Arc::try_new(pages).map_err(|_| DrmError::NoMemory)?;
        Arc::try_new(RamBacking { pages })
            .map(|x| x as Arc<dyn GemBacking>)
            .map_err(|_| DrmError::NoMemory)
    }
    fn present(&self, scanout: Scanout) -> DrmResult<Arc<Fence>> {
        let range = source_range(&scanout, self.preferred_mode())?;
        let pages = scanout.backing.shared_pages()?;
        let mut physical = Vec::new();
        physical
            .try_reserve_exact(range.end.div_ceil(PAGE_SIZE_4K))
            .map_err(|_| DrmError::NoMemory)?;
        // Resolve the entire fixed backing before the first screen write.
        for index in 0..range.end.div_ceil(PAGE_SIZE_4K) {
            physical.push(
                pages
                    .paddr_at(index)
                    .map_err(|_| DrmError::Invalid)?
                    .as_usize(),
            );
        }
        let _copy = self.copy.lock();
        copy_rows(
            &scanout,
            self.surface.pixel_layout(),
            self.surface.pitch() as usize,
            |offset| {
                // Source offsets are dword-aligned; no dword straddles a page.
                let address = physical[offset / PAGE_SIZE_4K] + offset % PAGE_SIZE_4K;
                // SAFETY: every page was resolved above and is retained by
                // pages; the validated dword lies within that fixed backing.
                unsafe {
                    phys_to_virt(PhysAddr::from_usize(address))
                        .as_ptr()
                        .cast::<u32>()
                        .read()
                }
            },
            |offset, bytes| {
                self.surface
                    .write_bytes(offset, bytes)
                    .map_err(|_| DrmError::DeviceLost)
            },
        )?;
        self.surface.present().map_err(|_| DrmError::DeviceLost)?;
        Ok(Fence::new(true))
    }
}

pub(crate) fn register(name: &'static str, surface: Arc<dyn ScanoutSurface>) -> DrmResult<()> {
    let adapter = Arc::try_new(LinearAdapter {
        surface,
        name,
        copy: Mutex::new(()),
    })
    .map_err(|_| DrmError::NoMemory)?;
    let mode = adapter.preferred_mode();
    super::register_primary_device(DrmDevice::new(adapter, 1, 2, 3, 4))?;
    axlog::info!(
        "drm: {name} /dev/dri/card0 fixed {}x{}; RAM dumb buffers, software-timed copy; \
         未在硬件上验证",
        mode.width,
        mode.height
    );
    Ok(())
}

pub(crate) fn init_firmware() -> DrmResult<bool> {
    let Some(framebuffer) = axhal::boot::framebuffer() else {
        return Ok(false);
    };
    let surface = BootFb::new(&framebuffer).map_err(|_| DrmError::DeviceLost)?;
    let surface = Arc::try_new(surface).map_err(|_| DrmError::NoMemory)?;
    register("simpledrm", surface)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Empty;
    impl GemBacking for Empty {
        fn shared_pages(&self) -> DrmResult<Arc<SharedPages>> {
            Err(DrmError::Invalid)
        }
    }
    fn scanout() -> Scanout {
        Scanout {
            backing: Arc::new(Empty),
            width: 2,
            height: 2,
            pitch: 16,
            bpp: 32,
            format: 0x3432_5258,
            framebuffer_width: 4,
            framebuffer_height: 2,
            backing_size: 32,
            framebuffer_offset: 0,
            offset: 4,
            source_x: 1,
            source_y: 0,
            mode: Mode {
                width: 2,
                height: 2,
                refresh_millihz: 60_000,
            },
            damage: None,
        }
    }
    #[test]
    fn fixed_mode_crop_and_last_row_are_bounded_before_copy() {
        let mut s = scanout();
        let mode = s.mode;
        assert_eq!(source_range(&s, mode), Ok(4..28));
        s.backing_size = 27;
        assert_eq!(source_range(&s, mode), Err(DrmError::Invalid));
        s = scanout();
        s.mode.width = 3;
        assert_eq!(source_range(&s, mode), Err(DrmError::Invalid));
        s = scanout();
        s.offset = u64::MAX & !3;
        assert_eq!(source_range(&s, mode), Err(DrmError::Overflow));
        s = scanout();
        s.pitch = 7;
        assert_eq!(source_range(&s, mode), Err(DrmError::Invalid));
    }

    #[test]
    fn copied_rows_preserve_crop_pitch_and_convert_before_aperture_writes() {
        let mut s = scanout();
        s.offset = 4092;
        let layout = PixelLayout {
            bits: 16,
            red: crate::pseudofs::dev::scanout::ColorChannel {
                position: 11,
                size: 5,
            },
            green: crate::pseudofs::dev::scanout::ColorChannel {
                position: 5,
                size: 6,
            },
            blue: crate::pseudofs::dev::scanout::ColorChannel {
                position: 0,
                size: 5,
            },
        };
        let mut source_offsets = Vec::new();
        let mut destination = [0x55u8; 12];
        let mut writes = 0;
        copy_rows(
            &s,
            layout,
            6,
            |offset| {
                source_offsets.push(offset);
                [0x00ff0000, 0x0000ff00, 0x000000ff, 0x00ffffff][source_offsets.len() - 1]
            },
            |offset, bytes| {
                destination[offset..offset + bytes.len()].copy_from_slice(bytes);
                writes += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(source_offsets, [4092, 4096, 4108, 4112]);
        assert_eq!(
            destination,
            [
                0, 0xf8, 0xe0, 7, 0x55, 0x55, 0x1f, 0, 0xff, 0xff, 0x55, 0x55
            ]
        );
        assert_eq!(writes, 2);
    }

    #[test]
    fn uhd_copy_batches_writes_and_stops_on_destination_failure() {
        let mut s = scanout();
        s.offset = 0;
        s.width = 3840;
        s.height = 2160;
        s.pitch = s.width * 4;
        let mut writes = 0;
        let mut bytes = 0;
        copy_rows(
            &s,
            PixelLayout::B8G8R8A8,
            s.pitch as usize,
            |_| 0x00123456,
            |_, batch| {
                assert!(batch.len() <= 2048);
                assert_eq!(&batch[..4], &[0x56, 0x34, 0x12, 0]);
                writes += 1;
                bytes += batch.len();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(writes, 8 * 2160);
        assert_eq!(bytes, 3840 * 2160 * 4);
        let mut attempts = 0;
        assert_eq!(
            copy_rows(
                &s,
                PixelLayout::B8G8R8A8,
                s.pitch as usize,
                |_| 0,
                |_, _| {
                    attempts += 1;
                    Err(DrmError::DeviceLost)
                }
            ),
            Err(DrmError::DeviceLost)
        );
        assert_eq!(attempts, 1);
    }
}
