// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/video/fbdev/core/cfbcopyarea.c and fb_copyarea.h (Linux v7.2.3).
//! `cfb_copyarea()`: packed-pixel area copy for I/O-memory framebuffers.
//! The reversed-pixel/byte variants of the template are not reachable on the
//! translated configuration (see `fb_draw_upstream`).
#![allow(unsafe_code, non_snake_case, dead_code)]

use crate::{
    fb_draw_upstream::{
        BITS_PER_BYTE, BITS_PER_LONG, FbAddress, cfb_entry_running, cfb_is_virtfb, cfb_sync,
        fb_address_backward, fb_address_forward, fb_address_init, fb_address_move_long, fb_left,
        fb_modify_offset, fb_pixel_mask, fb_read_offset, fb_right, fb_write_offset,
        warn_virtfb_once,
    },
    fb_types::{FbCopyArea, FbInfo},
};

/// `fb_copy_offset_masked()`.
#[inline]
unsafe fn fb_copy_offset_masked(mask: u64, offset: i32, dst: &FbAddress, src: &FbAddress) {
    unsafe { fb_modify_offset(fb_read_offset(offset, src), mask, offset, dst) };
}

/// `fb_copy_offset()`.
#[inline]
unsafe fn fb_copy_offset(offset: i32, dst: &FbAddress, src: &FbAddress) {
    unsafe { fb_write_offset(fb_read_offset(offset, src), offset, dst) };
}

/// `fb_copy_aligned_fwd()`: source and destination share a bit phase.
unsafe fn fb_copy_aligned_fwd(dst: &FbAddress, src: &FbAddress, mut end: i32) {
    unsafe {
        let first = fb_pixel_mask(dst.bits);
        let mut last = !fb_pixel_mask(end & (BITS_PER_LONG - 1));

        if end <= BITS_PER_LONG {
            last = if last != 0 { last & first } else { first };
            if last == !0u64 {
                fb_copy_offset(0, dst, src);
            } else {
                fb_copy_offset_masked(last, 0, dst, src);
            }
        } else {
            let mut offset = (first != !0u64) as i32;
            if offset != 0 {
                fb_copy_offset_masked(first, 0, dst, src);
            }
            end /= BITS_PER_LONG;
            while offset + 4 <= end {
                fb_copy_offset(offset, dst, src);
                fb_copy_offset(offset + 1, dst, src);
                fb_copy_offset(offset + 2, dst, src);
                fb_copy_offset(offset + 3, dst, src);
                offset += 4;
            }
            while offset < end {
                fb_copy_offset(offset, dst, src);
                offset += 1;
            }
            if last != 0 {
                fb_copy_offset_masked(last, offset, dst, src);
            }
        }
    }
}

/// `fb_copy_aligned_rev()`: as above, copying from the high end downwards.
unsafe fn fb_copy_aligned_rev(dst: &FbAddress, src: &FbAddress, mut end: i32) {
    unsafe {
        let mut first = fb_pixel_mask(dst.bits);
        let last = !fb_pixel_mask(end & (BITS_PER_LONG - 1));

        if end <= BITS_PER_LONG {
            if last != 0 {
                first &= last;
            }
            if first == !0u64 {
                fb_copy_offset(0, dst, src);
            } else {
                fb_copy_offset_masked(first, 0, dst, src);
            }
        } else {
            let offset = (first != !0u64) as i32;
            end /= BITS_PER_LONG;
            if last != 0 {
                fb_copy_offset_masked(last, end, dst, src);
            }
            while end >= offset + 4 {
                fb_copy_offset(end - 1, dst, src);
                fb_copy_offset(end - 2, dst, src);
                fb_copy_offset(end - 3, dst, src);
                fb_copy_offset(end - 4, dst, src);
                end -= 4;
            }
            while end > offset {
                end -= 1;
                fb_copy_offset(end, dst, src);
            }
            if offset != 0 {
                fb_copy_offset_masked(first, 0, dst, src);
            }
        }
    }
}

