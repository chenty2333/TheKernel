// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
// Source: drivers/video/fbdev/core/cfbfillrect.c and fb_fillrect.h (Linux v7.2.3).
//! `cfb_fillrect()`: packed-pixel rectangle fill for I/O-memory framebuffers.
#![allow(unsafe_code, non_snake_case, dead_code)]

use crate::{
    fb_draw_upstream::{
        BITS_PER_LONG, FbAddress, ROP_XOR, cfb_entry_running, cfb_is_virtfb, cfb_sync,
        fb_address_forward, fb_address_init, fb_invert_offset, fb_left, fb_modify_offset,
        fb_palette, fb_pixel_mask, fb_right, fb_write_offset, warn_virtfb_once,
    },
    fb_types::{FbFillRect, FbInfo},
};

/// `struct fb_pattern` without reversal: the rotating fill pattern.
struct FbPattern {
    pixels: u64,
    left: i32,
    right: i32,
}

#[inline]
fn fb_pattern_get(pattern: &FbPattern) -> u64 {
    pattern.pixels
}

#[inline]
fn fb_pattern_static(_pattern: &mut FbPattern) {}

#[inline]
fn fb_pattern_rotate(pattern: &mut FbPattern) {
    pattern.pixels =
        fb_left(pattern.pixels, pattern.left) | fb_right(pattern.pixels, pattern.right);
}

/// `FB_PAT(i)`: a pattern repeating a `i`-bit pixel across a long.
const fn fb_pat(i: u32) -> u64 {
    (((1u64 << ((63 / i) * i)) / ((1u64 << i) - 1)) << i) | 1
}

/// `mulconst[]` of `pixel_to_pat()` for `bpp` in `0..16`.
const MULCONST: [u64; 16] = [
    0,
    !0u64,
    fb_pat(2),
    fb_pat(3),
    fb_pat(4),
    fb_pat(5),
    fb_pat(6),
    fb_pat(7),
    fb_pat(8),
    fb_pat(9),
    fb_pat(10),
    fb_pat(11),
    fb_pat(12),
    fb_pat(13),
    fb_pat(14),
    fb_pat(15),
];

/// `pixel_to_pat()`: replicate a colour across one long (little-endian).
fn pixel_to_pat(bpp: i32, color: u32) -> u64 {
    let color = color as u64;
    match bpp {
        0..=15 => MULCONST[bpp as usize].wrapping_mul(color),
        16..=31 => {
            let mut pattern = color;
            pattern |= pattern << bpp;
            pattern |= pattern << (bpp * 2);
            pattern
        }
        32..=63 => {
            let mut pattern = color;
            pattern |= pattern << bpp;
            pattern
        }
        _ => color,
    }
}

/// `fb_rotate()`.
#[inline]
fn fb_rotate(pattern: u64, shift: i32, bpp: i32) -> u64 {
    let shift = shift % bpp;
    fb_right(pattern, shift) | fb_left(pattern, bpp - shift)
}

/// `bitfill()`: overwrite bits of one scanline with the pattern.
unsafe fn bitfill(
    dst: &FbAddress,
    pattern: &mut FbPattern,
    get: fn(&FbPattern) -> u64,
    rotate: fn(&mut FbPattern),
    mut end: i32,
) {
    unsafe {
        end += dst.bits;

        let mut first = fb_pixel_mask(dst.bits);
        let mut last = !fb_pixel_mask(end & (BITS_PER_LONG - 1));

        if end <= BITS_PER_LONG {
            last = if last != 0 { last & first } else { first };
            first = get(pattern);
            if last == !0u64 {
                fb_write_offset(first, 0, dst);
            } else if last != 0 {
                fb_modify_offset(first, last, 0, dst);
            }
        } else {
            let mut offset = (first != !0u64) as i32;
            if offset != 0 {
                fb_modify_offset(get(pattern), first, 0, dst);
                rotate(pattern);
            }
            end /= BITS_PER_LONG;
            while offset + 4 <= end {
                fb_write_offset(get(pattern), offset, dst);
                rotate(pattern);
                fb_write_offset(get(pattern), offset + 1, dst);
                rotate(pattern);
                fb_write_offset(get(pattern), offset + 2, dst);
                rotate(pattern);
                fb_write_offset(get(pattern), offset + 3, dst);
                rotate(pattern);
                offset += 4;
            }
            while offset < end {
                fb_write_offset(get(pattern), offset, dst);
                offset += 1;
                rotate(pattern);
            }
            if last != 0 {
                fb_modify_offset(get(pattern), last, offset, dst);
            }
        }
    }
}

