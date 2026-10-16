// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
// Source: drivers/video/fbdev/core/cfbimgblt.c and fb_imageblit.h (Linux v7.2.3).
//! `cfb_imageblit()`: glyph/bitmap and colour-image blits to I/O-memory
//! packed-pixel framebuffers.
//!
//! Translated configuration: 64-bit little-endian (see `fb_draw_upstream`).
//! The 32-bit-only `fb_bitmap_1ppw()` path is dead on this target and is not
//! carried over.
#![allow(unsafe_code, non_snake_case, dead_code)]

use crate::{
    fb_draw_upstream::{
        BITS_PER_BYTE, BITS_PER_LONG, FbAddress, cfb_entry_running, cfb_is_virtfb, cfb_sync,
        fb_address_forward, fb_address_init, fb_address_move_long, fb_comp, fb_left, fb_palette,
        fb_pixel_mask, fb_read_offset, fb_right, fb_write_offset, warn_virtfb_once,
    },
    fb_types::{FbImage, FbInfo},
};

/// `mul[]` of `fb_bitmap4x_imageblit()`.
const MUL: [u32; 8] = [
    0xf, 0x55, 0x249, 0x1111, 0x8421, 0x41041, 0x204081, 0x01010101,
];

/// `FB_GEN(a, b)` (little-endian).
const fn fb_gen(a: u32, b: u32) -> u32 {
    (a / 8 + ((a & 4) << (b - 2)) + ((a & 2) << (b * 2 - 1)) + ((a & 1) << (b * 3)))
        * ((1 << b) - 1)
}

/// `FB_GEN1(a)` (little-endian).
const fn fb_gen1(a: u32) -> u32 {
    a / 8 + (a & 4) / 2 + (a & 2) * 2 + (a & 1) * 8
}

/// `expand[BITS_PER_BYTE][1 << 4]` of `fb_bitmap4x_imageblit()`: row 0 is
/// `FB_GEN1`, row `r` is `FB_GENx(r + 1)`.
const fn expand_table() -> [[u32; 16]; 8] {
    let mut table = [[0u32; 16]; 8];
    let mut row = 0;
    while row < 8 {
        let mut a = 0;
        while a < 16 {
            table[row][a] = if row == 0 {
                fb_gen1(a as u32)
            } else {
                fb_gen(a as u32, row as u32 + 1)
            };
            a += 1;
        }
        row += 1;
    }
    table
}

static EXPAND: [[u32; 16]; 8] = expand_table();

/// `FB_PATP(a, b)`: nibble `a` placed at nibble `b`, replicated across a long.
const fn fb_patp(a: u64, b: u32) -> u64 {
    (a << (b * 16)).wrapping_mul((1u64 << 16) - 1)
}

/// `FB_PAT4(a)`.
const fn fb_pat4(a: u64) -> u64 {
    fb_patp(a & 1, 0)
        | fb_patp(((a & 2) / 2) as u64, 1)
        | fb_patp(((a & 4) / 4) as u64, 2)
        | fb_patp(((a & 8) / 8) as u64, 3)
}

const TAB16_LE: [u64; 16] = [
    0,
    fb_pat4(8),
    fb_pat4(4),
    fb_pat4(12),
    fb_pat4(2),
    fb_pat4(10),
    fb_pat4(6),
    fb_pat4(14),
    fb_pat4(1),
    fb_pat4(9),
    fb_pat4(5),
    fb_pat4(13),
    fb_pat4(3),
    fb_pat4(11),
    fb_pat4(7),
    !0u64,
];

/// `fb_bitblit()`: merge a run of pixels, produced by `get`, into `dst`.
/// `get` receives the pixel width `bits` and may update it.
unsafe fn fb_bitblit<F>(mut get: F, mut bits: i32, dst: &mut FbAddress)
where
    F: FnMut(&mut u64, &mut i32) -> bool,
{
    unsafe {
        let mut offset: i32 = 0;
        let mut shift = dst.bits;
        let mut old: u64;
        let mut val: u64;

        if shift != 0 {
            old = fb_read_offset(0, dst);
            val = old;
            val &= !fb_right(!0u64, shift);
        } else {
            old = 0;
            val = 0;
        }

        let mut pixels: u64 = 0;
        while get(&mut pixels, &mut bits) {
            val |= fb_right(pixels, shift);
            shift += bits;
            if shift < BITS_PER_LONG {
                continue;
            }
            fb_write_offset(val, offset, dst);
            offset += 1;
            shift &= BITS_PER_LONG - 1;
            val = if shift == 0 {
                0
            } else {
                fb_left(pixels, bits - shift)
            };
        }

        if shift != 0 {
            let mask = !fb_pixel_mask(shift);
            if offset != 0 || dst.bits == 0 {
                old = fb_read_offset(offset, dst);
            }
            fb_write_offset(fb_comp(val, old, mask), offset, dst);
        }
    }
}

