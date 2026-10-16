// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/sysfb/drm_sysfb_helper.h` (v7.2.3) type
//! definitions and inline helpers, plus the firmware-boot inputs the sysfb
//! helpers consume:
//! - `struct screen_info` (include/uapi/linux/screen_info.h), mirrored
//!   field-for-field as a packed 64-byte record;
//! - `struct pixel_format` (include/video/pixel_format.h) and its
//!   `PIXEL_FORMAT_*` descriptors used by the efidrm format table;
//! - `struct resource` (include/linux/ioport.h), reduced to the first four
//!   fields (`start`, `end`, `name`, `flags`) that the sysfb helpers read.
//!
//! The kernel's boot-loader side constructs [`ScreenInfo`]; this crate never
//! parses the boot protocol itself.
#![allow(
    non_camel_case_types,
    non_upper_case_globals,
    non_snake_case,
    dead_code,
    unused_imports
)]

use core::{
    ffi::{c_char, c_void},
    mem::{offset_of, size_of},
};

use crate::{
    drm_crtc_types::DrmCrtcState,
    drm_device_types::DrmDevice,
    drm_format_helper_types::DrmFormatConvState,
    drm_fourcc_types::DrmFormatInfo,
    drm_framebuffer_types::{DRM_FORMAT_MAX_PLANES, DrmFramebuffer},
    drm_gem_atomic_helper_types::DrmShadowPlaneState,
    drm_modes_types::DrmDisplayMode,
    drm_plane_types::DrmPlaneState,
    drm_rect_types::DrmRect,
    iosys_map_types::IosysMap,
    uapi_drm_consts::{DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR},
};

// ---------------------------------------------------------------------------
// include/video/pixel_format.h
// ---------------------------------------------------------------------------

// upstream: include/video/pixel_format.h struct pixel_format (bitfield pair)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PixelFormatBits {
    pub offset: u8,
    pub length: u8,
}

// upstream: include/video/pixel_format.h struct pixel_format, anonymous color struct
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PixelFormatColor {
    pub alpha: PixelFormatBits,
    pub red: PixelFormatBits,
    pub green: PixelFormatBits,
    pub blue: PixelFormatBits,
}

// upstream: include/video/pixel_format.h struct pixel_format, anonymous union
#[repr(C)]
#[derive(Clone, Copy)]
pub union PixelFormatFields {
    pub color: PixelFormatColor,
    pub index: PixelFormatBits,
}

// upstream: include/video/pixel_format.h struct pixel_format
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub indexed: bool,
    pub fields: PixelFormatFields,
}

const fn pf_bits(offset: u8, length: u8) -> PixelFormatBits {
    PixelFormatBits { offset, length }
}

const fn pf_color(alpha: (u8, u8), red: (u8, u8), green: (u8, u8), blue: (u8, u8)) -> PixelFormat {
    PixelFormat {
        bits_per_pixel: 0,
        indexed: false,
        fields: PixelFormatFields {
            color: PixelFormatColor {
                alpha: pf_bits(alpha.0, alpha.1),
                red: pf_bits(red.0, red.1),
                green: pf_bits(green.0, green.1),
                blue: pf_bits(blue.0, blue.1),
            },
        },
    }
}

const fn pf_with_bpp(bpp: u8, mut fmt: PixelFormat) -> PixelFormat {
    fmt.bits_per_pixel = bpp;
    fmt
}

// upstream: include/video/pixel_format.h PIXEL_FORMAT_XRGB1555
pub const PIXEL_FORMAT_XRGB1555: PixelFormat =
    pf_with_bpp(16, pf_color((0, 0), (10, 5), (5, 5), (0, 5)));
// upstream: include/video/pixel_format.h PIXEL_FORMAT_RGB565
pub const PIXEL_FORMAT_RGB565: PixelFormat =
    pf_with_bpp(16, pf_color((0, 0), (11, 5), (5, 6), (0, 5)));
// upstream: include/video/pixel_format.h PIXEL_FORMAT_RGB888
pub const PIXEL_FORMAT_RGB888: PixelFormat =
    pf_with_bpp(24, pf_color((0, 0), (16, 8), (8, 8), (0, 8)));
// upstream: include/video/pixel_format.h PIXEL_FORMAT_XRGB8888
pub const PIXEL_FORMAT_XRGB8888: PixelFormat =
    pf_with_bpp(32, pf_color((0, 0), (16, 8), (8, 8), (0, 8)));
// upstream: include/video/pixel_format.h PIXEL_FORMAT_XBGR8888
pub const PIXEL_FORMAT_XBGR8888: PixelFormat =
    pf_with_bpp(32, pf_color((0, 0), (0, 8), (8, 8), (16, 8)));
