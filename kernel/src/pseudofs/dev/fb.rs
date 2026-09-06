use alloc::vec::Vec;
use core::{
    any::Any,
    mem::{align_of, offset_of, size_of},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{DeviceId, NodeFlags, VfsError, VfsResult};
use axhal::mem::phys_to_virt;
use axsync::Mutex;
use axtask::WaitQueue;
use kspin::SpinNoIrq;
use lazy_static::lazy_static;
use memory_addr::{PAGE_SIZE_4K, PhysAddr};

use crate::{
    drm::DrmFbdev,
    file::IoctlContext,
    pseudofs::{
        DeviceMmap, DeviceOps,
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
struct VarScreenInfo {
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

fn get_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(bytes[offset..offset + size_of::<u32>()].try_into().unwrap())
}

fn get_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes(bytes[offset..offset + size_of::<u64>()].try_into().unwrap())
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
    let mut raw = [core::mem::MaybeUninit::new(0u8); size_of::<ColorMap>()];
    context
        .user_memory()
        .read_bytes(arg, &mut raw)
        .map_err(crate::mm::map_usercopy_error)?;
    let bytes = raw.map(|byte| unsafe { byte.assume_init() });
    Ok(ColorMap {
        start: get_u32(&bytes, offset_of!(ColorMap, start)),
        len: get_u32(&bytes, offset_of!(ColorMap, len)),
        red: get_u64(&bytes, offset_of!(ColorMap, red)),
        green: get_u64(&bytes, offset_of!(ColorMap, green)),
        blue: get_u64(&bytes, offset_of!(ColorMap, blue)),
        transp: get_u64(&bytes, offset_of!(ColorMap, transp)),
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
    let mut raw = [core::mem::MaybeUninit::new(0u8); 32];
    context
        .user_memory()
        .read_bytes(address, &mut raw[..bytes])
        .map_err(crate::mm::map_usercopy_error)?;
    Ok(raw[..bytes]
        .chunks_exact(2)
        .map(|value| unsafe {
            u16::from_ne_bytes([value[0].assume_init(), value[1].assume_init()])
        })
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

const fn default_pseudo_palette() -> [u32; 16] {
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
        xres: get_u32(bytes, offset_of!(VarScreenInfo, xres)),
        yres: get_u32(bytes, offset_of!(VarScreenInfo, yres)),
        xres_virtual: get_u32(bytes, offset_of!(VarScreenInfo, xres_virtual)),
        yres_virtual: get_u32(bytes, offset_of!(VarScreenInfo, yres_virtual)),
        xoffset: get_u32(bytes, offset_of!(VarScreenInfo, xoffset)),
        yoffset: get_u32(bytes, offset_of!(VarScreenInfo, yoffset)),
        bits_per_pixel: get_u32(bytes, offset_of!(VarScreenInfo, bits_per_pixel)),
        grayscale: get_u32(bytes, offset_of!(VarScreenInfo, grayscale)),
        nonstd: get_u32(bytes, offset_of!(VarScreenInfo, nonstd)),
        rotate: get_u32(bytes, offset_of!(VarScreenInfo, rotate)),
    }
}

fn current_var_screen_info(core: &DisplayCore) -> VarScreenInfo {
    let mode = core.scanout.mode();
    VarScreenInfo {
        xres: mode.width,
        yres: mode.height,
        xres_virtual: mode.width,
        yres_virtual: core.scanout.virtual_height(),
        xoffset: 0,
        yoffset: core.scanout.yoffset(),
        bits_per_pixel: 32,
        grayscale: 0,
        // virtio-gpu allocates B8G8R8A8_UNORM scanout; on x86_64 its bytes
        // are B, G, R, A, which is this native-endian 32-bit layout.
        red: FrameBufferBitfield {
            offset: 16,
            length: 8,
            msb_right: 0,
        },
        green: FrameBufferBitfield {
            offset: 8,
            length: 8,
            msb_right: 0,
        },
        blue: FrameBufferBitfield {
            offset: 0,
            length: 8,
            msb_right: 0,
        },
        transp: FrameBufferBitfield {
            offset: 24,
            length: 8,
            msb_right: 0,
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

const DAMAGE_CLEAN: u64 = 0;
const DAMAGE_FULL: u64 = u64::MAX;

/// A single, bounded damage record.  This deliberately coalesces all writes
/// between commits instead of allocating an unbounded list of rectangles.
struct DamageTracker {
    state: AtomicU64,
}

impl DamageTracker {
    const fn new() -> Self {
        Self {
            state: AtomicU64::new(DAMAGE_CLEAN),
        }
    }

    fn mark(&self, start: usize, len: usize, size: usize) {
        if len == 0 || start >= size {
            return;
        }
        let end = start.saturating_add(len).min(size);
        // fb_fix_screeninfo.smem_len is u32, so this representation covers
        // every fbdev mapping we advertise.
        if end > u32::MAX as usize {
            self.state.store(DAMAGE_FULL, Ordering::Release);
            return;
        }
        let update = ((start as u64) << 32) | end as u64;
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let merged = match current {
                DAMAGE_CLEAN => update,
                DAMAGE_FULL => return,
                value => {
                    let old_start = (value >> 32) as usize;
                    let old_end = value as u32 as usize;
                    ((old_start.min(start) as u64) << 32) | old_end.max(end) as u64
                }
            };
            match self.state.compare_exchange_weak(
                current,
                merged,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(value) => current = value,
            }
        }
    }

    fn mark_full(&self) {
        self.state.store(DAMAGE_FULL, Ordering::Release);
    }

    fn take(&self) -> u64 {
        self.state.swap(DAMAGE_CLEAN, Ordering::AcqRel)
    }
}

/// The sole fbdev-side scanout authority. Its backing is a DRM dumb GEM
/// object; fbdev and fbcon do not retain a raw-display ownership path.
struct DisplayCore {
    scanout: alloc::sync::Arc<DrmFbdev>,
    size: usize,
    /// Linux truecolor pseudo-palette, used by fbcon-style 0..15 pixel
    /// values. It is software state and never a hardware CLUT claim.
    pseudo_palette: Mutex<[u32; 16]>,
    damage: DamageTracker,
    /// Set after a writer publishes damage. The refresh task clears it before
    /// taking damage, so a concurrent writer cannot lose its wakeup.
    pending: AtomicBool,
    /// fbdev owns master only while the active VT is in KD_TEXT.  Damage may
    /// accumulate while false, but no refresh task may submit presentation.
    refresh_enabled: AtomicBool,
    /// Kept separate from `DisplayCore` in the worker: the worker holds only
    /// this queue plus a Weak, never a strong reference while idle.
    refresh: alloc::sync::Arc<WaitQueue>,
    /// Serializes driver flushes with explicit fsync/FBIOPAN publication.
    commit: Mutex<()>,
}

lazy_static! {
    // fbcon has no ownership of fbdev.  The weak handle lets its rendering
    // path disappear together with /dev/fb0 and its refresh worker.
    static ref FBCON_DISPLAY: SpinNoIrq<Option<alloc::sync::Weak<DisplayCore>>> =
        SpinNoIrq::new(None);
}

/// A short-lived, exclusive-in-practice view used only by the in-kernel
/// framebuffer console. fbdev users still own the ABI surface; fbcon merely
/// repaints before KD_TEXT scanout commits.
pub(crate) struct FbconFrame {
    scanout: alloc::sync::Arc<DrmFbdev>,
    size: usize,
    width: usize,
    height: usize,
    pitch: usize,
}

impl FbconFrame {
    pub(crate) fn clear(&self, color: u32) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.write_pixel(x, y, color);
            }
        }
    }

    pub(crate) fn glyph(&self, x: usize, y: usize, byte: u8) {
        for dy in 0..16 {
            let bits = glyph_row(byte, dy / 2);
            for dx in 0..8 {
                let px = x + dx;
                let py = y + dy;
                if px >= self.width || py >= self.height {
                    continue;
                }
                let color = if bits & (0x80 >> dx) != 0 {
                    0x00d0_d0d0
                } else {
                    0
                };
                if py
                    .checked_mul(self.pitch)
                    .and_then(|row| row.checked_add(px.saturating_mul(4)))
                    .is_some_and(|offset| offset + 4 <= self.size)
                {
                    self.write_pixel(px, py, color);
                }
            }
        }
    }

    fn write_pixel(&self, x: usize, y: usize, color: u32) {
        let Some(offset) = y
            .checked_mul(self.pitch)
            .and_then(|row| row.checked_add(x.checked_mul(4)?))
        else {
            return;
        };
        if offset + 4 > self.size {
            return;
        }
        let pages = self.scanout.pages();
        let Ok(page) = pages.paddr_at(offset / PAGE_SIZE_4K) else {
            return;
        };
        let address = PhysAddr::from(page.as_usize() + offset % PAGE_SIZE_4K);
        // Pixels are four-byte aligned, so they cannot straddle a 4 KiB
        // page boundary. The direct map is only used by in-kernel fbcon.
        unsafe {
            phys_to_virt(address)
                .as_mut_ptr()
                .cast::<u32>()
                .write_volatile(color)
        };
    }
}

// A compact built-in 5x7 ASCII subset, doubled vertically by `glyph`.  It
// covers normal kernel/login text without a font cache or any allocation.
fn glyph_row(byte: u8, row: usize) -> u8 {
    const GLYPHS: [[u8; 7]; 36] = [
        [14, 17, 17, 31, 17, 17, 17],
        [30, 17, 17, 30, 17, 17, 30],
        [15, 16, 16, 16, 16, 16, 15],
        [30, 17, 17, 17, 17, 17, 30],
        [31, 16, 16, 30, 16, 16, 31],
        [31, 16, 16, 30, 16, 16, 16],
        [15, 16, 16, 23, 17, 17, 15],
        [17, 17, 17, 31, 17, 17, 17],
        [31, 4, 4, 4, 4, 4, 31],
        [1, 1, 1, 1, 17, 17, 14],
        [17, 18, 20, 24, 20, 18, 17],
        [16, 16, 16, 16, 16, 16, 31],
        [17, 27, 21, 21, 17, 17, 17],
        [17, 25, 21, 19, 17, 17, 17],
        [14, 17, 17, 17, 17, 17, 14],
        [30, 17, 17, 30, 16, 16, 16],
        [14, 17, 17, 17, 21, 18, 13],
        [30, 17, 17, 30, 20, 18, 17],
        [15, 16, 16, 14, 1, 1, 30],
        [31, 4, 4, 4, 4, 4, 4],
        [17, 17, 17, 17, 17, 17, 14],
        [17, 17, 17, 17, 17, 10, 4],
        [17, 17, 17, 21, 21, 21, 10],
        [17, 17, 10, 4, 10, 17, 17],
        [17, 17, 10, 4, 4, 4, 4],
        [31, 1, 2, 4, 8, 16, 31],
        [14, 17, 19, 21, 25, 17, 14],
        [4, 12, 4, 4, 4, 4, 14],
        [14, 17, 1, 2, 4, 8, 31],
        [30, 1, 1, 14, 1, 1, 30],
        [2, 6, 10, 18, 31, 2, 2],
        [31, 16, 30, 1, 1, 17, 14],
        [6, 8, 16, 30, 17, 17, 14],
        [31, 1, 2, 4, 8, 8, 8],
        [14, 17, 17, 14, 17, 17, 14],
        [14, 17, 17, 15, 1, 2, 12],
    ];
    if row >= 7 {
        return 0;
    }
    let upper = byte.to_ascii_uppercase();
    let index = match upper {
        b'A'..=b'Z' => (upper - b'A') as usize,
        b'0'..=b'9' => 26 + (upper - b'0') as usize,
        _ => {
            return matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'[' | b']')
                .then_some(if row == 6 { 0x3e } else { 0 })
                .unwrap_or(0);
        }
    };
    GLYPHS[index][row] << 1
}

pub(crate) fn fbcon_dimensions() -> Option<(usize, usize)> {
    let display = FBCON_DISPLAY.lock().as_ref()?.upgrade()?;
    let mode = display.scanout.mode();
    let dimensions = (mode.width as usize, mode.height as usize);
    drop(display);
    Some(dimensions)
}

/// Runs a bounded synchronous fbcon repaint and marks the full scanout dirty.
/// No fbcon state lock is held by this module, so this cannot invert TTY/VT
/// locking with refresh or user-memory paths.
pub(crate) fn fbcon_draw(draw: impl FnOnce(&FbconFrame)) {
    let Some(display) = FBCON_DISPLAY
        .lock()
        .as_ref()
        .and_then(alloc::sync::Weak::upgrade)
    else {
        return;
    };
    let mode = display.scanout.mode();
    draw(&FbconFrame {
        scanout: display.scanout.clone(),
        size: display.size,
        width: mode.width as usize,
        height: mode.height as usize,
        pitch: display.scanout.pitch() as usize,
    });
    display.mark_full();
}

/// Called by the VT gate after it has changed `KD_TEXT`/`KD_GRAPHICS`. This
/// keeps the sole DRM master synchronized with the scanout owner: fbcon owns
/// it in text mode, while a graphics client can acquire it in graphics mode.
pub(crate) fn vt_graphics_changed(graphics: bool) {
    let Some(display) = FBCON_DISPLAY
        .lock()
        .as_ref()
        .and_then(alloc::sync::Weak::upgrade)
    else {
        return;
    };
    if graphics {
        display.suspend_refresh();
        display.scanout.release_master();
    } else if let Err(error) = display
        .scanout
        .acquire_master()
        .and_then(|_| display.scanout.restore_text_nonblocking())
    {
        warn!("failed to restore DRM fbdev master for KD_TEXT: {error}");
    } else {
        // Coalesce all writes made while the graphics VT owned the seat into
        // exactly one full text repaint after master is reacquired.
        display.resume_refresh();
    }
}

impl DisplayCore {
    fn new(scanout: alloc::sync::Arc<DrmFbdev>) -> Self {
        Self {
            size: scanout.len(),
            scanout,
            pseudo_palette: Mutex::new(default_pseudo_palette()),
            damage: DamageTracker::new(),
            pending: AtomicBool::new(false),
            refresh_enabled: AtomicBool::new(true),
            refresh: alloc::sync::Arc::new(WaitQueue::new()),
            commit: Mutex::new(()),
        }
    }

    fn mark_write(&self, offset: usize, len: usize) {
        self.damage.mark(offset, len, self.size);
        self.schedule_refresh();
    }

    fn mark_full(&self) {
        self.damage.mark_full();
        self.schedule_refresh();
    }

    fn schedule_refresh(&self) {
        if !self.refresh_enabled.load(Ordering::Acquire) {
            return;
        }
        // The condition is published before waking the waiter. WaitQueue's
        // check-arm-check wait closes the other side of this race.
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.refresh.notify_one(false);
        }
    }

    fn take_scheduled(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    fn suspend_refresh(&self) {
        // Publish disablement before waiting for an in-flight presentation.
        // A worker which woke during this transition sees false under the same
        // commit lock and leaves its bounded damage record intact.
        self.refresh_enabled.store(false, Ordering::Release);
        self.pending.store(false, Ordering::Release);
        let _commit = self.commit.lock();
    }

    fn resume_refresh(&self) {
        let _commit = self.commit.lock();
        self.refresh_enabled.store(true, Ordering::Release);
        self.damage.mark_full();
        drop(_commit);
        self.schedule_refresh();
    }

    fn refresh_wait_queue(&self) -> alloc::sync::Arc<WaitQueue> {
        self.refresh.clone()
    }

    fn read_bytes(&self, mut offset: usize, mut dst: &mut [u8]) -> VfsResult<()> {
        let pages = self.scanout.pages();
        while !dst.is_empty() {
            let in_page = offset % PAGE_SIZE_4K;
            let count = dst.len().min(PAGE_SIZE_4K - in_page);
            let page = pages.paddr_at(offset / PAGE_SIZE_4K)?;
            // SAFETY: page index and chunk are bounded by `size`; direct-map
            // access is used only to expose this fixed GEM backing.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_to_virt(PhysAddr::from(page.as_usize() + in_page)).as_ptr(),
                    dst.as_mut_ptr(),
                    count,
                )
            };
            offset += count;
            dst = &mut dst[count..];
        }
        Ok(())
    }

    fn write_bytes(&self, mut offset: usize, mut src: &[u8]) -> VfsResult<()> {
        let pages = self.scanout.pages();
        while !src.is_empty() {
            let in_page = offset % PAGE_SIZE_4K;
            let count = src.len().min(PAGE_SIZE_4K - in_page);
            let page = pages.paddr_at(offset / PAGE_SIZE_4K)?;
            // SAFETY: page index and chunk are bounded by `size`; direct-map
            // access is used only to expose this fixed GEM backing.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    phys_to_virt(PhysAddr::from(page.as_usize() + in_page)).as_mut_ptr(),
                    count,
                )
            };
            offset += count;
            src = &src[count..];
        }
        Ok(())
    }

    fn commit_if_needed(&self) {
        let _commit = self.commit.lock();
        if !self.refresh_enabled.load(Ordering::Acquire) {
            return;
        }
        let damage = self.damage.take();
        if damage == DAMAGE_CLEAN {
            return;
        }
        if let Err(error) = self.scanout.present() {
            warn!("Failed to commit framebuffer scanout: {error:?}");
            // Retain correctness after a transient device failure.
            self.mark_full();
        }
    }
}

