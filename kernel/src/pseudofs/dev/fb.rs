use alloc::vec::Vec;
use core::{
    any::Any,
    mem::{align_of, offset_of, size_of},
};

use axerrno::{AxError, LinuxError};
use axfs_ng_vfs::{DeviceId, NodeFlags, VfsError, VfsResult};

use super::console_fb::DisplayCore;
use crate::{
    file::IoctlContext,
    pseudofs::{
        DeviceMmap, DeviceOps,
        dev::scanout::ColorChannel,
        device_registry::{
            DeviceHandle, DeviceIdentity, DeviceRegistration, MAX_DEVICES, global_device_registry,
        },
    },
};

pub(crate) const FB_DEVICE_ID: DeviceId = DeviceId::new(29, 0);

// Types from https://github.com/Tangzh33/asterinas

#[repr(C)]
#[derive(Default, Debug, Clone, Copy)]
pub struct FrameBufferBitfield {
    /// The beginning of bitfield.
    offset: u32,
    /// The length of bitfield.
    length: u32,
    /// Most significant bit is right(!= 0).
    msb_right: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct VarScreenInfo {
    pub xres: u32, // Visible resolution
    pub yres: u32,
    pub xres_virtual: u32, // Virtual resolution
    pub yres_virtual: u32,
    pub xoffset: u32, // Offset from virtual to visible
    pub yoffset: u32,
    pub bits_per_pixel: u32, // Guess what
    pub grayscale: u32,      // 0 = color, 1 = grayscale, >1 = FOURCC
    // Add other fields as needed
    pub red: FrameBufferBitfield, // Bitfield in framebuffer memory if true color
    pub green: FrameBufferBitfield, // Else only length is significant
    pub blue: FrameBufferBitfield,
    pub transp: FrameBufferBitfield, // Transparency
    pub nonstd: u32,                 // Non-standard pixel format
    pub activate: u32,               // See FB_ACTIVATE_*
    pub height: u32,                 // Height of picture in mm
    pub width: u32,                  // Width of picture in mm
    pub accel_flags: u32,            // (OBSOLETE) see fb_info.flags
    pub pixclock: u32,               // Pixel clock in ps (pico seconds)
    pub left_margin: u32,            // Time from sync to picture
    pub right_margin: u32,           // Time from picture to sync
    pub upper_margin: u32,           // Time from sync to picture
    pub lower_margin: u32,
    pub hsync_len: u32,     // Length of horizontal sync
    pub vsync_len: u32,     // Length of vertical sync
    pub sync: u32,          // See FB_SYNC_*
    pub vmode: u32,         // See FB_VMODE_*
    pub rotate: u32,        // Angle we rotate counter-clockwise
    pub colorspace: u32,    // Colorspace for FOURCC-based modes
    pub reserved: [u32; 4], // Reserved for future compatibility
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct FixScreenInfo {
    pub id: [u8; 16],       // Identification string, e.g., "TT Builtin"
    pub smem_start: u64,    // Start of framebuffer memory (physical address)
    pub smem_len: u32,      // Length of framebuffer memory
    pub type_: u32,         // See FB_TYPE_*
    pub type_aux: u32,      // Interleave for interleaved planes
    pub visual: u32,        // See FB_VISUAL_*
    pub xpanstep: u16,      // Zero if no hardware panning
    pub ypanstep: u16,      // Zero if no hardware panning
    pub ywrapstep: u16,     // Zero if no hardware ywrap
    pub line_length: u32,   // Length of a line in bytes
    pub mmio_start: u64,    // Start of Memory Mapped I/O (physical address)
    pub mmio_len: u32,      // Length of Memory Mapped I/O
    pub accel: u32,         // Indicate to driver which specific chip/card we have
    pub capabilities: u16,  // See FB_CAP_*
    pub reserved: [u16; 2], // Reserved for future compatibility
}

const _: () = {
    assert!(size_of::<FrameBufferBitfield>() == 12);
    assert!(align_of::<FrameBufferBitfield>() == 4);
    assert!(offset_of!(FrameBufferBitfield, offset) == 0);
    assert!(offset_of!(FrameBufferBitfield, length) == 4);
    assert!(offset_of!(FrameBufferBitfield, msb_right) == 8);
    assert!(size_of::<VarScreenInfo>() == 160);
    assert!(align_of::<VarScreenInfo>() == 4);
    assert!(size_of::<FixScreenInfo>() == 80);
    assert!(align_of::<FixScreenInfo>() == 8);
    assert!(offset_of!(FixScreenInfo, smem_start) == 16);
    assert!(offset_of!(FixScreenInfo, line_length) == 48);
    assert!(offset_of!(FixScreenInfo, mmio_start) == 56);
};

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..][..2].copy_from_slice(&value.to_ne_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..][..4].copy_from_slice(&value.to_ne_bytes());
}

fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..][..8].copy_from_slice(&value.to_ne_bytes());
}

fn put_bitfield(bytes: &mut [u8], offset: usize, value: FrameBufferBitfield) {
    put_u32(
        bytes,
        offset + offset_of!(FrameBufferBitfield, offset),
        value.offset,
    );
    put_u32(
        bytes,
        offset + offset_of!(FrameBufferBitfield, length),
        value.length,
    );
    put_u32(
        bytes,
        offset + offset_of!(FrameBufferBitfield, msb_right),
        value.msb_right,
    );
}

fn var_screen_info_to_user_bytes(value: VarScreenInfo) -> [u8; size_of::<VarScreenInfo>()] {
    let mut bytes = [0u8; size_of::<VarScreenInfo>()];
    for (offset, field) in [
        (offset_of!(VarScreenInfo, xres), value.xres),
        (offset_of!(VarScreenInfo, yres), value.yres),
        (offset_of!(VarScreenInfo, xres_virtual), value.xres_virtual),
        (offset_of!(VarScreenInfo, yres_virtual), value.yres_virtual),
        (offset_of!(VarScreenInfo, xoffset), value.xoffset),
        (offset_of!(VarScreenInfo, yoffset), value.yoffset),
        (
            offset_of!(VarScreenInfo, bits_per_pixel),
            value.bits_per_pixel,
        ),
        (offset_of!(VarScreenInfo, grayscale), value.grayscale),
        (offset_of!(VarScreenInfo, nonstd), value.nonstd),
        (offset_of!(VarScreenInfo, activate), value.activate),
        (offset_of!(VarScreenInfo, height), value.height),
        (offset_of!(VarScreenInfo, width), value.width),
        (offset_of!(VarScreenInfo, accel_flags), value.accel_flags),
        (offset_of!(VarScreenInfo, pixclock), value.pixclock),
        (offset_of!(VarScreenInfo, left_margin), value.left_margin),
        (offset_of!(VarScreenInfo, right_margin), value.right_margin),
        (offset_of!(VarScreenInfo, upper_margin), value.upper_margin),
        (offset_of!(VarScreenInfo, lower_margin), value.lower_margin),
        (offset_of!(VarScreenInfo, hsync_len), value.hsync_len),
        (offset_of!(VarScreenInfo, vsync_len), value.vsync_len),
        (offset_of!(VarScreenInfo, sync), value.sync),
        (offset_of!(VarScreenInfo, vmode), value.vmode),
        (offset_of!(VarScreenInfo, rotate), value.rotate),
        (offset_of!(VarScreenInfo, colorspace), value.colorspace),
    ] {
        put_u32(&mut bytes, offset, field);
    }
    for (index, field) in value.reserved.into_iter().enumerate() {
        put_u32(
            &mut bytes,
            offset_of!(VarScreenInfo, reserved) + index * size_of::<u32>(),
            field,
        );
    }
    put_bitfield(&mut bytes, offset_of!(VarScreenInfo, red), value.red);
    put_bitfield(&mut bytes, offset_of!(VarScreenInfo, green), value.green);
    put_bitfield(&mut bytes, offset_of!(VarScreenInfo, blue), value.blue);
    put_bitfield(&mut bytes, offset_of!(VarScreenInfo, transp), value.transp);
    bytes
}

