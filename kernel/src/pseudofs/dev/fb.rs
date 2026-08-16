use core::{
    any::Any,
    mem::{align_of, offset_of, size_of},
    slice,
};

#[allow(unused_imports)]
use axdriver::prelude::DisplayDriverOps;
use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{NodeFlags, VfsError, VfsResult};
use axhal::mem::virt_to_phys;
use memory_addr::{PhysAddrRange, VirtAddr};

use crate::{
    file::IoctlContext,
    pseudofs::{DeviceMmap, DeviceOps},
};

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

fn refresh_task() -> Result<AxResult<()>, axtask::future::BlockOnError> {
    let delay = core::time::Duration::from_secs_f32(1. / 60.);
    loop {
        if !axdisplay::framebuffer_flush() {
            warn!("Failed to refresh framebuffer");
        }
        match axtask::future::block_on(axtask::future::sleep(delay)) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Ok(Err(AxError::from(error))),
            Err(error) => return Err(error),
        }
    }
}

pub struct FrameBuffer {
    base: VirtAddr,
    size: usize,
}
impl FrameBuffer {
    pub fn try_new() -> Result<Self, AxError> {
        axtask::spawn_with_name(
            || match refresh_task() {
                Ok(Ok(())) => error!("Framebuffer refresh worker ended unexpectedly"),
                Ok(Err(error)) => {
                    error!("Framebuffer refresh timer stopped: {error}")
                }
                Err(error) => error!("Framebuffer refresh worker stopped: {error}"),
            },
            "fb-refresh".into(),
        )?;
        let info = axdisplay::framebuffer_info();
        Ok(Self {
            base: VirtAddr::from(info.fb_base_vaddr),
            size: info.fb_size,
        })
    }

    #[allow(clippy::mut_from_ref)]
    fn as_mut_slice(&self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.base.as_mut_ptr(), self.size) }
    }
}
impl DeviceOps for FrameBuffer {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let slice = self.as_mut_slice();
        let offset = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(slice.len());
        let len = buf.len().min(slice.len().saturating_sub(offset));
        buf[..len].copy_from_slice(&slice[offset..offset + len]);
        Ok(len)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        let slice = self.as_mut_slice();
        if offset >= slice.len() as u64 {
            return Err(VfsError::StorageFull);
        }
        let offset = offset as usize;
        let len = buf.len().min(slice.len() - offset);
        slice[offset..offset + len].copy_from_slice(&buf[..len]);
        Ok(len)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            // FBIOGET_VSCREENINFO
            0x4600 => {
                let info = axdisplay::framebuffer_info();
                let line_length = (info.fb_size / info.height as usize) as u32;
                let bpp = line_length / info.width;
                let value = VarScreenInfo {
                    xres: info.width,
                    yres: info.height,
                    xres_virtual: info.width,
                    yres_virtual: info.height,
                    xoffset: 0,
                    yoffset: 0,
                    bits_per_pixel: bpp * 8,
                    grayscale: 0,
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
                    pixclock: 10000000 / info.width * 1000 / info.height,
                    left_margin: (info.width / 8) & 0xf8,
                    right_margin: 32,
                    upper_margin: 16,
                    lower_margin: 4,
                    hsync_len: (info.width / 8) & 0xf8,
                    vsync_len: 4,
                    sync: 0,
                    vmode: 0,
                    rotate: 0,
                    colorspace: 0,
                    reserved: [0; 4],
                };
                let bytes = var_screen_info_to_user_bytes(value);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(crate::mm::map_usercopy_error)?;
                Ok(0)
            }
            // FBIOPUT_VSCREENINFO
            0x4601 => Err(LinuxError::EOPNOTSUPP.into()),
            // FBIOGET_FSCREENINFO
            0x4602 => {
                let info = axdisplay::framebuffer_info();
                let value = FixScreenInfo {
                    id: *b"Virtio Framebuf\0",
                    smem_start: virt_to_phys(VirtAddr::from(info.fb_base_vaddr)).as_usize() as u64,
                    smem_len: info.fb_size as u32,
                    type_: 0,
                    type_aux: 0,
                    visual: 2, // FB_VISUAL_TRUECOLOR
                    xpanstep: 0,
                    ypanstep: 0,
                    ywrapstep: 0,
                    line_length: (info.fb_size / info.height as usize) as u32,
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
            // FBIOGETCMAP
            0x4604 => Err(LinuxError::EOPNOTSUPP.into()),
            // FBIOPUTCMAP
            0x4605 => Err(LinuxError::EOPNOTSUPP.into()),
            // FBIOPAN_DISPLAY
            0x4606 => Err(AxError::InvalidInput),
            // FBIOBLANK
            0x4611 => Err(AxError::InvalidInput),
            _ => Err(AxError::NotATty),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mmap(&self) -> DeviceMmap {
        DeviceMmap::Physical(PhysAddrRange::from_start_size(
            virt_to_phys(self.base),
            self.size,
        ))
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}
