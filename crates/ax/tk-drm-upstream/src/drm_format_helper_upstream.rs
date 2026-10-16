// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! Port of Linux `drivers/gpu/drm/drm_format_helper.c` (v7.2.3) together with
//! the XRGB8888 pixel helpers of `drivers/gpu/drm/drm_format_internal.h`.
//!
//! Scope: the format-conversion state (`drm_format_conv_state_*`), the clip
//! byte offset, the `drm_fb_xfrm()` transfer engine, `drm_fb_memcpy()`, and
//! every `drm_fb_xrgb8888_to_*()` converter that `drm_sysfb_get_blit_func()`
//! (drm_sysfb_modeset.c) dispatches to. The swab, gray, mono, argb4444 and
//! argb8888->argb4444 converters are not reachable from the sysfb/efidrm path
//! and are not ported here.
//!
//! Deviations from the C source:
//! - The 32-bit line helpers address the source and destination through byte
//!   offsets with unaligned little-endian loads and stores (`read_unaligned`,
//!   `to_le`) instead of `__le32 *` casts. The values are identical on the
//!   little-endian x86_64 target and no alignment is assumed for scanout
//!   buffers. The pixel expressions are kept literally.
//! - `memcpy_toio()` is a plain copy; the MMIO barriers are not modelled.
//! - `ARCH_KMALLOC_MINALIGN` is 8 on x86_64 and is used as a constant.

#![allow(non_snake_case, dead_code, unsafe_code)]

use core::{ffi::c_void, ptr};

use tk_linux_kapi::{
    config::{ENOMEM, GFP_KERNEL},
    memory::{kfree, krealloc},
};

use crate::{
    drm_format_helper_types::DrmFormatConvState,
    drm_fourcc_types::DrmFormatInfo,
    drm_fourcc_upstream::drm_format_info_bpp,
    drm_framebuffer_types::{DRM_FORMAT_MAX_PLANES, DrmFramebuffer},
    drm_gem_framebuffer_helper_upstream::iosys_map_incr,
    drm_rect_types::DrmRect,
    drm_rect_upstream::{drm_rect_height, drm_rect_width},
    iosys_map_types::IosysMap,
};

/// `ARCH_KMALLOC_MINALIGN` on x86_64 (`__alignof__(unsigned long long)`).
const ARCH_KMALLOC_MINALIGN: usize = 8;

/// `void (*xfrm_line)(void *dbuf, const void *sbuf, unsigned int npixels)`.
type XfrmLine = unsafe fn(*mut c_void, *const c_void, u32);

// ---------------------------------------------------------------------------
// Little-endian byte-offset access helpers (stand-ins for the __le casts).
// ---------------------------------------------------------------------------

#[inline(always)]
unsafe fn ld_le32_px(src: *const c_void, pixel: usize) -> u32 {
    // upstream: drm_format_helper.c le32_to_cpup(sbuf32 + pixel)
    u32::from_le(ptr::read_unaligned(
        (src as *const u8).add(pixel * 4).cast::<u32>(),
    ))
}

#[inline(always)]
unsafe fn st_u8(dst: *mut c_void, off: usize, v: u8) {
    *(dst as *mut u8).add(off) = v;
}

#[inline(always)]
unsafe fn st_u16(dst: *mut c_void, off: usize, v: u16) {
    ptr::write_unaligned((dst as *mut u8).add(off).cast::<u16>(), v.to_le());
}

#[inline(always)]
unsafe fn st_u32(dst: *mut c_void, off: usize, v: u32) {
    ptr::write_unaligned((dst as *mut u8).add(off).cast::<u32>(), v.to_le());
}

#[inline(always)]
unsafe fn st_u64(dst: *mut c_void, off: usize, v: u64) {
    ptr::write_unaligned((dst as *mut u8).add(off).cast::<u64>(), v.to_le());
}

// ---------------------------------------------------------------------------
// Format-conversion state
// ---------------------------------------------------------------------------

