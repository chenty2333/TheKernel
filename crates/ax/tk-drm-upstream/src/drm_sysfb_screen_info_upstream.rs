// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
//! Port of Linux `drivers/gpu/drm/sysfb/drm_sysfb_screen_info.c` (v7.2.3),
//! together with the `screen_info` helpers it consumes from
//! `include/linux/screen_info.h` and `drivers/video/screen_info_generic.c`
//! (video-type classification, linear-framebuffer base/size, resource
//! derivation and pixel-format derivation).
//!
//! The screen_info record itself is provided by the kernel boot side as
//! [`ScreenInfo`]; see `drm_sysfb_helper_types`.
//!
//! Deviations: `drm_sysfb_get_height_si()` and friends return `i32` with the
//! same negative-errno convention. `drm_sysfb_get_memory_si()` returns
//! `Option<&Resource>` instead of a NULL-or-pointer. A zero `height` in
//! `drm_sysfb_get_stride_si()` yields a zero size bound (rejected as
//! "exceeds maximum") instead of the C divide-by-zero.

#![allow(non_snake_case, dead_code, unsafe_code)]

use alloc::format;

use tk_linux_kapi::config::EINVAL;

use crate::{
    drm_device_types::DrmDevice,
    drm_fourcc_types::DrmFormatInfo,
    drm_fourcc_upstream::drm_format_info_min_pitch,
    drm_sysfb_helper_types::{
        IORESOURCE_IO, IORESOURCE_MEM, PixelFormat, PixelFormatBits, PixelFormatColor,
        PixelFormatFields, Resource, ScreenInfo, VIDEO_CAPABILITY_64BIT_BASE, VIDEO_TYPE_CGA,
        VIDEO_TYPE_EFI, VIDEO_TYPE_EGAC, VIDEO_TYPE_EGAM, VIDEO_TYPE_MDA, VIDEO_TYPE_MIPS_G364,
        VIDEO_TYPE_PICA_S3, VIDEO_TYPE_PMAC, VIDEO_TYPE_SGI, VIDEO_TYPE_SUN, VIDEO_TYPE_SUNPCI,
        VIDEO_TYPE_TGAC, VIDEO_TYPE_VGAC, VIDEO_TYPE_VLFB,
    },
    drm_sysfb_upstream::{drm_sysfb_get_validated_int0, drm_warn},
};

const S64_MAX: u64 = i64::MAX as u64;
const U16_MAX: u32 = u16::MAX as u32;

// upstream: drm_sysfb_screen_info.c drm_sysfb_get_validated_size0()
fn drm_sysfb_get_validated_size0(dev: *const DrmDevice, name: &str, value: u64, max: u64) -> i64 {
    if value == 0 {
        drm_warn(dev, &format!("{} of 0 not allowed\n", name));
        return -(EINVAL as i64);
    } else if value > core::cmp::min(max, S64_MAX) {
        drm_warn(
            dev,
            &format!("{} of {} exceeds maximum of {}\n", name, value, max),
        );
        return -(EINVAL as i64);
    }
    value as i64
}

// upstream: drm_sysfb_screen_info.c drm_sysfb_get_width_si()
pub fn drm_sysfb_get_width_si(dev: *const DrmDevice, si: &ScreenInfo) -> i32 {
    let width = si.lfb_width;
    drm_sysfb_get_validated_int0(dev, "width", width as u64, U16_MAX)
}

// upstream: drm_sysfb_screen_info.c drm_sysfb_get_height_si()
pub fn drm_sysfb_get_height_si(dev: *const DrmDevice, si: &ScreenInfo) -> i32 {
    let height = si.lfb_height;
    drm_sysfb_get_validated_int0(dev, "height", height as u64, U16_MAX)
}

// upstream: drm_sysfb_screen_info.c drm_sysfb_get_memory_si()
pub fn drm_sysfb_get_memory_si<'a>(
    dev: *const DrmDevice,
    si: &ScreenInfo,
    res: &'a mut Resource,
) -> Option<&'a Resource> {
    let num = screen_info_resources(si, core::slice::from_mut(res));
    if num == 0 {
        drm_warn(dev, "memory resource not found\n");
        return None;
    }
    Some(res)
}