/// `struct fb_bitmap_iter`: 1-bit bitmap, two colours.
struct FbBitmapIter {
    data: *const u8,
    colors: [u64; 2],
    width: i32,
    i: i32,
}

/// `fb_bitmap_image()`.
unsafe fn fb_bitmap_image(iter: &mut FbBitmapIter, pixels: &mut u64, _bits: &mut i32) -> bool {
    unsafe {
        if iter.i < iter.width {
            let bit = (!iter.i) & (BITS_PER_BYTE as i32 - 1);
            let byte = iter.i / BITS_PER_BYTE as i32;
            iter.i += 1;
            let sel = ((*iter.data.add(byte as usize) as i32) >> bit) & 1;
            *pixels = iter.colors[sel as usize];
            return true;
        }
        iter.data = iter.data.add(((iter.width + 7) / 8) as usize);
        iter.i = 0;
        false
    }
}

/// `struct fb_color_iter`: one pixel per data byte, optional palette.
struct FbColorIter {
    data: *const u8,
    palette: *const u32,
    shift: i32,
    width: i32,
    i: i32,
}

/// `fb_color_image()`.
unsafe fn fb_color_image(iter: &mut FbColorIter, pixels: &mut u64, _bits: &mut i32) -> bool {
    unsafe {
        if iter.i < iter.width {
            let mut color = *iter.data.add(iter.i as usize) as u64;
            iter.i += 1;
            if !iter.palette.is_null() {
                color = *iter.palette.add(color as usize) as u64;
            }
            *pixels = color.wrapping_shl(iter.shift as u32);
            return true;
        }
        iter.data = iter.data.add(iter.width as usize);
        iter.i = 0;
        false
    }
}

/// `struct fb_bitmap4x_iter`: 4-bit-per-pixel expansion through `expand[]`.
struct FbBitmap4xIter {
    data: *const u8,
    fgxcolor: u32,
    bgcolor: u32,
    width: i32,
    i: i32,
    expand: *const [u32; 16],
    bpp: i32,
    top: bool,
}

/// `fb_bitmap4x_image()`.
unsafe fn fb_bitmap4x_image(iter: &mut FbBitmap4xIter, pixels: &mut u64, bits: &mut i32) -> bool {
    unsafe {
        let half = (BITS_PER_BYTE / 2) as i32;
        let data: u8;
        if iter.i >= half {
            iter.i -= half;
            iter.top = !iter.top;
            if iter.top {
                data = *iter.data >> (BITS_PER_BYTE / 2);
                iter.data = iter.data.add(1);
            } else {
                data = *iter.data.sub(1) & 0x0f;
            }
        } else if iter.i != 0 {
            *bits = iter.bpp * iter.i;
            if iter.top {
                data = *iter.data.sub(1) & 0x0f;
            } else {
                data = *iter.data >> (BITS_PER_BYTE / 2);
                iter.data = iter.data.add(1);
            }
            iter.i = 0;
        } else {
            *bits = iter.bpp * BITS_PER_BYTE as i32 / 2;
            iter.i = iter.width;
            iter.top = false;
            return false;
        }

        let expanded = (*iter.expand)[data as usize];
        *pixels = ((iter.fgxcolor & expanded) ^ iter.bgcolor) as u64;
        true
    }
}

