// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Linux v7.2.3 drivers/video/fbdev/core/fb_sys_fops.c and sys{fillrect,
// copyarea,imgblt}.c service for x86_64 little-endian system-memory framebuffers.
//! Generic framebuffer read/write and packed-pixel drawing operations.
#![allow(unsafe_code, non_snake_case)]

use core::ffi::{c_char, c_long, c_void};

use tk_linux_kapi::{
    config::{EFAULT, ENODEV, ENOSPC},
    mm_native::{copy_from_user, copy_to_user},
};

use crate::fb_types::{FbCopyArea, FbFillRect, FbImage, FbInfo};

const EFBIG: isize = 27;
const EINVAL: isize = 22;
const ROP_COPY: u32 = 0;
const ROP_XOR: u32 = 1;
const FB_VISUAL_TRUECOLOR: u32 = 2;
const FB_VISUAL_DIRECTCOLOR: u32 = 4;

unsafe fn framebuffer_len(info: *const FbInfo) -> usize {
    let screen = unsafe { (*info).screen_size } as usize;
    if screen != 0 {
        screen
    } else {
        unsafe { (*info).fix.smem_len as usize }
    }
}

unsafe fn framebuffer_base(info: *mut FbInfo) -> *mut u8 {
    unsafe { (*info).screen_base.cast::<u8>() }
}

unsafe fn sync(info: *mut FbInfo) {
    if info.is_null() || unsafe { (*info).fbops.is_null() } {
        return;
    }
    if let Some(sync) = unsafe { (*(*info).fbops).fb_sync } {
        unsafe { sync(info) };
    }
}

/// Linux `fb_sys_read()` over a system-memory framebuffer.
///
/// # Safety
/// `info`, user `buf`, and `ppos` must be valid for the requested operation;
/// the framebuffer backing memory must remain live.
#[unsafe(export_name = "fb_sys_read")]
pub unsafe extern "C" fn fb_sys_read(
    info: *mut FbInfo,
    buf: *mut c_char,
    count: usize,
    ppos: *mut c_long,
) -> isize {
    if info.is_null() || ppos.is_null() {
        return -EINVAL;
    }
    let base = unsafe { framebuffer_base(info) };
    if base.is_null() {
        return -(ENODEV as isize);
    }

    let total = unsafe { framebuffer_len(info) };
    let position = unsafe { *ppos as usize };
    if position >= total {
        return 0;
    }
    let count = count.min(total).min(total - position);
    unsafe { sync(info) };

    let uncopied = unsafe {
        copy_to_user(
            buf.cast::<c_void>(),
            base.add(position).cast::<c_void>(),
            count,
        )
    };
    let copied = count.saturating_sub(uncopied);
    unsafe { *ppos = (*ppos).wrapping_add(copied as c_long) };
    if copied != 0 {
        copied as isize
    } else if uncopied != 0 {
        -(EFAULT as isize)
    } else {
        0
    }
}

/// Linux `fb_sys_write()` over a system-memory framebuffer. The DRM deferred
/// wrappers call `drm_fb_helper_damage_range()` after this operation; damage
/// notification is intentionally not duplicated here.
///
/// # Safety
/// `info`, user `buf`, and `ppos` must be valid for the requested operation;
/// the framebuffer backing memory must remain live.
#[unsafe(export_name = "fb_sys_write")]
pub unsafe extern "C" fn fb_sys_write(
    info: *mut FbInfo,
    buf: *const c_char,
    count: usize,
    ppos: *mut c_long,
) -> isize {
    if info.is_null() || ppos.is_null() {
        return -EINVAL;
    }
    let base = unsafe { framebuffer_base(info) };
    if base.is_null() {
        return -(ENODEV as isize);
    }

    let total = unsafe { framebuffer_len(info) };
    let position = unsafe { *ppos as usize };
    if position > total {
        return -EFBIG;
    }
    let mut error = 0;
    let mut count = count;
    if count > total {
        error = -EFBIG;
        count = total;
    }
    if count > total.saturating_sub(position) {
        if error == 0 {
            error = -(ENOSPC as isize);
        }
        count = total - position;
    }

    unsafe { sync(info) };
    let uncopied = unsafe {
        copy_from_user(
            base.add(position).cast::<c_void>(),
            buf.cast::<c_void>(),
            count,
        )
    };
    if uncopied != 0 {
        error = -(EFAULT as isize);
    }
    let copied = count.saturating_sub(uncopied);
    unsafe { *ppos = (*ppos).wrapping_add(copied as c_long) };
    if copied != 0 { copied as isize } else { error }
}

unsafe fn fb_color(info: *mut FbInfo, index: u32) -> u64 {
    let visual = unsafe { (*info).fix.visual };
    if visual == FB_VISUAL_TRUECOLOR || visual == FB_VISUAL_DIRECTCOLOR {
        let palette = unsafe { (*info).pseudo_palette.cast::<u32>() };
        if !palette.is_null() && index < 16 {
            return unsafe { *palette.add(index as usize) as u64 };
        }
    }
    index as u64
}

