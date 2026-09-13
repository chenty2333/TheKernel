//! The firmware's linear framebuffer, as a `/dev/fb0` scanout surface.
//!
//! A machine whose display was programmed by its own firmware before the
//! kernel started has no display device for the kernel to drive.  The pixels
//! already live in a linear aperture and the display controller already scans
//! them out; the only thing missing is a console that writes into them.  This
//! module is that surface, and on a machine whose only output device it is, it
//! is the difference between a diagnosable boot and a black screen.

use axerrno::{AxError, AxResult};
use axhal::{
    boot::BootFramebuffer,
    mem::{PhysAddr, PhysAddrRange, VirtAddr},
};

use crate::pseudofs::{
    DeviceMmap,
    dev::scanout::{ColorChannel, PixelLayout, ScanoutSurface},
};

/// A linear framebuffer owned by firmware rather than by a kernel driver.
pub(crate) struct BootFb {
    /// Virtual address of the aperture's first byte.
    base: VirtAddr,
    /// Physical address of the aperture's first byte.
    physical: PhysAddr,
    width: u32,
    height: u32,
    pitch: u32,
    size: usize,
    /// How the firmware packs a pixel, as the bootloader reported it.
    layout: PixelLayout,
}

/// The byte range `offset..offset + len`, or `None` if it leaves `size`.
///
/// Both byte operations share this so a surface can never be read or written
/// past the length it was mapped with, whatever the arithmetic in a caller.
fn range_within(offset: usize, len: usize, size: usize) -> Option<core::ops::Range<usize>> {
    let end = offset.checked_add(len)?;
    (end <= size).then_some(offset..end)
}

/// Convert the bootloader's channel description into the surface layout form.
///
/// The pixel is rejected rather than approximated when it describes something
/// the console cannot write: a wrong stride or a wrong field would paint a
/// garbled screen, and on a machine whose only output device that screen is,
/// a garbled console is indistinguishable from a kernel that never booted.
fn pixel_layout(framebuffer: &BootFramebuffer) -> AxResult<PixelLayout> {
    if !matches!(framebuffer.bpp, 15 | 16 | 24 | 32) {
        return Err(AxError::InvalidInput);
    }
    let layout = PixelLayout {
        bits: framebuffer.bpp,
        red: ColorChannel {
            position: framebuffer.red.position,
            size: framebuffer.red.size,
        },
        green: ColorChannel {
            position: framebuffer.green.position,
            size: framebuffer.green.size,
        },
        blue: ColorChannel {
            position: framebuffer.blue.position,
            size: framebuffer.blue.size,
        },
    };
    let fits = [layout.red, layout.green, layout.blue].into_iter().all(|channel| {
        channel.size != 0
            && u16::from(channel.position) + u16::from(channel.size) <= u16::from(layout.bits)
    });
    if !fits {
        return Err(AxError::InvalidInput);
    }
    // A scan line has to hold its pixels at this depth as well as at the depth
    // the boot parser checked, because a caller could have built this
    // description without going through that parser.
    let minimum_pitch = (u64::from(framebuffer.width) * u64::from(layout.bits)).div_ceil(8);
    if u64::from(framebuffer.pitch) < minimum_pitch {
        return Err(AxError::InvalidInput);
    }
    Ok(layout)
}

impl BootFb {
    /// Map the aperture described by `framebuffer` and take it as a scanout.
    ///
    /// The mapping happens here rather than on first write, so that a failure
    /// is reported once at the point the surface is chosen instead of turning
    /// the first console character into a fault.
    ///
    /// The surface is mapped device-uncached, which is what this kernel has:
    /// `MappingFlags::DEVICE` and `MappingFlags::UNCACHED` both encode the same
    /// page-table bits, and no write-combining path exists.  That is correct
    /// here in the strong sense as well as the weak one -- the aperture is
    /// display memory the kernel only writes, so there is no read of a cached
    /// line to become stale, and the cost is one memory transaction per pixel
    /// on a surface that is written a glyph at a time.
    pub(crate) fn new(framebuffer: &BootFramebuffer) -> AxResult<Self> {
        let layout = pixel_layout(framebuffer)?;
        let size = framebuffer.byte_len().ok_or(AxError::InvalidInput)?;
        if size == 0 || framebuffer.width == 0 || framebuffer.height == 0 {
            return Err(AxError::InvalidInput);
        }
        let address =
            usize::try_from(framebuffer.address).map_err(|_| AxError::InvalidInput)?;
        let physical = PhysAddr::from_usize(address);
        // The boot parser keeps the surface clear of usable RAM, so this maps
        // display memory and can never relocate memory the kernel allocated.
        let base = axmm::iomap(physical, size)?;
        Ok(Self {
            base,
            physical,
            width: framebuffer.width,
            height: framebuffer.height,
            pitch: framebuffer.pitch,
            size,
            layout,
        })
    }
}