// upstream: drm_format_helper.c drm_format_conv_state_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_format_conv_state_init(state: *mut DrmFormatConvState) {
    (*state).tmp.mem = ptr::null_mut();
    (*state).tmp.size = 0;
    (*state).tmp.preallocated = false;
}

// upstream: drm_format_helper.c drm_format_conv_state_copy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_format_conv_state_copy(
    state: *mut DrmFormatConvState,
    _old_state: *const DrmFormatConvState,
) {
    // So far there is only temporary storage, which is not duplicated.
    (*state).tmp.mem = ptr::null_mut();
    (*state).tmp.size = 0;
    (*state).tmp.preallocated = false;
}

// upstream: drm_format_helper.c drm_format_conv_state_reserve()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_format_conv_state_reserve(
    state: *mut DrmFormatConvState,
    new_size: usize,
    flags: u32,
) -> *mut c_void {
    if new_size <= (*state).tmp.size {
        return (*state).tmp.mem;
    } else if (*state).tmp.preallocated {
        return ptr::null_mut();
    }

    let mem = krealloc((*state).tmp.mem, new_size, flags);
    if mem.is_null() {
        return ptr::null_mut();
    }
    (*state).tmp.mem = mem;
    (*state).tmp.size = new_size;
    (*state).tmp.mem
}

// upstream: drm_format_helper.c drm_format_conv_state_release()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_format_conv_state_release(state: *mut DrmFormatConvState) {
    if (*state).tmp.preallocated {
        return;
    }
    kfree((*state).tmp.mem);
    (*state).tmp.mem = ptr::null_mut();
    (*state).tmp.size = 0;
}

// ---------------------------------------------------------------------------
// Clip offsets
// ---------------------------------------------------------------------------

// upstream: drm_format_helper.c clip_offset()
#[inline]
fn clip_offset(clip: &DrmRect, pitch: u32, cpp: u32) -> u32 {
    (clip.y1 as u32)
        .wrapping_mul(pitch)
        .wrapping_add((clip.x1 as u32).wrapping_mul(cpp))
}

// upstream: drm_format_helper.c drm_fb_clip_offset()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_clip_offset(
    pitch: u32,
    format: *const DrmFormatInfo,
    clip: *const DrmRect,
) -> u32 {
    clip_offset(&*clip, pitch, (*format)._anon_6.cpp[0] as u32)
}

// ---------------------------------------------------------------------------
// Transfer engine
// ---------------------------------------------------------------------------

// upstream: drm_format_helper.c __drm_fb_xfrm()
unsafe fn __drm_fb_xfrm(
    dst: *mut c_void,
    dst_pitch: usize,
    dst_pixsize: usize,
    vaddr: *const c_void,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    vaddr_cached_hint: bool,
    state: *mut DrmFormatConvState,
    xfrm_line: XfrmLine,
) -> i32 {
    let clip = &*clip;
    let linepixels = drm_rect_width(clip) as usize;
    let lines = drm_rect_height(clip) as usize;
    let cpp = (*(*fb).format)._anon_6.cpp[0] as usize;
    let sbuf_len = linepixels * cpp;
    let pitch0 = (*fb).pitches[0] as usize;

    // Some source buffers (DMA memory, write-combined) are slow to read, so
    // fetch one line at a time into a temporary.
    let mut stmp: *mut c_void = ptr::null_mut();
    if !vaddr_cached_hint {
        stmp = drm_format_conv_state_reserve(state, sbuf_len, GFP_KERNEL);
        if stmp.is_null() {
            return -ENOMEM;
        }
    }

    let dst_pitch = if dst_pitch == 0 {
        drm_rect_width(clip) as usize * dst_pixsize
    } else {
        dst_pitch
    };

    let mut vaddr = vaddr
        .cast::<u8>()
        .add(clip_offset(clip, pitch0 as u32, cpp as u32) as usize);
    let mut dst = dst.cast::<u8>();
    for _ in 0..lines {
        let sbuf: *const c_void = if !stmp.is_null() {
            ptr::copy_nonoverlapping(vaddr, stmp.cast::<u8>(), sbuf_len);
            stmp
        } else {
            vaddr.cast()
        };
        xfrm_line(dst.cast(), sbuf, linepixels as u32);
        vaddr = vaddr.add(pitch0);
        dst = dst.add(dst_pitch);
    }
    0
}