/// `fb_bitmap4x_imageblit()`: 1..8 bpp bitmap through the expansion table.
unsafe fn fb_bitmap4x_imageblit(
    image: &FbImage,
    dst: &mut FbAddress,
    fgcolor: u64,
    bgcolor: u64,
    bpp: i32,
    bits_per_line: u32,
) {
    unsafe {
        let mul = MUL[(bpp - 1) as usize] as u64;
        let mut iter = FbBitmap4xIter {
            data: image.data.cast::<u8>(),
            fgxcolor: (fgcolor ^ bgcolor).wrapping_mul(mul) as u32,
            bgcolor: bgcolor.wrapping_mul(mul) as u32,
            width: image.width as i32,
            i: image.width as i32,
            expand: core::ptr::addr_of!(EXPAND[(bpp - 1) as usize]),
            bpp,
            top: false,
        };

        let mut height = image.height;
        while height != 0 {
            height -= 1;
            let bits = bpp * BITS_PER_BYTE as i32 / 2;
            fb_bitblit(
                |pixels, bits| fb_bitmap4x_image(&mut iter, pixels, bits),
                bits,
                dst,
            );
            fb_address_forward(dst, bits_per_line);
        }
    }
}

/// `fb_bitmap1x_imageblit()`: 9..64 bpp bitmap, two colours.
unsafe fn fb_bitmap1x_imageblit(
    image: &FbImage,
    dst: &mut FbAddress,
    fgcolor: u64,
    bgcolor: u64,
    bpp: i32,
    bits_per_line: u32,
) {
    unsafe {
        let mut iter = FbBitmapIter {
            data: image.data.cast::<u8>(),
            colors: [bgcolor, fgcolor],
            width: image.width as i32,
            i: 0,
        };

        let mut height = image.height;
        while height != 0 {
            height -= 1;
            fb_bitblit(
                |pixels, bits| fb_bitmap_image(&mut iter, pixels, bits),
                bpp,
                dst,
            );
            fb_address_forward(dst, bits_per_line);
        }
    }
}

/// `fb_bitmap_2ppw()`: 32 bpp bitmap, two pixels per long.
unsafe fn fb_bitmap_2ppw(
    image: &FbImage,
    dst: &mut FbAddress,
    fgcolor: u64,
    bgcolor: u64,
    words_per_line: u32,
) {
    unsafe {
        let half = BITS_PER_LONG / 2;
        let tab: [u64; 4] = [
            bgcolor | bgcolor << half,
            bgcolor | fgcolor << half,
            fgcolor | bgcolor << half,
            fgcolor | fgcolor << half,
        ];
        let mut src = image.data.cast::<u8>();
        let width = (image.width / 2) as i32;

        let mut height = image.height;
        while height != 0 {
            height -= 1;
            let mut offset: i32 = 0;
            while offset + 4 <= width {
                let srcbyte = *src as u32;
                src = src.add(1);
                fb_write_offset(tab[((srcbyte >> 6) & 3) as usize], offset, dst);
                fb_write_offset(tab[((srcbyte >> 4) & 3) as usize], offset + 1, dst);
                fb_write_offset(tab[((srcbyte >> 2) & 3) as usize], offset + 2, dst);
                fb_write_offset(tab[(srcbyte & 3) as usize], offset + 3, dst);
                offset += 4;
            }
            if offset < width {
                let mut srcbyte = *src as u32;
                src = src.add(1);
                while offset < width {
                    fb_write_offset(tab[((srcbyte >> 6) & 3) as usize], offset, dst);
                    srcbyte <<= 2;
                    offset += 1;
                }
            }
            fb_address_move_long(dst, words_per_line as i32);
        }
    }
}

/// `fb_bitmap_4ppw()`: 16 bpp bitmap, four pixels per long.
unsafe fn fb_bitmap_4ppw(
    image: &FbImage,
    dst: &mut FbAddress,
    fgcolor: u64,
    bgcolor: u64,
    words_per_line: u32,
) {
    unsafe {
        let quarter = BITS_PER_LONG as u32 / 4;
        let half = BITS_PER_LONG as u32 / 2;
        let mut fgcolor = fgcolor | fgcolor << quarter;
        let mut bgcolor = bgcolor | bgcolor << quarter;
        fgcolor |= fgcolor << half;
        bgcolor |= bgcolor << half;
        fgcolor ^= bgcolor;

        let tab = &TAB16_LE;
        let mut src = image.data.cast::<u8>();
        let width = (image.width / 4) as i32;

        let mut height = image.height;
        while height != 0 {
            height -= 1;
            let mut offset: i32 = 0;
            while offset + 2 <= width {
                let byte = *src as usize;
                fb_write_offset((fgcolor & tab[byte >> 4]) ^ bgcolor, offset, dst);
                fb_write_offset((fgcolor & tab[byte & 0xf]) ^ bgcolor, offset + 1, dst);
                offset += 2;
                src = src.add(1);
            }
            if offset < width {
                let byte = *src as usize;
                src = src.add(1);
                fb_write_offset((fgcolor & tab[byte >> 4]) ^ bgcolor, offset, dst);
            }
            fb_address_move_long(dst, words_per_line as i32);
        }
    }
}