fn color_mask(bits_per_pixel: u32) -> u64 {
    match bits_per_pixel {
        0 => 0,
        1..=63 => (1u64 << bits_per_pixel) - 1,
        _ => u64::MAX,
    }
}

unsafe fn pixel_bit_offset(info: *const FbInfo, x: u32, y: u32) -> Option<usize> {
    let bpp = unsafe { (*info).var.bits_per_pixel as usize };
    if bpp == 0 || bpp > 64 {
        return None;
    }
    let line = unsafe { (*info).fix.line_length as usize };
    let row = (y as usize).checked_mul(line)?.checked_mul(8)?;
    let pixel = (x as usize).checked_mul(bpp)?;
    row.checked_add(pixel)
}

unsafe fn read_pixel(info: *mut FbInfo, x: u32, y: u32) -> Option<u64> {
    let base = unsafe { framebuffer_base(info) };
    if base.is_null() {
        return None;
    }
    let bpp = unsafe { (*info).var.bits_per_pixel as usize };
    let start = unsafe { pixel_bit_offset(info, x, y) }?;
    let memory_bits = unsafe { framebuffer_len(info) }.checked_mul(8)?;
    if start.checked_add(bpp)? > memory_bits {
        return None;
    }
    let mut pixel = 0u64;
    for bit in 0..bpp {
        let absolute = start + bit;
        let byte = unsafe { *base.add(absolute / 8) };
        pixel |= (((byte >> (absolute & 7)) & 1) as u64) << bit;
    }
    Some(pixel)
}

unsafe fn write_pixel(info: *mut FbInfo, x: u32, y: u32, color: u64, xor: bool) {
    let base = unsafe { framebuffer_base(info) };
    if base.is_null() {
        return;
    }
    let bpp = unsafe { (*info).var.bits_per_pixel as usize };
    let Some(start) = (unsafe { pixel_bit_offset(info, x, y) }) else {
        return;
    };
    let Some(memory_bits) = (unsafe { framebuffer_len(info) }).checked_mul(8) else {
        return;
    };
    if start.checked_add(bpp).is_none_or(|end| end > memory_bits) {
        return;
    }
    let color = color & color_mask(bpp as u32);
    for bit in 0..bpp {
        if xor && color & (1u64 << bit) == 0 {
            continue;
        }
        let absolute = start + bit;
        let byte = unsafe { base.add(absolute / 8) };
        let mask = 1u8 << (absolute & 7);
        unsafe {
            if xor {
                *byte ^= mask;
            } else if color & (1u64 << bit) != 0 {
                *byte |= mask;
            } else {
                *byte &= !mask;
            }
        }
    }
}

fn clipped_size(info: *mut FbInfo, x: u32, y: u32, width: u32, height: u32) -> (u32, u32) {
    let (xres, yres) = unsafe { ((*info).var.xres, (*info).var.yres) };
    if x >= xres || y >= yres {
        return (0, 0);
    }
    (width.min(xres - x), height.min(yres - y))
}

/// Linux sysmem rectangle fill for packed pixels from 1 through 64 bpp.
#[unsafe(export_name = "sys_fillrect")]
pub unsafe extern "C" fn sys_fillrect(info: *mut FbInfo, rect: *const FbFillRect) {
    if info.is_null() || rect.is_null() {
        return;
    }
    let (dx, dy, width, height, color, rop) = unsafe {
        ((*rect).dx, (*rect).dy, (*rect).width, (*rect).height, (*rect).color, (*rect).rop)
    };
    let (width, height) = clipped_size(info, dx, dy, width, height);
    if width == 0 || height == 0 {
        return;
    }
    let color = unsafe { fb_color(info, color) };
    if rop != ROP_COPY && rop != ROP_XOR {
        return;
    }
    for y in 0..height {
        for x in 0..width {
            unsafe { write_pixel(info, dx + x, dy + y, color, rop == ROP_XOR) };
        }
    }
}

/// Linux sysmem overlapping rectangle copy for packed pixels from 1 through
/// 64 bpp. Traversal direction follows `memmove` semantics for overlapping
/// source and destination rectangles.
#[unsafe(export_name = "sys_copyarea")]
pub unsafe extern "C" fn sys_copyarea(info: *mut FbInfo, area: *const FbCopyArea) {
    if info.is_null() || area.is_null() {
        return;
    }
    let (sx, sy, dx, dy, width, height) = unsafe {
        ((*area).sx, (*area).sy, (*area).dx, (*area).dy, (*area).width, (*area).height)
    };
    let (src_w, src_h) = clipped_size(info, sx, sy, width, height);
    let (dst_w, dst_h) = clipped_size(info, dx, dy, width, height);
    let width = src_w.min(dst_w);
    let height = src_h.min(dst_h);
    if width == 0 || height == 0 {
        return;
    }
    let reverse_y = dy > sy && dy < sy.saturating_add(height);
    let reverse_x = dy == sy && dx > sx && dx < sx.saturating_add(width);
    for row_step in 0..height {
        let row = if reverse_y { height - 1 - row_step } else { row_step };
        if reverse_x {
            for col_step in 0..width {
                let col = width - 1 - col_step;
                if let Some(pixel) = unsafe { read_pixel(info, sx + col, sy + row) } {
                    unsafe { write_pixel(info, dx + col, dy + row, pixel, false) };
                }
            }
        } else {
            for col in 0..width {
                if let Some(pixel) = unsafe { read_pixel(info, sx + col, sy + row) } {
                    unsafe { write_pixel(info, dx + col, dy + row, pixel, false) };
                }
            }
        }
    }
}