// upstream: drm_format_helper.c __drm_fb_xfrm_toio()
unsafe fn __drm_fb_xfrm_toio(
    dst: *mut c_void,
    dst_pitch: usize,
    dst_pixsize: usize,
    vaddr: *const c_void,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    vaddr_cached_hint: bool,
    state: *mut DrmFormatConvState,
    xfrm_line: XfrmLine,
) -> i32 {
    let clip = &*clip;
    let linepixels = drm_rect_width(clip) as usize;
    let lines = drm_rect_height(clip) as usize;
    let dbuf_len = linepixels * dst_pixsize;
    let stmp_off = (dbuf_len + ARCH_KMALLOC_MINALIGN - 1) & !(ARCH_KMALLOC_MINALIGN - 1);
    let cpp = (*(*fb).format)._anon_6.cpp[0] as usize;
    let sbuf_len = linepixels * cpp;
    let pitch0 = (*fb).pitches[0] as usize;

    let dbuf: *mut c_void;
    let mut stmp: *mut u8 = ptr::null_mut();
    if vaddr_cached_hint {
        dbuf = drm_format_conv_state_reserve(state, dbuf_len, GFP_KERNEL);
    } else {
        dbuf = drm_format_conv_state_reserve(state, stmp_off + sbuf_len, GFP_KERNEL);
    }
    if dbuf.is_null() {
        return -ENOMEM;
    }
    if !vaddr_cached_hint {
        stmp = dbuf.cast::<u8>().add(stmp_off);
    }

    let dst_pitch = if dst_pitch == 0 {
        linepixels * dst_pixsize
    } else {
        dst_pitch
    };

    let mut vaddr = vaddr
        .cast::<u8>()
        .add(clip_offset(clip, pitch0 as u32, cpp as u32) as usize);
    let mut dst = dst.cast::<u8>();
    for _ in 0..lines {
        let sbuf: *const c_void = if !stmp.is_null() {
            ptr::copy_nonoverlapping(vaddr, stmp, sbuf_len);
            stmp.cast()
        } else {
            vaddr.cast()
        };
        xfrm_line(dbuf, sbuf, linepixels as u32);
        // memcpy_toio(dst, dbuf, dbuf_len)
        ptr::copy_nonoverlapping(dbuf.cast::<u8>(), dst, dbuf_len);
        vaddr = vaddr.add(pitch0);
        dst = dst.add(dst_pitch);
    }
    0
}

// upstream: drm_format_helper.c drm_fb_xfrm()
unsafe fn drm_fb_xfrm(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    dst_pixsize: *const u8,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    vaddr_cached_hint: bool,
    state: *mut DrmFormatConvState,
    xfrm_line: XfrmLine,
) -> i32 {
    // default_dst_pitch[] is all zero: a NULL pitch means "tight packing".
    let pitch0 = if dst_pitch.is_null() {
        0
    } else {
        *dst_pitch as usize
    };
    let pixsize0 = *dst_pixsize as usize;

    // TODO (upstream): handle src in I/O memory here.
    if (*dst).is_iomem {
        __drm_fb_xfrm_toio(
            (*dst)._anon_0.vaddr_iomem,
            pitch0,
            pixsize0,
            (*src)._anon_0.vaddr,
            fb,
            clip,
            vaddr_cached_hint,
            state,
            xfrm_line,
        )
    } else {
        __drm_fb_xfrm(
            (*dst)._anon_0.vaddr,
            pitch0,
            pixsize0,
            (*src)._anon_0.vaddr,
            fb,
            clip,
            vaddr_cached_hint,
            state,
            xfrm_line,
        )
    }
}

