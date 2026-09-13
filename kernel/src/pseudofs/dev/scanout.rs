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

/// Bit position and width of one colour channel inside a pixel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ColorChannel {
    /// Index of the channel's least significant bit within a pixel.
    pub(crate) position: u8,
    /// Channel width in bits.
    pub(crate) size: u8,
}

/// How a scanout surface stores one pixel.
///
/// The console produces colours as canonical `0x00RRGGBB` values, and fbdev
/// has to describe the surface to userspace.  Both need the surface to state
/// its own layout: a firmware framebuffer is whatever its GOP mode was, which
/// is frequently not the 32-bit layout a virtio-gpu scanout is allocated with,
/// and assuming one would write a 16-bit surface at a 32-bit stride.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PixelLayout {
    /// Bits one pixel occupies.
    pub(crate) bits: u8,
    /// Red channel layout.
    pub(crate) red: ColorChannel,
    /// Green channel layout.
    pub(crate) green: ColorChannel,
    /// Blue channel layout.
    pub(crate) blue: ColorChannel,
}

impl PixelLayout {
    /// The 32-bit B8G8R8A8 layout a virtio-gpu scanout is allocated with.
    pub(crate) const B8G8R8A8: Self = Self {
        bits: 32,
        red: ColorChannel {
            position: 16,
            size: 8,
        },
        green: ColorChannel {
            position: 8,
            size: 8,
        },
        blue: ColorChannel {
            position: 0,
            size: 8,
        },
    };

    /// Bytes one pixel occupies.
    pub(crate) fn bytes_per_pixel(&self) -> u32 {
        u32::from(self.bits).div_ceil(8)
    }

    /// Place an 8-bit channel value into its field.
    fn place(value: u8, channel: ColorChannel) -> u32 {
        if channel.size == 0 {
            return 0;
        }
        // A narrower field takes the value's most significant bits and a wider
        // one shifts it up.  Scaling any other way leaves a 5-bit field unable
        // to reach full intensity, which shows as a display that cannot render
        // white.
        let scaled = if channel.size <= 8 {
            u32::from(value) >> (8 - channel.size)
        } else {
            u32::from(value) << (channel.size - 8)
        };
        scaled << channel.position
    }

    /// Encode a canonical `0x00RRGGBB` colour in this layout.
    pub(crate) fn encode(&self, color: u32) -> u32 {
        Self::place((color >> 16) as u8, self.red)
            | Self::place((color >> 8) as u8, self.green)
            | Self::place(color as u8, self.blue)
    }

    /// Store a canonical `0x00RRGGBB` colour as this layout's pixel bytes.
    ///
    /// `dst` must have room for [`Self::bytes_per_pixel`] bytes, and the
    /// return value is how many of them hold the pixel.  The whole point of
    /// this method is that the truncation happens once: a caller which encoded
    /// to a `u32` and wrote all four bytes would walk a 16-bit surface at
    /// twice its stride, which paints a garbled screen rather than a visible
    /// error.
    ///
    /// Bytes come out little-endian, which is the byte order of every surface
    /// this kernel has and the order the bootloader's channel positions are
    /// stated in.
    pub(crate) fn encode_into(&self, color: u32, dst: &mut [u8]) -> usize {
        let width = (self.bytes_per_pixel() as usize).min(size_of::<u32>());
        if width == 0 || dst.len() < width {
            return 0;
        }
        let encoded = self.encode(color).to_le_bytes();
        dst[..width].copy_from_slice(&encoded[..width]);
        width
    }

    /// The bits the colour channels occupy, as a mask within one pixel.
    ///
    /// A channel which is empty, or which would reach past the end of the
    /// pixel, contributes nothing: a malformed description must not be able to
    /// mask in bits that are not part of the pixel it describes.
    fn color_mask(&self) -> u32 {
        let field = |channel: ColorChannel| {
            if channel.size == 0 || u16::from(channel.position) + u16::from(channel.size) > 32 {
                0
            } else {
                (u32::MAX >> (32 - channel.size)) << channel.position
            }
        };
        field(self.red) | field(self.green) | field(self.blue)
    }

    /// The bits no colour channel occupies, as a mask within one pixel.
    fn spare_mask(&self) -> u32 {
        let width = u32::from(self.bits).min(32);
        let pixel_mask = if width == 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        !self.color_mask() & pixel_mask
    }

    /// The single contiguous unused field, if the layout leaves exactly one.
    ///
    /// fbdev calls this field transparency.  A surface whose colour channels
    /// fill the pixel has no such field, and reporting one anyway would
    /// describe bits that do not exist.
    pub(crate) fn spare_field(&self) -> Option<ColorChannel> {
        // A layout which names no colour channel at all has no transparency
        // field either.  Every bit of it is unaccounted for, and reporting the
        // whole pixel as alpha would describe a channel that is not there.
        if self.color_mask() == 0 {
            return None;
        }
        let spare = self.spare_mask();
        if spare == 0 {
            return None;
        }
        let size = spare.count_ones();
        let position = spare.trailing_zeros();
        (size < 32 && (spare >> position) == (1u32 << size) - 1).then_some(ColorChannel {
            position: position as u8,
            size: size as u8,
        })
    }
}

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