fn fix_screen_info_to_user_bytes(value: FixScreenInfo) -> [u8; size_of::<FixScreenInfo>()] {
    let mut bytes = [0u8; size_of::<FixScreenInfo>()];
    bytes[offset_of!(FixScreenInfo, id)..][..value.id.len()].copy_from_slice(&value.id);
    put_u64(
        &mut bytes,
        offset_of!(FixScreenInfo, smem_start),
        value.smem_start,
    );
    put_u32(
        &mut bytes,
        offset_of!(FixScreenInfo, smem_len),
        value.smem_len,
    );
    put_u32(&mut bytes, offset_of!(FixScreenInfo, type_), value.type_);
    put_u32(
        &mut bytes,
        offset_of!(FixScreenInfo, type_aux),
        value.type_aux,
    );
    put_u32(&mut bytes, offset_of!(FixScreenInfo, visual), value.visual);
    put_u16(
        &mut bytes,
        offset_of!(FixScreenInfo, xpanstep),
        value.xpanstep,
    );
    put_u16(
        &mut bytes,
        offset_of!(FixScreenInfo, ypanstep),
        value.ypanstep,
    );
    put_u16(
        &mut bytes,
        offset_of!(FixScreenInfo, ywrapstep),
        value.ywrapstep,
    );
    put_u32(
        &mut bytes,
        offset_of!(FixScreenInfo, line_length),
        value.line_length,
    );
    put_u64(
        &mut bytes,
        offset_of!(FixScreenInfo, mmio_start),
        value.mmio_start,
    );
    put_u32(
        &mut bytes,
        offset_of!(FixScreenInfo, mmio_len),
        value.mmio_len,
    );
    put_u32(&mut bytes, offset_of!(FixScreenInfo, accel), value.accel);
    put_u16(
        &mut bytes,
        offset_of!(FixScreenInfo, capabilities),
        value.capabilities,
    );
    for (index, field) in value.reserved.into_iter().enumerate() {
        put_u16(
            &mut bytes,
            offset_of!(FixScreenInfo, reserved) + index * size_of::<u16>(),
            field,
        );
    }
    bytes
}

/// Read a `u32` field from the byte image of a `#[repr(C)]` structure.
///
/// Both the image's length and the field's offset are const generics of this
/// function, so the assertion below is evaluated for the exact pair of values
/// each call site passes: an offset that does not name a whole `u32` inside the
/// image fails the build instead of panicking on a path a process reaches
/// through an ioctl.
fn get_u32<const LEN: usize, const OFFSET: usize>(bytes: &[u8; LEN]) -> u32 {
    const { assert!(OFFSET + size_of::<u32>() <= LEN, "outside image") };
    u32::from_ne_bytes(bytes[OFFSET..OFFSET + size_of::<u32>()].try_into().unwrap())
}

/// Read a `u64` field from the byte image of a `#[repr(C)]` structure.
///
/// As [`get_u32`], including the bound the build checks.
fn get_u64<const LEN: usize, const OFFSET: usize>(bytes: &[u8; LEN]) -> u64 {
    const { assert!(OFFSET + size_of::<u64>() <= LEN, "outside image") };
    u64::from_ne_bytes(bytes[OFFSET..OFFSET + size_of::<u64>()].try_into().unwrap())
}

/// Linux `struct fb_cmap` on x86_64.  Truecolor devices use this only for
/// their 16-entry software pseudo-palette; it is never advertised as a CLUT.
#[repr(C)]
#[derive(Clone, Copy)]
struct ColorMap {
    start: u32,
    len: u32,
    red: u64,
    green: u64,
    blue: u64,
    transp: u64,
}

const _: () = {
    assert!(size_of::<ColorMap>() == 40);
    assert!(align_of::<ColorMap>() == 8);
    assert!(offset_of!(ColorMap, start) == 0);
    assert!(offset_of!(ColorMap, red) == 8);
};