// ---------------------------------------------------------------------------
// 32-bit source line helpers (drm_format_helper.c drm_fb_xfrm_line_32toN)
// ---------------------------------------------------------------------------

// upstream: drm_format_helper.c drm_fb_xfrm_line_32to8()
#[inline(always)]
unsafe fn drm_fb_xfrm_line_32to8(
    dbuf: *mut c_void,
    sbuf: *const c_void,
    pixels: u32,
    xfrm_pixel: fn(u32) -> u32,
) {
    let pixels = pixels as usize;
    let mut i = 0usize;
    // write 4 pixels at once; each pixel is one output byte
    while i + 4 <= pixels {
        let pix = [
            ld_le32_px(sbuf, i),
            ld_le32_px(sbuf, i + 1),
            ld_le32_px(sbuf, i + 2),
            ld_le32_px(sbuf, i + 3),
        ];
        let val32 = xfrm_pixel(pix[0])
            | (xfrm_pixel(pix[1]) << 8)
            | (xfrm_pixel(pix[2]) << 16)
            | (xfrm_pixel(pix[3]) << 24);
        st_u32(dbuf, i, val32);
        i += 4;
    }
    // trailing pixels
    while i < pixels {
        st_u8(dbuf, i, xfrm_pixel(ld_le32_px(sbuf, i)) as u8);
        i += 1;
    }
}

// upstream: drm_format_helper.c drm_fb_xfrm_line_32to16()  (CONFIG_64BIT path)
#[inline(always)]
unsafe fn drm_fb_xfrm_line_32to16(
    dbuf: *mut c_void,
    sbuf: *const c_void,
    pixels: u32,
    xfrm_pixel: fn(u32) -> u32,
) {
    let pixels = pixels as usize;
    let mut i = 0usize;
    // write 4 pixels at once
    while i + 4 <= pixels {
        let pix = [
            ld_le32_px(sbuf, i),
            ld_le32_px(sbuf, i + 1),
            ld_le32_px(sbuf, i + 2),
            ld_le32_px(sbuf, i + 3),
        ];
        let val64 = (xfrm_pixel(pix[0]) as u64)
            | ((xfrm_pixel(pix[1]) as u64) << 16)
            | ((xfrm_pixel(pix[2]) as u64) << 32)
            | ((xfrm_pixel(pix[3]) as u64) << 48);
        st_u64(dbuf, i * 2, val64);
        i += 4;
    }
    // write 2 pixels at once
    while i + 2 <= pixels {
        let pix = [ld_le32_px(sbuf, i), ld_le32_px(sbuf, i + 1)];
        let val32 = xfrm_pixel(pix[0]) | (xfrm_pixel(pix[1]) << 16);
        st_u32(dbuf, i * 2, val32);
        i += 2;
    }
    // trailing pixel
    while i < pixels {
        st_u16(dbuf, i * 2, xfrm_pixel(ld_le32_px(sbuf, i)) as u16);
        i += 1;
    }
}