// upstream: drm_sysfb_screen_info.c drm_sysfb_get_stride_si()
pub fn drm_sysfb_get_stride_si(
    dev: *const DrmDevice,
    si: &ScreenInfo,
    format: *const DrmFormatInfo,
    width: u32,
    height: u32,
    size: u64,
) -> i32 {
    let mut lfb_linelength = { si.lfb_linelength } as u64;
    if lfb_linelength == 0 {
        // SAFETY: `format` is a live DRM format descriptor from drm_format_info().
        lfb_linelength = unsafe { drm_format_info_min_pitch(format, 0, width) };
    }

    // div64_u64(size, height); height is validated non-zero by callers.
    let max = if height == 0 { 0 } else { size / height as u64 };
    let stride = drm_sysfb_get_validated_size0(dev, "stride", lfb_linelength, max);
    if stride < i32::MIN as i64 || stride > i32::MAX as i64 {
        return -EINVAL;
    }
    stride as i32 // stride or negative errno code
}

// upstream: drm_sysfb_screen_info.c drm_sysfb_get_visible_size_si()
pub fn drm_sysfb_get_visible_size_si(
    dev: *const DrmDevice,
    _si: &ScreenInfo,
    height: u32,
    stride: u32,
    size: u64,
) -> i64 {
    // mul_u32_u32(height, stride)
    let vsize = (height as u64) * (stride as u64);
    drm_sysfb_get_validated_size0(dev, "visible size", vsize, size)
}

// ---------------------------------------------------------------------------
// include/linux/screen_info.h inline helpers
// ---------------------------------------------------------------------------

// upstream: include/linux/screen_info.h __screen_info_lfb_base()
pub fn __screen_info_lfb_base(si: &ScreenInfo) -> u64 {
    let mut lfb_base = { si.lfb_base } as u64;
    if { si.capabilities } & VIDEO_CAPABILITY_64BIT_BASE != 0 {
        lfb_base |= ({ si.ext_lfb_base } as u64) << 32;
    }
    lfb_base
}

// upstream: include/linux/screen_info.h __screen_info_lfb_size()
pub fn __screen_info_lfb_size(si: &ScreenInfo, typ: u8) -> u64 {
    let mut lfb_size = { si.lfb_size } as u64;
    if typ == VIDEO_TYPE_VLFB {
        lfb_size <<= 16;
    }
    lfb_size
}

// upstream: include/linux/screen_info.h __screen_info_video_type()
pub fn __screen_info_video_type(typ: u8) -> u8 {
    match typ {
        VIDEO_TYPE_MDA | VIDEO_TYPE_CGA | VIDEO_TYPE_EGAM | VIDEO_TYPE_EGAC | VIDEO_TYPE_VGAC
        | VIDEO_TYPE_VLFB | VIDEO_TYPE_PICA_S3 | VIDEO_TYPE_MIPS_G364 | VIDEO_TYPE_SGI
        | VIDEO_TYPE_TGAC | VIDEO_TYPE_SUN | VIDEO_TYPE_SUNPCI | VIDEO_TYPE_PMAC
        | VIDEO_TYPE_EFI => typ,
        _ => 0,
    }
}

// upstream: include/linux/screen_info.h screen_info_video_type()
pub fn screen_info_video_type(si: &ScreenInfo) -> u8 {
    let is_vga = { si.orig_video_isVGA };

    // check if display output is on
    if is_vga == 0 {
        return 0;
    }

    // check for a known VIDEO_TYPE_ constant
    let typ = __screen_info_video_type(is_vga);
    if typ != 0 {
        return is_vga;
    }

    // check if text mode has been initialized
    if { si.orig_video_lines } == 0 || { si.orig_video_cols } == 0 {
        return 0;
    }

    // 80x25 text, mono
    if { si.orig_video_mode } == 0x07 {
        if ({ si.orig_video_ega_bx } & 0xff) != 0x10 {
            return VIDEO_TYPE_EGAM;
        } else {
            return VIDEO_TYPE_MDA;
        }
    }

    // EGA/VGA, 16 colors
    if ({ si.orig_video_ega_bx } & 0xff) != 0x10 {
        if is_vga != 0 {
            return VIDEO_TYPE_VGAC;
        } else {
            return VIDEO_TYPE_EGAC;
        }
    }

    // the rest...
    VIDEO_TYPE_CGA
}