fn color_map_from_user(context: &IoctlContext, arg: usize) -> VfsResult<ColorMap> {
    let mut bytes = [0u8; size_of::<ColorMap>()];
    context
        .user_memory()
        .read_into(arg as *const u8, &mut bytes)
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(ColorMap {
        start: get_u32::<_, { offset_of!(ColorMap, start) }>(&bytes),
        len: get_u32::<_, { offset_of!(ColorMap, len) }>(&bytes),
        red: get_u64::<_, { offset_of!(ColorMap, red) }>(&bytes),
        green: get_u64::<_, { offset_of!(ColorMap, green) }>(&bytes),
        blue: get_u64::<_, { offset_of!(ColorMap, blue) }>(&bytes),
        transp: get_u64::<_, { offset_of!(ColorMap, transp) }>(&bytes),
    })
}

fn pseudo_palette_range(cmap: ColorMap) -> VfsResult<core::ops::Range<usize>> {
    let start = usize::try_from(cmap.start).map_err(|_| AxError::InvalidInput)?;
    let len = usize::try_from(cmap.len).map_err(|_| AxError::InvalidInput)?;
    let end = start.checked_add(len).ok_or(AxError::InvalidInput)?;
    if end > 16 || (len != 0 && (cmap.red == 0 || cmap.green == 0 || cmap.blue == 0)) {
        return Err(AxError::InvalidInput);
    }
    Ok(start..end)
}

fn read_cmap_channel(context: &IoctlContext, pointer: u64, len: usize) -> VfsResult<Vec<u16>> {
    if pointer == 0 {
        return Err(AxError::InvalidInput);
    }
    let address = usize::try_from(pointer).map_err(|_| AxError::InvalidInput)?;
    let bytes = len.checked_mul(2).ok_or(AxError::InvalidInput)?;
    let mut raw = [0u8; 32];
    context
        .user_memory()
        .read_into(address as *const u8, &mut raw[..bytes])
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(raw[..bytes]
        .chunks_exact(2)
        .map(|value| u16::from_ne_bytes([value[0], value[1]]))
        .collect())
}

fn write_cmap_channel(
    context: &IoctlContext,
    pointer: u64,
    values: impl IntoIterator<Item = u16>,
) -> VfsResult<()> {
    if pointer == 0 {
        return Ok(());
    }
    let address = usize::try_from(pointer).map_err(|_| AxError::InvalidInput)?;
    let mut raw = Vec::new();
    for value in values {
        raw.try_reserve(2).map_err(|_| AxError::NoMemory)?;
        raw.extend_from_slice(&value.to_ne_bytes());
    }
    context
        .user_memory()
        .write_bytes(address, &raw)
        .map_err(crate::mm::map_usercopy_error)
}

const fn rgb8_to_u16(value: u8) -> u16 {
    (value as u16) * 257
}

pub(crate) const fn default_pseudo_palette() -> [u32; 16] {
    [
        0xff00_0000,
        0xffaa_0000,
        0xff00_aa00,
        0xffaa_5500,
        0xff00_00aa,
        0xffaa_00aa,
        0xff00_aaaa,
        0xffaa_aaaa,
        0xff55_5555,
        0xffff_5555,
        0xff55_ff55,
        0xffff_ff55,
        0xff55_55ff,
        0xffff_55ff,
        0xff55_ffff,
        0xffff_ffff,
    ]
}

fn set_pseudo_palette(core: &DisplayCore, context: &IoctlContext, arg: usize) -> VfsResult<()> {
    let cmap = color_map_from_user(context, arg)?;
    let range = pseudo_palette_range(cmap)?;
    let len = range.len();
    let red = read_cmap_channel(context, cmap.red, len)?;
    let green = read_cmap_channel(context, cmap.green, len)?;
    let blue = read_cmap_channel(context, cmap.blue, len)?;
    let transp = (cmap.transp != 0)
        .then(|| read_cmap_channel(context, cmap.transp, len))
        .transpose()?;
    let mut palette = core.pseudo_palette.lock();
    for (offset, index) in range.enumerate() {
        let alpha = transp
            .as_ref()
            .map_or(0xff, |values| (values[offset] >> 8) as u8);
        palette[index] = (u32::from(alpha) << 24)
            | (u32::from((red[offset] >> 8) as u8) << 16)
            | (u32::from((green[offset] >> 8) as u8) << 8)
            | u32::from((blue[offset] >> 8) as u8);
    }
    Ok(())
}

