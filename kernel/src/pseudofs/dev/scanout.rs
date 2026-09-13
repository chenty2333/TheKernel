//! The backing store `/dev/fb0` and the in-kernel console draw into.
//!
//! Two different things can own the pixels of the kernel's single scanout.  A
//! DRM dumb buffer on a virtio-gpu is published by an atomic commit which has
//! to be issued before anything becomes visible.  The linear aperture the
//! firmware's GOP already programmed is the display controller's own memory,
//! so a write is visible as soon as it lands and there is nothing to publish.
//!
//! Everything else is shared: the fbdev ABI, the damage tracker, the refresh
//! worker and the console treat the surface the same way.  This trait is where
//! that shared treatment is written down, so the two backends differ only in
//! the operations below rather than in the code which drives them.

use axerrno::AxResult;

use crate::pseudofs::DeviceMmap;

/// How a surface stores one pixel, as the display stack states it.
///
/// The type itself lives with the other OS-neutral display mechanisms in
/// `axgpu`, because the firmware boot description and the display driver
/// interface have to name it too and neither can see this module. Re-exported
/// here so the surface abstraction and the format it is written in are read
/// from the same place.
pub(crate) use axgpu::{ColorChannel, PixelLayout};

/// One scanout the kernel can draw into and publish.
///
/// Geometry is expressed as width, height and pitch rather than as a DRM
/// mode.  A firmware framebuffer has no refresh rate to report, and the fbdev
/// ABI must not be handed a fabricated one, so a caller which needs only the
/// visible extent asks for exactly that.
pub(crate) trait ScanoutSurface: Send + Sync {
    /// Visible width in pixels.
    fn width(&self) -> u32;

    /// Visible height in pixels.
    fn height(&self) -> u32;

    /// Bytes between the starts of two consecutive scan lines.
    ///
    /// This is not necessarily the visible width times the pixel size:
    /// backends pad scan lines, and a caller which recomputed the stride
    /// instead of asking would shear the image.
    fn pitch(&self) -> u32;

    /// How this surface stores one pixel.
    fn pixel_layout(&self) -> PixelLayout;

    /// Bytes one pixel occupies.
    fn bytes_per_pixel(&self) -> u32 {
        self.pixel_layout().bytes_per_pixel()
    }

    /// Write one pixel at `offset` bytes from the start of the surface.
    ///
    /// `color` is a canonical `0x00RRGGBB` value and the surface encodes it in
    /// its own layout, so a caller never has to know the pixel format to draw
    /// into it.  An out-of-range offset is ignored rather than reported: the
    /// console clips its own output, and a console write must not be able to
    /// fault the kernel.
    fn write_pixel(&self, offset: usize, color: u32);

    /// Height of the addressable surface.
    ///
    /// This can exceed [`Self::height`] when the backend can pan.
    fn virtual_height(&self) -> u32;

    /// Current vertical pan offset in pixels.
    fn yoffset(&self) -> u32;

    /// Bytes addressable from the start of the surface.
    fn len(&self) -> usize;

    /// Copy `dst.len()` bytes out of the surface, starting `offset` bytes in.
    fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> AxResult<()>;

    /// Copy `src` into the surface, starting `offset` bytes in.
    fn write_bytes(&self, offset: usize, src: &[u8]) -> AxResult<()>;

    /// How `/dev/fb0` should be mapped for a userspace writer.
    fn mmap(&self) -> DeviceMmap;

    /// Make the surface's current contents visible.
    ///
    /// A backend whose writes are already visible must still accept this, so
    /// that the damage tracker and the explicit publication points in the
    /// fbdev ABI keep one meaning on every backend.
    fn present(&self) -> AxResult<()>;

    /// Move the displayed window within a taller surface.
    fn pan(&self, yoffset: u32) -> AxResult<()>;

    /// Blank or unblank the display.
    fn set_blank(&self, blank: bool) -> AxResult<()>;

    /// Re-establish this surface as the console's content.
    ///
    /// `nonblocking` asks the backend not to wait for a host presentation, so
    /// that a seat rollback cannot stall on hardware which has stopped
    /// answering.
    fn restore_text(&self, nonblocking: bool) -> AxResult<()>;

    /// Take or yield exclusive ownership of the display, if the backend has
    /// any to give.
    ///
    /// The console holds it while text is active, so that a graphics client
    /// cannot be handed a seat the kernel is still drawing into.
    fn set_master(&self, master: bool) -> AxResult<()>;
}