// upstream: drivers/video/screen_info_generic.c __screen_info_has_ega_gfx()
fn __screen_info_has_ega_gfx(mode: u32) -> bool {
    matches!(mode, 0x0d | 0x0e | 0x0f | 0x10)
}

// upstream: drivers/video/screen_info_generic.c __screen_info_has_vga_gfx()
fn __screen_info_has_vga_gfx(mode: u32) -> bool {
    match mode {
        0x10 | 0x12 | 0x13 | 0x6a => true,
        _ => __screen_info_has_ega_gfx(mode),
    }
}

// upstream: drivers/video/screen_info_generic.c resource_init_named()
fn resource_init_named(r: &mut Resource, start: u64, size: u64, name: &'static [u8], flags: u64) {
    *r = Resource {
        start: 0,
        end: 0,
        name: core::ptr::null(),
        flags: 0,
    };
    r.start = start;
    r.end = start.wrapping_add(size).wrapping_sub(1);
    r.name = name.as_ptr().cast();
    r.flags = flags;
}

// upstream: drivers/video/screen_info_generic.c resource_init_io_named()
fn resource_init_io_named(r: &mut Resource, start: u64, size: u64, name: &'static [u8]) {
    resource_init_named(r, start, size, name, IORESOURCE_IO);
}

// upstream: drivers/video/screen_info_generic.c resource_init_mem_named()
fn resource_init_mem_named(r: &mut Resource, start: u64, size: u64, name: &'static [u8]) {
    resource_init_named(r, start, size, name, IORESOURCE_MEM);
}

// upstream: drivers/video/screen_info_generic.c screen_info_resources()
/// Fills up to `r.len()` resources and returns how many were written, or a
/// negative value (-EINVAL) for unsupported video types.
pub fn screen_info_resources(si: &ScreenInfo, r: &mut [Resource]) -> isize {
    let num = r.len();
    let typ = screen_info_video_type(si);
    let mut pos = 0usize;

    match typ {
        VIDEO_TYPE_MDA => {
            if num > 0 {
                resource_init_io_named(&mut r[pos], 0x3b0, 12, b"mda\0");
                pos += 1;
            }
            if num > 1 {
                resource_init_io_named(&mut r[pos], 0x3bf, 0x01, b"mda\0");
                pos += 1;
            }
            if num > 2 {
                resource_init_mem_named(&mut r[pos], 0xb0000, 0x2000, b"mda\0");
                pos += 1;
            }
        }
        VIDEO_TYPE_CGA => {
            if num > 0 {
                resource_init_io_named(&mut r[pos], 0x3d4, 0x02, b"cga\0");
                pos += 1;
            }
            if num > 1 {
                resource_init_mem_named(&mut r[pos], 0xb8000, 0x2000, b"cga\0");
                pos += 1;
            }
        }
        VIDEO_TYPE_EGAM => {
            if num > 0 {
                resource_init_io_named(&mut r[pos], 0x3bf, 0x10, b"ega\0");
                pos += 1;
            }
            if num > 1 {
                resource_init_mem_named(&mut r[pos], 0xb0000, 0x8000, b"ega\0");
                pos += 1;
            }
        }
        VIDEO_TYPE_EGAC => {
            if num > 0 {
                resource_init_io_named(&mut r[pos], 0x3c0, 0x20, b"ega\0");
                pos += 1;
            }
            if num > 1 {
                if __screen_info_has_ega_gfx({ si.orig_video_mode } as u32) {
                    resource_init_mem_named(&mut r[pos], 0xa0000, 0x10000, b"ega\0");
                } else {
                    resource_init_mem_named(&mut r[pos], 0xb8000, 0x8000, b"ega\0");
                }
                pos += 1;
            }
        }
        VIDEO_TYPE_VGAC => {
            if num > 0 {
                resource_init_io_named(&mut r[pos], 0x3c0, 0x20, b"vga+\0");
                pos += 1;
            }
            if num > 1 {
                if __screen_info_has_vga_gfx({ si.orig_video_mode } as u32) {
                    resource_init_mem_named(&mut r[pos], 0xa0000, 0x10000, b"vga+\0");
                } else {
                    resource_init_mem_named(&mut r[pos], 0xb8000, 0x8000, b"vga+\0");
                }
                pos += 1;
            }
        }
        VIDEO_TYPE_VLFB | VIDEO_TYPE_EFI => {
            let base = __screen_info_lfb_base(si);
            if base == 0 {
                return pos as isize;
            }
            let size = __screen_info_lfb_size(si, typ);
            if size == 0 {
                return pos as isize;
            }
            if num > 0 {
                resource_init_mem_named(&mut r[pos], base, size, b"lfb\0");
                pos += 1;
            }
        }
        _ => {
            // not supported
            return -(EINVAL as isize);
        }
    }

    pos as isize
}