#[cfg(test)]
mod tests {
    use super::{ColorChannel, PixelLayout};

    /// The 16-bit RGB565 layout a firmware may report for a 16-bit mode.
    const RGB565: PixelLayout = PixelLayout {
        bits: 16,
        red: ColorChannel {
            position: 11,
            size: 5,
        },
        green: ColorChannel {
            position: 5,
            size: 6,
        },
        blue: ColorChannel {
            position: 0,
            size: 5,
        },
    };

    /// A 24-bit layout with the channels in the opposite order to BGRA.
    const RGB888: PixelLayout = PixelLayout {
        bits: 24,
        red: ColorChannel {
            position: 0,
            size: 8,
        },
        green: ColorChannel {
            position: 8,
            size: 8,
        },
        blue: ColorChannel {
            position: 16,
            size: 8,
        },
    };

    #[test]
    fn virtio_gpu_layout_encodes_canonical_colours_unchanged() {
        // The DRM scanout is B8G8R8A8, whose native little-endian word is
        // exactly the canonical 0x00RRGGBB value, so the console's colours
        // must pass through untouched.
        for color in [0x0000_0000, 0x00d0_d0d0, 0x00ff_0000, 0x0000_00ff] {
            assert_eq!(PixelLayout::B8G8R8A8.encode(color), color);
        }
        assert_eq!(PixelLayout::B8G8R8A8.bytes_per_pixel(), 4);
    }

    #[test]
    fn narrow_fields_take_the_most_significant_bits() {
        // A 5-bit field must be able to reach all-ones, or the display cannot
        // render a fully saturated channel.  Truncating instead of taking the
        // top bits would cap a full-intensity blue at 0b11111 only by luck.
        assert_eq!(RGB565.encode(0x00ff_ffff), 0xffff);
        assert_eq!(RGB565.encode(0x00ff_0000), 0xf800);
        assert_eq!(RGB565.encode(0x0000_ff00), 0x07e0);
        assert_eq!(RGB565.encode(0x0000_00ff), 0x001f);
        assert_eq!(RGB565.encode(0), 0);
        assert_eq!(RGB565.bytes_per_pixel(), 2);
    }

    #[test]
    fn channel_order_follows_the_reported_layout() {
        // Two surfaces can both be 24 bits deep and disagree completely about
        // where red lives; the encoder has to follow the layout, not a guess.
        assert_eq!(RGB888.encode(0x00ff_0000), 0x0000_ff);
        assert_eq!(RGB888.encode(0x0000_00ff), 0xff_0000);
        assert_eq!(RGB888.bytes_per_pixel(), 3);
    }

    #[test]
    fn spare_field_is_reported_only_when_one_exists() {
        // B8G8R8A8 leaves exactly the top byte uncovered.
        assert_eq!(
            PixelLayout::B8G8R8A8.spare_field(),
            Some(ColorChannel {
                position: 24,
                size: 8
            })
        );
        // RGB565 fills its 16 bits, so claiming a transparency field would
        // describe bits that do not exist.
        assert_eq!(RGB565.spare_field(), None);
        assert_eq!(RGB888.spare_field(), None);
    }

    #[test]
    fn a_pixel_occupies_exactly_its_own_bytes() {
        // The surface must receive its own depth and nothing more.  Writing
        // four bytes for a 16-bit pixel is the failure this pins down: it both
        // overruns the last pixel of the surface and shears every scan line.
        let mut pixel = [0xaau8; 4];
        assert_eq!(RGB565.encode_into(0x00ff_ffff, &mut pixel), 2);
        assert_eq!(pixel, [0xff, 0xff, 0xaa, 0xaa]);
        assert_eq!(PixelLayout::B8G8R8A8.encode_into(0x0012_3456, &mut pixel), 4);
        assert_eq!(pixel, [0x56, 0x34, 0x12, 0x00]);
        // This 24-bit layout keeps red in the low bits, so its bytes come out
        // in the opposite order to B8G8R8A8's -- which is the whole reason the
        // layout is consulted rather than assumed.
        assert_eq!(RGB888.encode_into(0x0012_3456, &mut pixel), 3);
        assert_eq!(&pixel[..3], &[0x12, 0x34, 0x56]);
    }

    #[test]
    fn a_pixel_is_not_written_into_a_slice_too_small_for_it() {
        // Handing back a short count is the only safe answer: writing what
        // fits would put half a pixel on the surface and leave the rest of the
        // old one beside it.
        let mut pixel = [0xaau8; 2];
        assert_eq!(PixelLayout::B8G8R8A8.encode_into(0x00ff_ffff, &mut pixel), 0);
        assert_eq!(pixel, [0xaa, 0xaa]);
    }

    #[test]
    fn zero_width_channels_are_ignored_rather_than_shifting_out_of_range() {
        let layout = PixelLayout {
            bits: 16,
            red: ColorChannel {
                position: 0,
                size: 0,
            },
            green: ColorChannel {
                position: 0,
                size: 0,
            },
            blue: ColorChannel {
                position: 0,
                size: 0,
            },
        };
        assert_eq!(layout.encode(0x00ff_ffff), 0);
        assert_eq!(layout.spare_field(), None);
    }
}