// upstream: drm_format_helper.c drm_fb_xfrm_line_32to24()
#[inline(always)]
unsafe fn drm_fb_xfrm_line_32to24(
    dbuf: *mut c_void,
    sbuf: *const c_void,
    pixels: u32,
    xfrm_pixel: fn(u32) -> u32,
) {
    let pixels = pixels as usize;
    let mut i = 0usize;
    // write pixels in chunks of 4 (12 output bytes)
    while i + 4 <= pixels {
        let val24 = [
            xfrm_pixel(ld_le32_px(sbuf, i)),
            xfrm_pixel(ld_le32_px(sbuf, i + 1)),
            xfrm_pixel(ld_le32_px(sbuf, i + 2)),
            xfrm_pixel(ld_le32_px(sbuf, i + 3)),
        ];
        let out32 = [
            (val24[0] & 0x000000ff)
                | (val24[0] & 0x0000ff00)
                | (val24[0] & 0x00ff0000)
                | ((val24[1] & 0x000000ff) << 24),
            ((val24[1] & 0x0000ff00) >> 8)
                | ((val24[1] & 0x00ff0000) >> 8)
                | ((val24[2] & 0x000000ff) << 16)
                | ((val24[2] & 0x0000ff00) << 16),
            ((val24[2] & 0x00ff0000) >> 16)
                | ((val24[3] & 0x000000ff) << 8)
                | ((val24[3] & 0x0000ff00) << 8)
                | ((val24[3] & 0x00ff0000) << 8),
        ];
        st_u32(dbuf, i * 3, out32[0]);
        st_u32(dbuf, i * 3 + 4, out32[1]);
        st_u32(dbuf, i * 3 + 8, out32[2]);
        i += 4;
    }
    // trailing pixels, three bytes each, little-endian
    while i < pixels {
        let val24 = xfrm_pixel(ld_le32_px(sbuf, i));
        st_u8(dbuf, i * 3, (val24 & 0x000000ff) as u8);
        st_u8(dbuf, i * 3 + 1, ((val24 & 0x0000ff00) >> 8) as u8);
        st_u8(dbuf, i * 3 + 2, ((val24 & 0x00ff0000) >> 16) as u8);
        i += 1;
    }
}

// upstream: drm_format_helper.c drm_fb_xfrm_line_32to32()
#[inline(always)]
unsafe fn drm_fb_xfrm_line_32to32(
    dbuf: *mut c_void,
    sbuf: *const c_void,
    pixels: u32,
    xfrm_pixel: fn(u32) -> u32,
) {
    for i in 0..pixels as usize {
        st_u32(dbuf, i * 4, xfrm_pixel(ld_le32_px(sbuf, i)));
    }
}