// upstream: drivers/video/screen_info_generic.c __screen_info_lfb_bits_per_pixel()
pub fn __screen_info_lfb_bits_per_pixel(si: &ScreenInfo) -> u32 {
    let mut bits_per_pixel = { si.lfb_depth } as u32;
    if bits_per_pixel > 8 {
        let red = { si.red_size } as u32 + { si.red_pos } as u32;
        let green = { si.green_size } as u32 + { si.green_pos } as u32;
        let blue = { si.blue_size } as u32 + { si.blue_pos } as u32;
        let rsvd = { si.rsvd_size } as u32 + { si.rsvd_pos } as u32;
        bits_per_pixel = core::cmp::max(core::cmp::max(core::cmp::max(red, green), blue), rsvd);
        bits_per_pixel = core::cmp::max(bits_per_pixel, { si.lfb_depth } as u32);
    }
    bits_per_pixel
}

// upstream: drivers/video/screen_info_generic.c __screen_info_lfb_pixel_format()
fn __screen_info_lfb_pixel_format(si: &ScreenInfo, f: &mut PixelFormat) -> i32 {
    let bits_per_pixel = __screen_info_lfb_bits_per_pixel(si);

    if bits_per_pixel > u8::MAX as u32 {
        return -EINVAL;
    }

    f.bits_per_pixel = bits_per_pixel as u8;

    if { si.lfb_depth } > 8 {
        f.indexed = false;
        f.fields = PixelFormatFields {
            color: PixelFormatColor {
                alpha: PixelFormatBits {
                    offset: 0,
                    length: 0,
                },
                red: PixelFormatBits {
                    offset: { si.red_pos },
                    length: { si.red_size },
                },
                green: PixelFormatBits {
                    offset: { si.green_pos },
                    length: { si.green_size },
                },
                blue: PixelFormatBits {
                    offset: { si.blue_pos },
                    length: { si.blue_size },
                },
            },
        };
    } else {
        f.indexed = true;
        f.fields = PixelFormatFields {
            index: PixelFormatBits {
                offset: 0,
                length: { si.lfb_depth } as u8,
            },
        };
    }
    0
}

// upstream: drivers/video/screen_info_generic.c screen_info_pixel_format()
pub fn screen_info_pixel_format(si: &ScreenInfo, f: &mut PixelFormat) -> i32 {
    let typ = screen_info_video_type(si);

    // TODO (upstream): Add support for additional types as needed.
    match typ {
        VIDEO_TYPE_VLFB | VIDEO_TYPE_EFI => __screen_info_lfb_pixel_format(si, f),
        _ => -EINVAL, // not supported
    }
}