/// `fb_copy_aligned()`.
unsafe fn fb_copy_aligned(
    dst: &mut FbAddress,
    src: &mut FbAddress,
    width: i32,
    mut height: u32,
    bits_per_line: u32,
    rev_copy: bool,
) {
    unsafe {
        if rev_copy {
            while height != 0 {
                height -= 1;
                fb_copy_aligned_rev(dst, src, width + dst.bits);
                fb_address_backward(dst, bits_per_line);
                fb_address_backward(src, bits_per_line);
            }
        } else {
            while height != 0 {
                height -= 1;
                fb_copy_aligned_fwd(dst, src, width + dst.bits);
                fb_address_forward(dst, bits_per_line);
                fb_address_forward(src, bits_per_line);
            }
        }
    }
}

/// `fb_copy_fwd()`: copy a scanline with the source shifted by `shift` bits.
unsafe fn fb_copy_fwd(dst: &FbAddress, src: &FbAddress, width: i32) {
    unsafe {
        let first = fb_pixel_mask(dst.bits);
        let end = dst.bits + width;
        let mut last = !fb_pixel_mask(end & (BITS_PER_LONG - 1));
        let shift = dst.bits - src.bits;
        let right = shift & (BITS_PER_LONG - 1);
        let left = shift.wrapping_neg() & (BITS_PER_LONG - 1);

        if end <= BITS_PER_LONG {
            last = if last != 0 { last & first } else { first };
            if shift < 0 {
                let mut d0 = fb_left(fb_read_offset(-1, src), left);
                if src.bits + width > BITS_PER_LONG {
                    d0 |= fb_right(fb_read_offset(0, src), right);
                }
                if last == !0u64 {
                    fb_write_offset(d0, 0, dst);
                } else {
                    fb_modify_offset(d0, last, 0, dst);
                }
            } else {
                let d0 = fb_right(fb_read_offset(0, src), right);
                fb_modify_offset(d0, last, 0, dst);
            }
        } else {
            let mut offset = (first != !0u64) as i32;
            let mut d0: u64 = if shift < 0 {
                fb_read_offset(-1, src)
            } else {
                0
            };
            if offset != 0 {
                let d1 = fb_read_offset(0, src);
                d0 = fb_left(d0, left) | fb_right(d1, right);
                fb_modify_offset(d0, first, 0, dst);
                d0 = d1;
            }

            let end = end / BITS_PER_LONG;
            while offset < end {
                let d1 = fb_read_offset(offset, src);
                d0 = fb_left(d0, left) | fb_right(d1, right);
                fb_write_offset(d0, offset, dst);
                d0 = d1;
                offset += 1;
            }

            if last != 0 {
                d0 = fb_left(d0, left);
                if src.bits + width
                    > offset * BITS_PER_LONG + if shift < 0 { BITS_PER_LONG } else { 0 }
                {
                    d0 |= fb_right(fb_read_offset(offset, src), right);
                }
                fb_modify_offset(d0, last, offset, dst);
            }
        }
    }
}

/// `fb_copy_rev()`: copy a scanline from the high end downwards.
unsafe fn fb_copy_rev(dst: &FbAddress, src: &FbAddress, mut end: i32) {
    unsafe {
        let mut first = fb_pixel_mask(dst.bits);
        let last = !fb_pixel_mask(end & (BITS_PER_LONG - 1));
        let shift = dst.bits - src.bits;
        let right = shift & (BITS_PER_LONG - 1);
        let left = shift.wrapping_neg() & (BITS_PER_LONG - 1);

        if end <= BITS_PER_LONG {
            if last != 0 {
                first &= last;
            }
            if shift > 0 {
                let mut d0 = fb_right(fb_read_offset(1, src), right);
                if src.bits > left {
                    d0 |= fb_left(fb_read_offset(0, src), left);
                }
                fb_modify_offset(d0, first, 0, dst);
            } else {
                let mut d0 = fb_left(fb_read_offset(0, src), left);
                if src.bits + end - dst.bits > BITS_PER_LONG {
                    d0 |= fb_right(fb_read_offset(1, src), right);
                }
                if first == !0u64 {
                    fb_write_offset(d0, 0, dst);
                } else {
                    fb_modify_offset(d0, first, 0, dst);
                }
            }
        } else {
            let offset = (first != !0u64) as i32;
            end /= BITS_PER_LONG;

            let mut d0: u64 = if fb_right(!0u64, right) & last != 0 {
                fb_right(fb_read_offset(end + 1, src), right)
            } else {
                0
            };
            let d1 = fb_read_offset(end, src);
            if last != 0 {
                fb_modify_offset(fb_left(d1, left) | d0, last, end, dst);
            }
            d0 = d1;

            while end > offset {
                end -= 1;
                let d1 = fb_read_offset(end, src);
                d0 = fb_left(d1, left) | fb_right(d0, right);
                fb_write_offset(d0, end, dst);
                d0 = d1;
            }

            if offset != 0 {
                d0 = fb_right(d0, right);
                if src.bits > left {
                    d0 |= fb_left(fb_read_offset(0, src), left);
                }
                fb_modify_offset(d0, first, 0, dst);
            }
        }
    }
}