// upstream: include/video/pixel_format.h PIXEL_FORMAT_XRGB2101010
pub const PIXEL_FORMAT_XRGB2101010: PixelFormat =
    pf_with_bpp(32, pf_color((0, 0), (20, 10), (10, 10), (0, 10)));

// upstream: include/video/pixel_format.h pixel_format_cmp() / pixel_format_equal()
pub fn pixel_format_equal(lhs: &PixelFormat, rhs: &PixelFormat) -> bool {
    if lhs.bits_per_pixel != rhs.bits_per_pixel || lhs.indexed != rhs.indexed {
        return false;
    }
    // SAFETY: both unions are read through the member selected by `indexed`,
    // which both descriptors share, exactly as pixel_format_cmp() does.
    unsafe {
        if lhs.indexed {
            let (l, r) = (lhs.fields.index, rhs.fields.index);
            l.offset == r.offset && l.length == r.length
        } else {
            let (l, r) = (lhs.fields.color, rhs.fields.color);
            l.alpha.offset == r.alpha.offset
                && l.alpha.length == r.alpha.length
                && l.red.offset == r.red.offset
                && l.red.length == r.red.length
                && l.green.offset == r.green.offset
                && l.green.length == r.green.length
                && l.blue.offset == r.blue.offset
                && l.blue.length == r.blue.length
        }
    }
}

// ---------------------------------------------------------------------------
// include/uapi/linux/screen_info.h
// ---------------------------------------------------------------------------

pub const VIDEO_TYPE_MDA: u8 = 0x10;
pub const VIDEO_TYPE_CGA: u8 = 0x11;
pub const VIDEO_TYPE_EGAM: u8 = 0x20;
pub const VIDEO_TYPE_EGAC: u8 = 0x21;
pub const VIDEO_TYPE_VGAC: u8 = 0x22;
pub const VIDEO_TYPE_VLFB: u8 = 0x23;
pub const VIDEO_TYPE_PICA_S3: u8 = 0x30;
pub const VIDEO_TYPE_MIPS_G364: u8 = 0x31;
pub const VIDEO_TYPE_SGI: u8 = 0x33;
pub const VIDEO_TYPE_TGAC: u8 = 0x40;
pub const VIDEO_TYPE_SUN: u8 = 0x50;
pub const VIDEO_TYPE_SUNPCI: u8 = 0x51;
pub const VIDEO_TYPE_PMAC: u8 = 0x60;
pub const VIDEO_TYPE_EFI: u8 = 0x70;

pub const VIDEO_CAPABILITY_SKIP_QUIRKS: u32 = 1 << 0;
pub const VIDEO_CAPABILITY_64BIT_BASE: u32 = 1 << 1;

// upstream: include/uapi/linux/screen_info.h struct screen_info (packed, 0x40 bytes)
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ScreenInfo {
    pub orig_x: u8,
    pub orig_y: u8,
    pub ext_mem_k: u16,
    pub orig_video_page: u16,
    pub orig_video_mode: u8,
    pub orig_video_cols: u8,
    pub flags: u8,
    pub unused2: u8,
    pub orig_video_ega_bx: u16,
    pub unused3: u16,
    pub orig_video_lines: u8,
    pub orig_video_isVGA: u8,
    pub orig_video_points: u16,
    pub lfb_width: u16,
    pub lfb_height: u16,
    pub lfb_depth: u16,
    pub lfb_base: u32,
    pub lfb_size: u32,
    pub cl_magic: u16,
    pub cl_offset: u16,
    pub lfb_linelength: u16,
    pub red_size: u8,
    pub red_pos: u8,
    pub green_size: u8,
    pub green_pos: u8,
    pub blue_size: u8,
    pub blue_pos: u8,
    pub rsvd_size: u8,
    pub rsvd_pos: u8,
    pub vesapm_seg: u16,
    pub vesapm_off: u16,
    pub pages: u16,
    pub vesa_attributes: u16,
    pub capabilities: u32,
    pub ext_lfb_base: u32,
    pub _reserved: [u8; 2],
}

const _: () = assert!(size_of::<ScreenInfo>() == 0x40);
const _: () = assert!(offset_of!(ScreenInfo, orig_video_isVGA) == 0x0f);
const _: () = assert!(offset_of!(ScreenInfo, lfb_width) == 0x12);
const _: () = assert!(offset_of!(ScreenInfo, lfb_base) == 0x18);
const _: () = assert!(offset_of!(ScreenInfo, lfb_linelength) == 0x24);
const _: () = assert!(offset_of!(ScreenInfo, red_size) == 0x26);
const _: () = assert!(offset_of!(ScreenInfo, capabilities) == 0x36);
const _: () = assert!(offset_of!(ScreenInfo, ext_lfb_base) == 0x3a);