/// `fb_bitmap_imageblit()`: a 1-bit image, dispatched by depth.
unsafe fn fb_bitmap_imageblit(
    image: &FbImage,
    dst: &mut FbAddress,
    bits_per_line: u32,
    palette: *const u32,
    bpp: i32,
) {
    unsafe {
        let (fgcolor, bgcolor) = if !palette.is_null() {
            (
                *palette.add(image.fg_color as usize) as u64,
                *palette.add(image.bg_color as usize) as u64,
            )
        } else {
            (image.fg_color as u64, image.bg_color as u64)
        };

        if dst.bits == 0 && bits_per_line & (BITS_PER_LONG as u32 - 1) == 0 {
            if bpp == BITS_PER_LONG / 2 && image.width & 1 == 0 {
                fb_bitmap_2ppw(
                    image,
                    dst,
                    fgcolor,
                    bgcolor,
                    bits_per_line / BITS_PER_LONG as u32,
                );
                return;
            }
            if bpp == BITS_PER_LONG / 4 && image.width & 3 == 0 {
                fb_bitmap_4ppw(
                    image,
                    dst,
                    fgcolor,
                    bgcolor,
                    bits_per_line / BITS_PER_LONG as u32,
                );
                return;
            }
        }

        if bpp > 0 && bpp <= BITS_PER_BYTE as i32 {
            fb_bitmap4x_imageblit(image, dst, fgcolor, bgcolor, bpp, bits_per_line);
        } else if bpp > BITS_PER_BYTE as i32 && bpp <= BITS_PER_LONG {
            fb_bitmap1x_imageblit(image, dst, fgcolor, bgcolor, bpp, bits_per_line);
        }
    }
}

/// `fb_color_imageblit()`: a colour image, one pixel per byte.
unsafe fn fb_color_imageblit(
    image: &FbImage,
    dst: &mut FbAddress,
    bits_per_line: u32,
    palette: *const u32,
    bpp: i32,
) {
    unsafe {
        let mut iter = FbColorIter {
            data: image.data.cast::<u8>(),
            palette,
            shift: 0,
            width: image.width as i32,
            i: 0,
        };

        let mut height = image.height;
        while height != 0 {
            height -= 1;
            fb_bitblit(
                |pixels, bits| fb_color_image(&mut iter, pixels, bits),
                bpp,
                dst,
            );
            fb_address_forward(dst, bits_per_line);
        }
    }
}

/// `fb_imageblit()`.
unsafe fn fb_imageblit(p: *mut FbInfo, image: &FbImage) {
    unsafe {
        let bpp = (*p).var.bits_per_pixel as i32;
        let bits_per_line = (*p).fix.line_length * BITS_PER_BYTE;
        let mut dst = fb_address_init(p);
        let palette = fb_palette(p);

        fb_address_forward(
            &mut dst,
            (image.dy as u32)
                .wrapping_mul(bits_per_line)
                .wrapping_add((image.dx as u32).wrapping_mul(bpp as u32)),
        );

        if image.depth == 1 {
            fb_bitmap_imageblit(image, &mut dst, bits_per_line, palette, bpp);
        } else {
            fb_color_imageblit(image, &mut dst, bits_per_line, palette, bpp);
        }
    }
}

/// `cfb_imageblit()`: draw an image into an I/O-memory framebuffer.
///
/// # Safety
/// `p` must be a live framebuffer whose `screen_base` is valid for its
/// `screen_size`; `image` and its `data` must be readable for the image size.
// upstream: cfbimgblt.c cfb_imageblit()
#[unsafe(export_name = "cfb_imageblit")]
pub unsafe extern "C" fn cfb_imageblit(p: *mut FbInfo, image: *const FbImage) {
    unsafe {
        if !cfb_entry_running(p) {
            return;
        }
        if cfb_is_virtfb(p) {
            warn_virtfb_once();
        }
        cfb_sync(p);
        fb_imageblit(p, &*image);
    }
}