/// Linux sysmem image blit. Monochrome source bits are MSB-first, while color
/// images use one 8-bit palette/index value per pixel as in fb_imageblit.
#[unsafe(export_name = "sys_imageblit")]
pub unsafe extern "C" fn sys_imageblit(info: *mut FbInfo, image: *const FbImage) {
    if info.is_null() || image.is_null() || unsafe { (*image).data.is_null() } {
        return;
    }
    let (dx, dy, width, height, fg, bg, depth, data) = unsafe {
        (
            (*image).dx,
            (*image).dy,
            (*image).width,
            (*image).height,
            (*image).fg_color,
            (*image).bg_color,
            (*image).depth,
            (*image).data.cast::<u8>(),
        )
    };
    let (draw_width, draw_height) = clipped_size(info, dx, dy, width, height);
    if draw_width == 0 || draw_height == 0 {
        return;
    }
    if depth == 1 {
        let stride = (width as usize).saturating_add(7) / 8;
        let fg = unsafe { fb_color(info, fg) };
        let bg = unsafe { fb_color(info, bg) };
        for y in 0..draw_height as usize {
            for x in 0..draw_width as usize {
                let source = unsafe { *data.add(y * stride + x / 8) };
                let selected = (source >> (7 - (x & 7))) & 1 != 0;
                unsafe { write_pixel(info, dx + x as u32, dy + y as u32, if selected { fg } else { bg }, false) };
            }
        }
    } else if depth != 0 {
        let stride = width as usize;
        for y in 0..draw_height as usize {
            for x in 0..draw_width as usize {
                let index = unsafe { *data.add(y * stride + x) } as u32;
                unsafe { write_pixel(info, dx + x as u32, dy + y as u32, fb_color(info, index), false) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use core::mem::MaybeUninit;
    use core::ptr;
    use crate::fb_types::FbCmap;

    unsafe fn test_info<'a>(buffer: &'a mut [u8], bpp: u32, width: u32, height: u32) -> *mut FbInfo {
        let mut info = Box::new(unsafe { MaybeUninit::<FbInfo>::zeroed().assume_init() });
        info.var.bits_per_pixel = bpp;
        info.var.xres = width;
        info.var.yres = height;
        info.fix.line_length = (width * bpp).div_ceil(8);
        info.screen_size = buffer.len() as u64;
        info.screen_base = buffer.as_mut_ptr().cast::<c_char>();
        Box::into_raw(info)
    }

    #[test]
    fn fill_rect_writes_little_endian_packed_pixels_and_clips() {
        let mut buffer = [0u8; 12];
        let info = unsafe { test_info(&mut buffer, 24, 3, 1) };
        let rect = FbFillRect { dx: 1, dy: 0, width: 8, height: 1, color: 0x00a1_b2c3, rop: ROP_COPY };
        unsafe { sys_fillrect(info, &rect) };
        assert_eq!(&buffer[..3], &[0, 0, 0]);
        assert_eq!(&buffer[3..9], &[0xc3, 0xb2, 0xa1, 0xc3, 0xb2, 0xa1]);
        unsafe { drop(Box::from_raw(info)) };
    }

    #[test]
    fn copyarea_preserves_overlapping_memmove_order() {
        let mut buffer = [1u8, 2, 3, 4, 5, 6];
        let info = unsafe { test_info(&mut buffer, 8, 6, 1) };
        let area = FbCopyArea { sx: 0, sy: 0, dx: 1, dy: 0, width: 5, height: 1 };
        unsafe { sys_copyarea(info, &area) };
        assert_eq!(buffer, [1, 1, 2, 3, 4, 5]);
        unsafe { drop(Box::from_raw(info)) };
    }

    #[test]
    fn imageblit_reads_msb_first_monochrome_source() {
        let mut buffer = [0u8; 8];
        let data = [0b1010_0000u8];
        let info = unsafe { test_info(&mut buffer, 8, 4, 1) };
        let image = FbImage {
            dx: 0, dy: 0, width: 4, height: 1, fg_color: 0xff, bg_color: 0x11,
            depth: 1, data: data.as_ptr().cast(),
            cmap: FbCmap { start: 0, len: 0, red: ptr::null_mut(), green: ptr::null_mut(), blue: ptr::null_mut(), transp: ptr::null_mut() },
        };
        unsafe { sys_imageblit(info, &image) };
        assert_eq!(&buffer[..4], &[0xff, 0x11, 0xff, 0x11]);
        unsafe { drop(Box::from_raw(info)) };
    }
}