// ---------------------------------------------------------------------------
// include/linux/ioport.h (reduced struct resource)
// ---------------------------------------------------------------------------

pub const IORESOURCE_IO: u64 = 0x0000_0100;
pub const IORESOURCE_MEM: u64 = 0x0000_0200;

// upstream: include/linux/ioport.h struct resource (first four members only)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Resource {
    pub start: u64,
    pub end: u64,
    pub name: *const c_char,
    pub flags: u64,
}

// ---------------------------------------------------------------------------
// drm_sysfb_helper.h
// ---------------------------------------------------------------------------

// upstream: drm_sysfb_helper.h struct drm_sysfb_format
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DrmSysfbFormat {
    pub pixel: PixelFormat,
    pub fourcc: u32,
}

// upstream: drm_sysfb_helper.h drm_sysfb_blit_func
pub type DrmSysfbBlitFunc = unsafe extern "C" fn(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
);

// upstream: include/drm/drm_panic.h struct drm_scanout_buffer
#[repr(C)]
pub struct DrmScanoutBuffer {
    pub format: *const DrmFormatInfo,
    pub map: [IosysMap; DRM_FORMAT_MAX_PLANES],
    pub pages: *mut *mut c_void,
    pub width: u32,
    pub height: u32,
    pub pitch: [u32; DRM_FORMAT_MAX_PLANES],
    pub set_pixel: Option<unsafe extern "C" fn(*mut DrmScanoutBuffer, u32, u32, u32)>,
    pub private: *mut c_void,
}

// upstream: drm_sysfb_helper.h struct drm_sysfb_device
#[repr(C)]
pub struct DrmSysfbDevice {
    pub dev: DrmDevice,
    /// Can be NULL.
    pub edid: *const u8,
    // hardware settings
    pub fb_mode: DrmDisplayMode,
    pub fb_format: *const DrmFormatInfo,
    pub fb_pitch: u32,
    pub fb_gamma_lut_size: u32,
    // hardware-framebuffer kernel address
    pub fb_addr: IosysMap,
}

// upstream: drm_sysfb_helper.h to_drm_sysfb_device()
/// # Safety
/// `dev` must point to the `dev` member of a live `DrmSysfbDevice`.
#[inline]
pub unsafe fn to_drm_sysfb_device(dev: *mut DrmDevice) -> *mut DrmSysfbDevice {
    dev.cast::<u8>()
        .sub(offset_of!(DrmSysfbDevice, dev))
        .cast::<DrmSysfbDevice>()
}

// upstream: drm_sysfb_helper.h struct drm_sysfb_plane_state
#[repr(C)]
pub struct DrmSysfbPlaneState {
    pub base: DrmShadowPlaneState,
    /// transfers framebuffer data to scanout buffer in CRTC format
    pub blit_to_crtc: Option<DrmSysfbBlitFunc>,
}

// upstream: drm_sysfb_helper.h to_drm_sysfb_plane_state()
/// # Safety
/// `base` must point to the `base` member of a live `DrmSysfbPlaneState`.
#[inline]
pub unsafe fn to_drm_sysfb_plane_state(base: *mut DrmPlaneState) -> *mut DrmSysfbPlaneState {
    // to_drm_shadow_plane_state() is a cast (the shadow state heads the plane state).
    base.cast::<DrmShadowPlaneState>()
        .cast::<u8>()
        .sub(offset_of!(DrmSysfbPlaneState, base))
        .cast::<DrmSysfbPlaneState>()
}

// upstream: drm_sysfb_helper.h struct drm_sysfb_crtc_state
#[repr(C)]
pub struct DrmSysfbCrtcState {
    pub base: DrmCrtcState,
    /// CRTC input color format; required for color mgmt.
    pub format: *const DrmFormatInfo,
}

// upstream: drm_sysfb_helper.h to_drm_sysfb_crtc_state()
/// # Safety
/// `base` must point to the `base` member of a live `DrmSysfbCrtcState`.
#[inline]
pub unsafe fn to_drm_sysfb_crtc_state(base: *mut DrmCrtcState) -> *mut DrmSysfbCrtcState {
    base.cast::<u8>()
        .sub(offset_of!(DrmSysfbCrtcState, base))
        .cast::<DrmSysfbCrtcState>()
}

// upstream: drm_sysfb_helper.h DRM_SYSFB_PLANE_NFORMATS()
pub const fn DRM_SYSFB_PLANE_NFORMATS(num_native: usize) -> usize {
    num_native + 1
}

// upstream: drm_sysfb_helper.h DRM_SYSFB_PLANE_FORMAT_MODIFIERS
pub const DRM_SYSFB_PLANE_FORMAT_MODIFIERS: [u64; 2] =
    [DRM_FORMAT_MOD_LINEAR, DRM_FORMAT_MOD_INVALID];