fn get_pseudo_palette(core: &DisplayCore, context: &IoctlContext, arg: usize) -> VfsResult<()> {
    let cmap = color_map_from_user(context, arg)?;
    let range = pseudo_palette_range(cmap)?;
    let palette = core.pseudo_palette.lock();
    let colors = range
        .clone()
        .map(|index| palette[index])
        .collect::<Vec<_>>();
    drop(palette);
    write_cmap_channel(
        context,
        cmap.red,
        colors.iter().map(|color| rgb8_to_u16((color >> 16) as u8)),
    )?;
    write_cmap_channel(
        context,
        cmap.green,
        colors.iter().map(|color| rgb8_to_u16((color >> 8) as u8)),
    )?;
    write_cmap_channel(
        context,
        cmap.blue,
        colors.iter().map(|color| rgb8_to_u16(*color as u8)),
    )?;
    write_cmap_channel(
        context,
        cmap.transp,
        colors
            .into_iter()
            .map(|color| rgb8_to_u16((color >> 24) as u8)),
    )
}

struct RequestedVarScreenInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    nonstd: u32,
    rotate: u32,
}

fn var_screen_info_from_user_bytes(
    bytes: &[u8; size_of::<VarScreenInfo>()],
) -> RequestedVarScreenInfo {
    RequestedVarScreenInfo {
        xres: get_u32::<_, { offset_of!(VarScreenInfo, xres) }>(bytes),
        yres: get_u32::<_, { offset_of!(VarScreenInfo, yres) }>(bytes),
        xres_virtual: get_u32::<_, { offset_of!(VarScreenInfo, xres_virtual) }>(bytes),
        yres_virtual: get_u32::<_, { offset_of!(VarScreenInfo, yres_virtual) }>(bytes),
        xoffset: get_u32::<_, { offset_of!(VarScreenInfo, xoffset) }>(bytes),
        yoffset: get_u32::<_, { offset_of!(VarScreenInfo, yoffset) }>(bytes),
        bits_per_pixel: get_u32::<_, { offset_of!(VarScreenInfo, bits_per_pixel) }>(bytes),
        grayscale: get_u32::<_, { offset_of!(VarScreenInfo, grayscale) }>(bytes),
        nonstd: get_u32::<_, { offset_of!(VarScreenInfo, nonstd) }>(bytes),
        rotate: get_u32::<_, { offset_of!(VarScreenInfo, rotate) }>(bytes),
    }
}

/// Translate one surface channel into the fbdev bitfield form.
fn color_bitfield(channel: ColorChannel) -> FrameBufferBitfield {
    FrameBufferBitfield {
        offset: u32::from(channel.position),
        length: u32::from(channel.size),
        msb_right: 0,
    }
}

pub(crate) fn current_var_screen_info(core: &DisplayCore) -> VarScreenInfo {
    let layout = core.scanout.pixel_layout();
    VarScreenInfo {
        xres: core.scanout.width(),
        yres: core.scanout.height(),
        xres_virtual: core.scanout.width(),
        yres_virtual: core.scanout.virtual_height(),
        xoffset: 0,
        yoffset: core.scanout.yoffset(),
        bits_per_pixel: u32::from(layout.bits),
        grayscale: 0,
        // Report the surface's own layout rather than a fixed one.  The
        // virtio-gpu scanout is B8G8R8A8_UNORM and its little-endian word is
        // the canonical colour value, but a firmware framebuffer is whatever
        // its GOP mode was; describing a 16-bit mode as 32-bit BGRA would make
        // every userspace client render noise.
        red: color_bitfield(layout.red),
        green: color_bitfield(layout.green),
        blue: color_bitfield(layout.blue),
        // fbdev calls the unoccupied field transparency.  A surface whose
        // colour channels fill the pixel has none, and reporting one anyway
        // would describe bits that do not exist.
        transp: match layout.spare_field() {
            Some(channel) => color_bitfield(channel),
            None => FrameBufferBitfield {
                offset: 0,
                length: 0,
                msb_right: 0,
            },
        },
        nonstd: 0,
        activate: 0,
        height: 0,
        width: 0,
        accel_flags: 0,
        // No EDID/timing source is available, so do not fabricate timings.
        pixclock: 0,
        left_margin: 0,
        right_margin: 0,
        upper_margin: 0,
        lower_margin: 0,
        hsync_len: 0,
        vsync_len: 0,
        sync: 0,
        vmode: 0,
        rotate: 0,
        colorspace: 0,
        reserved: [0; 4],
    }
}