/// `fb_copy()`: unaligned copy of `height` scanlines.
unsafe fn fb_copy(
    dst: &mut FbAddress,
    src: &mut FbAddress,
    width: i32,
    mut height: u32,
    bits_per_line: u32,
    rev_copy: bool,
) {
    unsafe {
        if rev_copy {
            while height != 0 {
                height -= 1;
                let mv = if src.bits < dst.bits { -1 } else { 0 };
                fb_address_move_long(src, mv);
                fb_copy_rev(dst, src, width + dst.bits);
                fb_address_backward(dst, bits_per_line);
                fb_address_backward(src, bits_per_line);
                fb_address_move_long(src, -mv);
            }
        } else {
            while height != 0 {
                height -= 1;
                let mv = if src.bits > dst.bits { 1 } else { 0 };
                fb_address_move_long(src, mv);
                fb_copy_fwd(dst, src, width);
                fb_address_forward(dst, bits_per_line);
                fb_address_forward(src, bits_per_line);
                fb_address_move_long(src, -mv);
            }
        }
    }
}

/// `fb_copyarea()`.
unsafe fn fb_copyarea(p: *mut FbInfo, area: &FbCopyArea) {
    unsafe {
        let bpp = (*p).var.bits_per_pixel as i32;
        let mut dy = area.dy;
        let mut sy = area.sy;
        let height = area.height;
        let width = area.width.wrapping_mul(bpp as u32) as i32;
        let bits_per_line = (*p).fix.line_length * BITS_PER_BYTE;

        let mut dst = fb_address_init(p);
        let mut src = dst;

        let rev_copy = (dy > sy) || (dy == sy && area.dx > area.sx);
        if rev_copy {
            dy = dy.wrapping_add(height.wrapping_sub(1));
            sy = sy.wrapping_add(height.wrapping_sub(1));
        }

        fb_address_forward(
            &mut dst,
            dy.wrapping_mul(bits_per_line)
                .wrapping_add(area.dx.wrapping_mul(bpp as u32)),
        );
        fb_address_forward(
            &mut src,
            sy.wrapping_mul(bits_per_line)
                .wrapping_add(area.sx.wrapping_mul(bpp as u32)),
        );

        if src.bits == dst.bits {
            fb_copy_aligned(&mut dst, &mut src, width, height, bits_per_line, rev_copy);
        } else {
            fb_copy(&mut dst, &mut src, width, height, bits_per_line, rev_copy);
        }
    }
}

/// `cfb_copyarea()`: copy an area within an I/O-memory framebuffer.
///
/// # Safety
/// `p` must be a live framebuffer whose `screen_base` is valid for its
/// `screen_size`, and `area` must be readable.
// upstream: cfbcopyarea.c cfb_copyarea()
#[unsafe(export_name = "cfb_copyarea")]
pub unsafe extern "C" fn cfb_copyarea(p: *mut FbInfo, area: *const FbCopyArea) {
    unsafe {
        if !cfb_entry_running(p) {
            return;
        }
        if cfb_is_virtfb(p) {
            warn_virtfb_once();
        }
        cfb_sync(p);
        fb_copyarea(p, &*area);
    }
}