// ---------------------------------------------------------------------------
// drm_format_internal.h pixel helpers (XRGB8888 source)
// ---------------------------------------------------------------------------

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_rgb332()
#[inline]
fn drm_pixel_xrgb8888_to_rgb332(pix: u32) -> u32 {
    ((pix & 0x00e00000) >> 16) | ((pix & 0x0000e000) >> 11) | ((pix & 0x000000c0) >> 6)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_rgb565()
#[inline]
fn drm_pixel_xrgb8888_to_rgb565(pix: u32) -> u32 {
    ((pix & 0x00f80000) >> 8) | ((pix & 0x0000fc00) >> 5) | ((pix & 0x000000f8) >> 3)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_rgb565be()
#[inline]
fn drm_pixel_xrgb8888_to_rgb565be(pix: u32) -> u32 {
    // swab16() on the 16-bit RGB565 value
    (drm_pixel_xrgb8888_to_rgb565(pix) as u16).swap_bytes() as u32
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_rgbx5551()
#[inline]
fn drm_pixel_xrgb8888_to_rgbx5551(pix: u32) -> u32 {
    ((pix & 0x00f80000) >> 8) | ((pix & 0x0000f800) >> 5) | ((pix & 0x000000f8) >> 2)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_rgba5551()
#[inline]
fn drm_pixel_xrgb8888_to_rgba5551(pix: u32) -> u32 {
    drm_pixel_xrgb8888_to_rgbx5551(pix) | (1 << 0) // set alpha bit
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_xrgb1555()
#[inline]
fn drm_pixel_xrgb8888_to_xrgb1555(pix: u32) -> u32 {
    ((pix & 0x00f80000) >> 9) | ((pix & 0x0000f800) >> 6) | ((pix & 0x000000f8) >> 3)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_argb1555()
#[inline]
fn drm_pixel_xrgb8888_to_argb1555(pix: u32) -> u32 {
    (1 << 15) | drm_pixel_xrgb8888_to_xrgb1555(pix) // set alpha bit
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_rgb888()
#[inline]
fn drm_pixel_xrgb8888_to_rgb888(pix: u32) -> u32 {
    pix & 0x00ff_ffff // GENMASK(23, 0)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_bgr888()
#[inline]
fn drm_pixel_xrgb8888_to_bgr888(pix: u32) -> u32 {
    ((pix & 0x00ff0000) >> 16) | (pix & 0x0000ff00) | ((pix & 0x000000ff) << 16)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_argb8888()
#[inline]
fn drm_pixel_xrgb8888_to_argb8888(pix: u32) -> u32 {
    0xff00_0000 | pix // GENMASK(31, 24): fill alpha bits
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_xbgr8888()
#[inline]
fn drm_pixel_xrgb8888_to_xbgr8888(pix: u32) -> u32 {
    (pix & 0xff00_0000)
        | ((pix & 0x00ff0000) >> 16)
        | (pix & 0x0000ff00)
        | ((pix & 0x000000ff) << 16)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_abgr8888()
#[inline]
fn drm_pixel_xrgb8888_to_abgr8888(pix: u32) -> u32 {
    0xff00_0000 | drm_pixel_xrgb8888_to_xbgr8888(pix)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_bgrx8888()
#[inline]
fn drm_pixel_xrgb8888_to_bgrx8888(pix: u32) -> u32 {
    ((pix & 0xff000000) >> 24)
        | ((pix & 0x00ff0000) >> 8)
        | ((pix & 0x0000ff00) << 8)
        | ((pix & 0x000000ff) << 24)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_xrgb2101010()
#[inline]
fn drm_pixel_xrgb8888_to_xrgb2101010(pix: u32) -> u32 {
    let pix = ((pix & 0x000000ff) << 2) | ((pix & 0x0000ff00) << 4) | ((pix & 0x00ff0000) << 6);
    pix | ((pix >> 8) & 0x00300c03)
}

// upstream: drm_format_internal.h drm_pixel_xrgb8888_to_argb2101010()
#[inline]
fn drm_pixel_xrgb8888_to_argb2101010(pix: u32) -> u32 {
    0xc000_0000 | drm_pixel_xrgb8888_to_xrgb2101010(pix) // GENMASK(31, 30)
}

// ---------------------------------------------------------------------------
// Line converters and exported XRGB8888 -> format entry points
// ---------------------------------------------------------------------------

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgb332_line()
unsafe fn drm_fb_xrgb8888_to_rgb332_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to8(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_rgb332);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgb332()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_rgb332(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [1, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_rgb332_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgb565_line()
unsafe fn drm_fb_xrgb8888_to_rgb565_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to16(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_rgb565);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgb565()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_rgb565(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [2, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_rgb565_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgb565be_line()
unsafe fn drm_fb_xrgb8888_to_rgb565be_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to16(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_rgb565be);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgb565be()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_rgb565be(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [2, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_rgb565be_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_xrgb1555_line()
unsafe fn drm_fb_xrgb8888_to_xrgb1555_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to16(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_xrgb1555);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_xrgb1555()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_xrgb1555(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [2, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_xrgb1555_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_argb1555_line()
unsafe fn drm_fb_xrgb8888_to_argb1555_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to16(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_argb1555);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_argb1555()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_argb1555(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [2, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_argb1555_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgba5551_line()
unsafe fn drm_fb_xrgb8888_to_rgba5551_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to16(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_rgba5551);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgba5551()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_rgba5551(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [2, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_rgba5551_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgb888_line()
unsafe fn drm_fb_xrgb8888_to_rgb888_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to24(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_rgb888);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_rgb888()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_rgb888(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [3, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_rgb888_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_bgr888_line()
unsafe fn drm_fb_xrgb8888_to_bgr888_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to24(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_bgr888);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_bgr888()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_bgr888(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [3, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_bgr888_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_argb8888_line()
unsafe fn drm_fb_xrgb8888_to_argb8888_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to32(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_argb8888);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_argb8888()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_argb8888(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [4, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_argb8888_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_abgr8888_line()
unsafe fn drm_fb_xrgb8888_to_abgr8888_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to32(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_abgr8888);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_abgr8888()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_abgr8888(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [4, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_abgr8888_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_xbgr8888_line()
unsafe fn drm_fb_xrgb8888_to_xbgr8888_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to32(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_xbgr8888);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_xbgr8888()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_xbgr8888(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [4, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_xbgr8888_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_bgrx8888_line()
unsafe fn drm_fb_xrgb8888_to_bgrx8888_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to32(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_bgrx8888);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_bgrx8888()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_bgrx8888(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [4, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_bgrx8888_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_xrgb2101010_line()
unsafe fn drm_fb_xrgb8888_to_xrgb2101010_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to32(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_xrgb2101010);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_xrgb2101010()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_xrgb2101010(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [4, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_xrgb2101010_line,
    );
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_argb2101010_line()
unsafe fn drm_fb_xrgb8888_to_argb2101010_line(dbuf: *mut c_void, sbuf: *const c_void, pixels: u32) {
    drm_fb_xfrm_line_32to32(dbuf, sbuf, pixels, drm_pixel_xrgb8888_to_argb2101010);
}

// upstream: drm_format_helper.c drm_fb_xrgb8888_to_argb2101010()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_xrgb8888_to_argb2101010(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
    state: *mut DrmFormatConvState,
) {
    static DST_PIXSIZE: [u8; DRM_FORMAT_MAX_PLANES] = [4, 0, 0, 0];
    drm_fb_xfrm(
        dst,
        dst_pitch,
        DST_PIXSIZE.as_ptr(),
        src,
        fb,
        clip,
        false,
        state,
        drm_fb_xrgb8888_to_argb2101010_line,
    );
}

// ---------------------------------------------------------------------------
// drm_fb_memcpy()
// ---------------------------------------------------------------------------

// upstream: include/linux/iosys-map.h iosys_map_memcpy_to()
unsafe fn iosys_map_memcpy_to(
    dst: *mut IosysMap,
    dst_offset: usize,
    src: *const c_void,
    len: usize,
) {
    let base = if (*dst).is_iomem {
        (*dst)._anon_0.vaddr_iomem
    } else {
        (*dst)._anon_0.vaddr
    };
    ptr::copy_nonoverlapping(src.cast::<u8>(), base.cast::<u8>().add(dst_offset), len);
}

// upstream: drm_format_helper.c drm_fb_memcpy()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn drm_fb_memcpy(
    dst: *mut IosysMap,
    dst_pitch: *const u32,
    src: *const IosysMap,
    fb: *const DrmFramebuffer,
    clip: *const DrmRect,
) {
    let format = (*fb).format;
    let lines = drm_rect_height(&*clip) as u32;

    for i in 0..(*format).num_planes as usize {
        let bpp_i = drm_format_info_bpp(format, i as i32);
        let cpp_i = bpp_i.div_ceil(8);
        let len_i = (drm_rect_width(&*clip) as u32)
            .wrapping_mul(bpp_i)
            .div_ceil(8) as usize;
        let mut dst_pitch_i = if dst_pitch.is_null() {
            0
        } else {
            *dst_pitch.add(i)
        };
        let mut dst_i: IosysMap = ptr::read(dst.add(i));
        let mut src_i: IosysMap = ptr::read(src.add(i));

        if dst_pitch_i == 0 {
            dst_pitch_i = len_i as u32;
        }

        iosys_map_incr(
            &mut src_i,
            clip_offset(&*clip, (*fb).pitches[i], cpp_i) as usize,
        );
        for _ in 0..lines {
            // TODO (upstream): handle src_i in I/O memory here.
            iosys_map_memcpy_to(&mut dst_i, 0, src_i._anon_0.vaddr, len_i);
            iosys_map_incr(&mut src_i, (*fb).pitches[i] as usize);
            iosys_map_incr(&mut dst_i, dst_pitch_i as usize);
        }
    }
}