fn fixed_mode_matches(request: RequestedVarScreenInfo, current: VarScreenInfo) -> bool {
    request.xres == current.xres
        && request.yres == current.yres
        && request.xres_virtual == current.xres_virtual
        && request.yres_virtual == current.yres_virtual
        && request.xoffset == current.xoffset
        && request.yoffset == current.yoffset
        && request.bits_per_pixel == current.bits_per_pixel
        && request.grayscale == 0
        && request.nonstd == 0
        && request.rotate == 0
}

fn pan_mode_matches(request: &RequestedVarScreenInfo, current: VarScreenInfo) -> bool {
    request.xres == current.xres
        && request.yres == current.yres
        && request.xres_virtual == current.xres_virtual
        && request.yres_virtual == current.yres_virtual
        && request.bits_per_pixel == current.bits_per_pixel
        && request.grayscale == 0
        && request.nonstd == 0
        && request.rotate == 0
}

fn read_var_screen_info(context: &IoctlContext, arg: usize) -> VfsResult<RequestedVarScreenInfo> {
    // `read_bytes` deliberately accepts uninitialized destinations, but this
    // fixed-size ioctl buffer need not rely on that: every byte begins valid
    // and the user copy either replaces all 160 bytes or returns an error.
    let mut raw = [core::mem::MaybeUninit::new(0u8); size_of::<VarScreenInfo>()];
    context
        .user_memory()
        .read_bytes(arg, &mut raw)
        .map_err(crate::mm::map_usercopy_error)?;
    // SAFETY: `raw` was initialized with zero bytes before the copy, so all
    // elements are initialized whether or not the user copy changed them.
    let bytes = raw.map(|byte| unsafe { byte.assume_init() });
    Ok(var_screen_info_from_user_bytes(&bytes))
}


pub struct FrameBuffer {
    core: alloc::sync::Arc<DisplayCore>,
    registry_handle: DeviceHandle<'static, MAX_DEVICES>,
}

fn fb_sysfs_registration() -> VfsResult<alloc::sync::Arc<DeviceRegistration>> {
    DeviceRegistration::try_new(
        DeviceIdentity::new(
            "virtual".into(),
            "graphics".into(),
            "fb0".into(),
            FB_DEVICE_ID,
        )?,
        "graphics".into(),
        Vec::new(),
        None,
    )
}

impl FrameBuffer {
    /// Publish the legacy `/dev/fb0` node over an installed console.
    ///
    /// The console owns the surface, its damage and its refresh worker; this
    /// node only adds the fbdev ABI and the sysfs identity.
    pub fn try_new(core: alloc::sync::Arc<DisplayCore>) -> Result<Self, AxError> {
        let registration = fb_sysfs_registration()?;
        let reservation = global_device_registry().reserve(registration.identity().clone())?;
        // The registry commit atomically exposes the device through its
        // class, canonical /sys/devices path, and /sys/dev/char link.  The
        // devfs node owns this handle and removes all three together.
        let registry_handle = reservation.publish(registration)?;
        Ok(Self {
            core,
            registry_handle,
        })
    }
}