impl ScanoutSurface for BootFb {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn pitch(&self) -> u32 {
        self.pitch
    }

    fn pixel_layout(&self) -> PixelLayout {
        self.layout
    }

    fn write_pixel(&self, offset: usize, color: u32) {
        // The aperture is written through as bytes, so the surface gets its own
        // depth and nothing more: a 16-bit surface must not receive four bytes.
        let mut pixel = [0u8; size_of::<u32>()];
        let width = self.layout.encode_into(color, &mut pixel);
        let _ = self.write_bytes(offset, &pixel[..width]);
    }

    fn virtual_height(&self) -> u32 {
        // One screen tall: the firmware programmed where this surface starts
        // and there is no second page behind it to scroll to.
        self.height
    }

    fn yoffset(&self) -> u32 {
        0
    }

    fn len(&self) -> usize {
        self.size
    }

    fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> AxResult<()> {
        let range = range_within(offset, dst.len(), self.size).ok_or(AxError::InvalidInput)?;
        // SAFETY: the range was just checked to lie inside the aperture this
        // surface mapped, and the aperture stays mapped for this value's
        // lifetime.  Both regions are byte slices, so they cannot overlap in a
        // way `copy_nonoverlapping` would reject.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.base.as_ptr().add(range.start),
                dst.as_mut_ptr(),
                dst.len(),
            )
        };
        Ok(())
    }

    fn write_bytes(&self, offset: usize, src: &[u8]) -> AxResult<()> {
        let range = range_within(offset, src.len(), self.size).ok_or(AxError::InvalidInput)?;
        // SAFETY: as for `read_bytes`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.as_ptr(),
                self.base.as_mut_ptr().add(range.start),
                src.len(),
            )
        };
        Ok(())
    }

    fn mmap(&self) -> DeviceMmap {
        // The aperture is linear physical memory, so userspace maps it
        // directly rather than through page objects, and `smem_start` can be
        // reported truthfully instead of as a fictitious zero.
        match PhysAddrRange::try_from_start_size(self.physical, self.size) {
            Some(range) => DeviceMmap::Physical(range),
            None => DeviceMmap::None,
        }
    }

    fn present(&self) -> AxResult<()> {
        // A linear aperture is the display controller's own memory: a write is
        // visible as soon as it lands and there is nothing to submit.  The
        // call is still accepted so that the damage tracker and the explicit
        // publication points in the fbdev ABI keep one meaning across
        // backends.
        Ok(())
    }

    fn pan(&self, _yoffset: u32) -> AxResult<()> {
        // The firmware chose where this surface starts.  Moving the displayed
        // window would need a display-controller register this kernel does not
        // own, so the request is refused rather than silently ignored: a
        // caller that believes a pan happened would show the wrong page.
        Err(AxError::Unsupported)
    }

    fn set_blank(&self, _blank: bool) -> AxResult<()> {
        // There is no power control the kernel can reach from here.  Accepting
        // keeps a blanking client working; refusing would abort it over a
        // request whose failure costs nothing.
        Ok(())
    }

    fn restore_text(&self, _nonblocking: bool) -> AxResult<()> {
        // The console's pixels *are* the aperture's contents, so there is no
        // saved copy to put back and nothing that a graphics client could have
        // displaced.  Nothing to do, and nothing that can fail.
        Ok(())
    }

    fn set_master(&self, _master: bool) -> AxResult<()> {
        // No exclusive owner exists to yield: this surface is not a device
        // with a master, and whoever writes last is what the display shows.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::range_within;

    #[test]
    fn byte_range_never_leaves_the_surface() {
        assert_eq!(range_within(0, 4, 8), Some(0..4));
        assert_eq!(range_within(4, 4, 8), Some(4..8));
        // A zero-length access at the very end is still inside the surface.
        assert_eq!(range_within(8, 0, 8), Some(8..8));
        // One byte past the end is not.
        assert_eq!(range_within(4, 5, 8), None);
        assert_eq!(range_within(9, 0, 8), None);
    }

    #[test]
    fn byte_range_refuses_instead_of_wrapping() {
        // A caller which computed a huge length must get no range at all, not
        // a wrapped one that lands back inside the surface.
        assert_eq!(range_within(usize::MAX - 1, 4, 8), None);
        assert_eq!(range_within(usize::MAX, 1, 8), None);
    }
}