/// `bitinvert()`: xor the pattern into one scanline.
unsafe fn bitinvert(
    dst: &FbAddress,
    pattern: &mut FbPattern,
    get: fn(&FbPattern) -> u64,
    rotate: fn(&mut FbPattern),
    mut end: i32,
) {
    unsafe {
        end += dst.bits;

        let mut first = fb_pixel_mask(dst.bits);
        let mut last = !fb_pixel_mask(end & (BITS_PER_LONG - 1));
        let mut offset: i32;

        if end <= BITS_PER_LONG {
            offset = 0;
            last = if last != 0 { last & first } else { first };
        } else {
            offset = (first != !0u64) as i32;
            if offset != 0 {
                first &= get(pattern);
                if first != 0 {
                    fb_invert_offset(first, 0, dst);
                }
                rotate(pattern);
            }
            end /= BITS_PER_LONG;
            while offset + 4 <= end {
                fb_invert_offset(get(pattern), offset, dst);
                rotate(pattern);
                fb_invert_offset(get(pattern), offset + 1, dst);
                rotate(pattern);
                fb_invert_offset(get(pattern), offset + 2, dst);
                rotate(pattern);
                fb_invert_offset(get(pattern), offset + 3, dst);
                rotate(pattern);
                offset += 4;
            }
            while offset < end {
                fb_invert_offset(get(pattern), offset, dst);
                offset += 1;
                rotate(pattern);
            }
        }

        last &= get(pattern);
        if last != 0 {
            fb_invert_offset(last, offset, dst);
        }
    }
}

/// `fb_fillrect_static()`: pattern does not change (1..64 bpp powers of two).
unsafe fn fb_fillrect_static(
    rect: &FbFillRect,
    bpp: i32,
    dst: &mut FbAddress,
    pattern: &mut FbPattern,
    bits_per_line: u32,
) {
    unsafe {
        let mut height = rect.height;
        let width = rect.width.wrapping_mul(bpp as u32) as i32;

        if rect.rop == ROP_XOR {
            while height != 0 {
                height -= 1;
                bitinvert(dst, pattern, fb_pattern_get, fb_pattern_static, width);
                fb_address_forward(dst, bits_per_line);
            }
        } else {
            while height != 0 {
                height -= 1;
                bitfill(dst, pattern, fb_pattern_get, fb_pattern_static, width);
                fb_address_forward(dst, bits_per_line);
            }
        }
    }
}

/// `fb_fillrect_rotating()`: pattern rotates by the start bit (e.g. 24 bpp).
unsafe fn fb_fillrect_rotating(
    rect: &FbFillRect,
    bpp: i32,
    dst: &mut FbAddress,
    pattern: &mut FbPattern,
    bits_per_line: u32,
) {
    unsafe {
        let pat = pattern.pixels;
        let mut height = rect.height;
        let width = rect.width.wrapping_mul(bpp as u32) as i32;

        if rect.rop == ROP_XOR {
            while height != 0 {
                height -= 1;
                pattern.pixels = fb_rotate(pat, dst.bits, bpp);
                bitinvert(dst, pattern, fb_pattern_get, fb_pattern_rotate, width);
                fb_address_forward(dst, bits_per_line);
            }
        } else {
            while height != 0 {
                height -= 1;
                pattern.pixels = fb_rotate(pat, dst.bits, bpp);
                bitfill(dst, pattern, fb_pattern_get, fb_pattern_rotate, width);
                fb_address_forward(dst, bits_per_line);
            }
        }
    }
}

/// `fb_fillrect()`.
unsafe fn fb_fillrect(p: *mut FbInfo, rect: &FbFillRect) {
    unsafe {
        let bpp = (*p).var.bits_per_pixel as i32;
        let bits_per_line = (*p).fix.line_length * 8;
        let palette = fb_palette(p);
        let mut dst = fb_address_init(p);

        fb_address_forward(
            &mut dst,
            rect.dy
                .wrapping_mul(bits_per_line)
                .wrapping_add(rect.dx.wrapping_mul(bpp as u32)),
        );

        let color = if palette.is_null() {
            rect.color
        } else {
            *palette.add(rect.color as usize)
        };
        let left = if bpp == 0 { 0 } else { BITS_PER_LONG % bpp };
        let mut pattern = FbPattern {
            pixels: pixel_to_pat(bpp, color),
            left,
            right: if left != 0 { bpp - left } else { 0 },
        };

        if left != 0 {
            fb_fillrect_rotating(rect, bpp, &mut dst, &mut pattern, bits_per_line);
        } else {
            fb_fillrect_static(rect, bpp, &mut dst, &mut pattern, bits_per_line);
        }
    }
}

/// `cfb_fillrect()`: fill a rectangle in an I/O-memory framebuffer.
///
/// # Safety
/// `p` must be a live framebuffer whose `screen_base` is valid for its
/// `screen_size`, and `rect` must be readable.
// upstream: cfbfillrect.c cfb_fillrect()
#[unsafe(export_name = "cfb_fillrect")]
pub unsafe extern "C" fn cfb_fillrect(p: *mut FbInfo, rect: *const FbFillRect) {
    unsafe {
        if !cfb_entry_running(p) {
            return;
        }
        if cfb_is_virtfb(p) {
            warn_virtfb_once();
        }
        cfb_sync(p);
        fb_fillrect(p, &*rect);
    }
}