impl Drop for FrameBuffer {
    fn drop(&mut self) {
        if let Err(error) = self.registry_handle.remove() {
            warn!("Framebuffer sysfs removal failed: {error}");
        }
    }
}
impl DeviceOps for FrameBuffer {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let offset = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.core.size);
        let len = buf.len().min(self.core.size.saturating_sub(offset));
        self.core.read_bytes(offset, &mut buf[..len])?;
        Ok(len)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if offset >= self.core.size as u64 {
            return Err(VfsError::StorageFull);
        }
        let offset = offset as usize;
        let len = buf.len().min(self.core.size - offset);
        self.core.write_bytes(offset, &buf[..len])?;
        self.core.mark_write(offset, len);
        Ok(len)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            // FBIOGET_VSCREENINFO
            0x4600 => {
                let value = current_var_screen_info(&self.core);
                let bytes = var_screen_info_to_user_bytes(value);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            // FBIOPUT_VSCREENINFO: the only supported mode is the current
            // fixed scanout mode.  Linux fbdev drivers return the applied var.
            0x4601 => {
                let request = read_var_screen_info(context, arg)?;
                let current = current_var_screen_info(&self.core);
                if !fixed_mode_matches(request, current) {
                    return Err(AxError::InvalidInput);
                }
                let bytes = var_screen_info_to_user_bytes(current);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            // FBIOGET_FSCREENINFO
            0x4602 => {
                let value = FixScreenInfo {
                    id: *b"DRM fbdev\0\0\0\0\0\0\0",
                    // This is an SG GEM allocation, so no fictitious linear
                    // physical base is reported. mmap maps its SharedPages.
                    smem_start: 0,
                    smem_len: self.core.size as u32,
                    type_: 0,
                    type_aux: 0,
                    visual: 2, // FB_VISUAL_TRUECOLOR
                    xpanstep: 0,
                    ypanstep: 1,
                    ywrapstep: 0,
                    line_length: self.core.scanout.pitch(),
                    mmio_start: 0,
                    mmio_len: 0,
                    accel: 0,
                    capabilities: 0,
                    reserved: [0; 2],
                };
                let bytes = fix_screen_info_to_user_bytes(value);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            // FBIOGET/PUTCMAP address the Linux truecolor pseudo-palette.
            // They do not advertise a programmable hardware CLUT.
            0x4604 => get_pseudo_palette(&self.core, context, arg).map(|_| 0),
            0x4605 => set_pseudo_palette(&self.core, context, arg).map(|_| 0),
            // FBIOPAN_DISPLAY translates to the atomic primary-plane source
            // offset; page zero is not a special raw-display path.
            0x4606 => {
                let request = read_var_screen_info(context, arg)?;
                let current = current_var_screen_info(&self.core);
                if !pan_mode_matches(&request, current) || request.xoffset != 0 {
                    return Err(AxError::InvalidInput);
                }
                self.core.scanout.pan(request.yoffset).map(|_| 0)
            }
            // FBIOBLANK maps to connector DPMS in the same atomic state
            // machine as ordinary KMS clients.
            0x4611 if arg <= 4 => self.core.scanout.set_blank(arg != 0).map(|_| 0),
            0x4611 => Err(LinuxError::EOPNOTSUPP.into()),
            _ => Err(AxError::NotATty),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mmap(&self) -> DeviceMmap {
        // mmap is deliberately passive: raw writers publish through fsync or
        // FBIOPAN_DISPLAY. The mapping is whatever the surface's own backing
        // requires.
        self.core.scanout.mmap()
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        // fsync is the explicit publication point for raw mmap writes.
        self.core.mark_full();
        self.core.commit_if_needed();
        Ok(())
    }

    fn len(&self) -> VfsResult<u64> {
        Ok(self.core.size as u64)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

#[cfg(test)]
mod tests {
    use super::{FB_DEVICE_ID, fb_sysfs_registration};

    #[test]
    fn framebuffer_sysfs_registration_matches_the_fbdev_uevent_identity() {
        let registration = fb_sysfs_registration().unwrap();
        let identity = registration.identity();
        assert_eq!(identity.bus, "virtual");
        assert_eq!(identity.class, "graphics");
        assert_eq!(identity.name, "fb0");
        assert_eq!(identity.device_id, Some(FB_DEVICE_ID));
        assert_eq!(identity.devname.as_deref(), Some("fb0"));
        assert_eq!(
            registration.uevent_payload(),
            concat!(
                "MAJOR=29\n",
                "MINOR=0\n",
                "DEVNAME=fb0\n",
                "DEVPATH=/devices/virtual/fb0\n",
                "SUBSYSTEM=graphics\n",
                "DEVTYPE=graphics\n",
            )
        );
    }
}