impl Drop for DisplayCore {
    fn drop(&mut self) {
        // The idle worker owns only a Weak. Wake it so it observes teardown
        // instead of depending on a periodic timer.
        self.refresh.notify_all(false);
    }
}

fn refresh_task(
    display: alloc::sync::Weak<DisplayCore>,
    refresh: alloc::sync::Arc<WaitQueue>,
) -> Result<AxResult<()>, axtask::future::BlockOnError> {
    let delay = core::time::Duration::from_secs_f32(1. / 60.);
    loop {
        // `wait_until` checks the predicate before and after it arms the
        // listener, so a write racing the idle transition cannot lose a wake.
        if let Err(error) = refresh.wait_until(|| {
            display.upgrade().is_none()
                || display
                    .upgrade()
                    .is_some_and(|core| core.pending.load(Ordering::Acquire))
        }) {
            return Ok(Err(error.into()));
        }
        let Some(display) = display.upgrade() else {
            return Ok(Ok(()));
        };
        if !display.take_scheduled() {
            continue;
        }
        // Coalesce bursty write_at/fbcon updates, but stay completely idle
        // when no writer has published damage.
        match axtask::future::block_on(axtask::future::sleep(delay)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Ok(Err(AxError::from(error))),
            Err(error) => return Err(error),
        }
        display.commit_if_needed();
    }
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
    pub fn try_new() -> Result<Self, AxError> {
        let device = crate::drm::primary_device().ok_or(AxError::NoSuchDevice)?;
        let scanout = alloc::sync::Arc::try_new(DrmFbdev::new(device).map_err(AxError::from)?)
            .map_err(|_| AxError::NoMemory)?;
        let core =
            alloc::sync::Arc::try_new(DisplayCore::new(scanout)).map_err(|_| AxError::NoMemory)?;
        let registration = fb_sysfs_registration()?;
        let reservation = global_device_registry().reserve(registration.identity().clone())?;
        let refresh_core = alloc::sync::Arc::downgrade(&core);
        let refresh = core.refresh_wait_queue();
        axtask::spawn_with_name(
            move || match refresh_task(refresh_core, refresh) {
                // Normal owner teardown releases the final strong reference.
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    error!("Framebuffer refresh timer stopped: {error}")
                }
                Err(error) => error!("Framebuffer refresh worker stopped: {error}"),
            },
            "fb-refresh".into(),
        )?;
        // The registry commit atomically exposes the device through its
        // class, canonical /sys/devices path, and /sys/dev/char link.  The
        // devfs node owns this handle and removes all three together.
        let registry_handle = reservation.publish(registration)?;
        *FBCON_DISPLAY.lock() = Some(alloc::sync::Arc::downgrade(&core));
        crate::pseudofs::dev::tty::fbcon::install();
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
                self.core
                    .scanout
                    .pan(request.yoffset)
                    .map_err(AxError::from)
                    .map(|_| 0)
            }
            // FBIOBLANK maps to connector DPMS in the same atomic state
            // machine as ordinary KMS clients.
            0x4611 if arg <= 4 => self
                .core
                .scanout
                .set_blank(arg != 0)
                .map_err(AxError::from)
                .map(|_| 0),
            0x4611 => Err(LinuxError::EOPNOTSUPP.into()),
            _ => Err(AxError::NotATty),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mmap(&self) -> DeviceMmap {
        // mmap is deliberately passive: raw writers publish through fsync or
        // FBIOPAN_DISPLAY. The mapping retains the actual SG GEM pages.
        DeviceMmap::SharedPages(self.core.scanout.pages())
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
    use super::{DAMAGE_CLEAN, DAMAGE_FULL, DamageTracker, FB_DEVICE_ID, fb_sysfs_registration};

    #[test]
    fn damage_tracker_coalesces_and_clears() {
        let damage = DamageTracker::new();
        damage.mark(32, 8, 128);
        damage.mark(8, 16, 128);
        assert_eq!(damage.take(), ((8u64) << 32) | 40);
        assert_eq!(damage.take(), DAMAGE_CLEAN);
    }

    #[test]
    fn damage_tracker_clamps_and_marks_full() {
        let damage = DamageTracker::new();
        damage.mark(120, 32, 128);
        assert_eq!(damage.take(), ((120u64) << 32) | 128);
        damage.mark_full();
        assert_eq!(damage.take(), DAMAGE_FULL);
    }

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
