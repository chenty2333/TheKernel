// SPDX-License-Identifier: GPL-2.0-only
// Copyright 2026 TheKernel contributors.
// Source: drivers/video/fbdev/core/fb_draw.h and cfbmem.h (Linux v7.2.3,
// Zsolt Kajtar's bit-address engine shared by the cfb_* drawing operations).
//! Bit-addressed framebuffer access for I/O-memory packed-pixel drawing.
//!
//! Configuration of the translated target: little-endian, 64-bit long,
//! `CONFIG_FB_FOREIGN_ENDIAN` and `CONFIG_FB_CFB_REV_PIXELS_IN_BYTE` unset.
//! `fb_reverse_init()` therefore always yields `{byte: false, pixel: false}`,
//! and the reversal branches of the C templates are not reachable; they are
//! not carried over.
#![allow(unsafe_code, non_snake_case, dead_code)]

use core::ptr;

use crate::fb_types::{FBINFO_STATE_RUNNING, FBINFO_VIRTFB, FbInfo};

pub(crate) const BITS_PER_LONG: i32 = 64;
pub(crate) const BITS_PER_BYTE: u32 = 8;
pub(crate) const ROP_XOR: u32 = 1;
const FB_VISUAL_TRUECOLOR: u32 = 2;
const FB_VISUAL_DIRECTCOLOR: u32 = 4;

/// `struct fb_address`: a bit address in framebuffer memory.
#[derive(Clone, Copy)]
pub(crate) struct FbAddress {
    pub address: *mut u8,
    pub bits: i32,
}

/// `fb_address_init()`: the beginning of the framebuffer, aligned down to a
/// long, with the misalignment expressed in bits.
pub(crate) unsafe fn fb_address_init(p: *mut FbInfo) -> FbAddress {
    let base = unsafe { (*p).screen_base } as usize;
    let aligned = base & !((BITS_PER_LONG as usize / 8) - 1);
    FbAddress {
        address: aligned as *mut u8,
        bits: ((base - aligned) * BITS_PER_BYTE as usize) as i32,
    }
}

/// `fb_write_offset()`: store one long at `dst + offset` longs.
#[inline]
pub(crate) unsafe fn fb_write_offset(val: u64, offset: i32, dst: &FbAddress) {
    unsafe {
        let p = dst.address.offset(offset as isize * 8).cast::<u64>();
        ptr::write_volatile(p, val);
    }
}

/// `fb_read_offset()`: load one long at `src + offset` longs.
#[inline]
pub(crate) unsafe fn fb_read_offset(offset: i32, src: &FbAddress) -> u64 {
    unsafe { ptr::read_volatile(src.address.offset(offset as isize * 8).cast::<u64>()) }
}

/// `fb_comp()`: compose bits from `set` and `unset` under `mask`.
#[inline]
pub(crate) fn fb_comp(set: u64, unset: u64, mask: u64) -> u64 {
    ((set ^ unset) & mask) ^ unset
}

/// `fb_modify_offset()`: read-modify-write the bits selected by `mask`.
#[inline]
pub(crate) unsafe fn fb_modify_offset(val: u64, mask: u64, offset: i32, dst: &FbAddress) {
    unsafe {
        let old = fb_read_offset(offset, dst);
        fb_write_offset(fb_comp(val, old, mask), offset, dst);
    }
}

/// `fb_invert_offset()`: xor a pattern into one long.
#[inline]
pub(crate) unsafe fn fb_invert_offset(pat: u64, offset: i32, dst: &FbAddress) {
    unsafe {
        let old = fb_read_offset(offset, dst);
        fb_write_offset(old ^ pat, offset, dst);
    }
}

/// `fb_address_move_long()`: move the address by whole longs.
#[inline]
pub(crate) fn fb_address_move_long(adr: &mut FbAddress, offset: i32) {
    adr.address = adr.address.wrapping_offset(offset as isize * 8);
}

/// `fb_address_forward()`: advance by `offset` bits.
#[inline]
pub(crate) fn fb_address_forward(adr: &mut FbAddress, offset: u32) {
    let bits = (adr.bits as u32).wrapping_add(offset);
    adr.bits = (bits & (BITS_PER_LONG as u32 - 1)) as i32;
    adr.address = adr
        .address
        .wrapping_add(((bits & !(BITS_PER_LONG as u32 - 1)) / BITS_PER_BYTE) as usize);
}

/// `fb_address_backward()`: retreat by `offset` bits.
#[inline]
pub(crate) fn fb_address_backward(adr: &mut FbAddress, offset: u32) {
    let bits = adr.bits.wrapping_sub(offset as i32);
    adr.bits = bits & (BITS_PER_LONG - 1);
    if bits < 0 {
        let step = (adr.bits - bits) / BITS_PER_BYTE as i32;
        adr.address = adr.address.wrapping_sub(step as usize);
    } else {
        let step = (bits - adr.bits) / BITS_PER_BYTE as i32;
        adr.address = adr.address.wrapping_add(step as usize);
    }
}

/// `fb_left()` (little-endian): move pixels towards the lower address.
#[inline]
pub(crate) fn fb_left(value: u64, index: i32) -> u64 {
    value.wrapping_shr(index as u32)
}

/// `fb_right()` (little-endian): move pixels towards the higher address.
#[inline]
pub(crate) fn fb_right(value: u64, index: i32) -> u64 {
    value.wrapping_shl(index as u32)
}

/// `fb_pixel_mask()` with no reversal: the bits of a long from `index` up.
#[inline]
pub(crate) fn fb_pixel_mask(index: i32) -> u64 {
    fb_right(!0u64, index)
}

/// `fb_palette()`: the pseudo palette, if the visual uses one.
#[inline]
pub(crate) unsafe fn fb_palette(info: *mut FbInfo) -> *const u32 {
    let visual = unsafe { (*info).fix.visual };
    if visual == FB_VISUAL_TRUECOLOR || visual == FB_VISUAL_DIRECTCOLOR {
        unsafe { (*info).pseudo_palette.cast::<u32>() }
    } else {
        ptr::null()
    }
}

/// `FBINFO_VIRTFB` / `FBINFO_STATE_RUNNING` checks shared by the cfb entry points.
#[inline]
pub(crate) unsafe fn cfb_entry_running(p: *mut FbInfo) -> bool {
    unsafe { (*p).state == FBINFO_STATE_RUNNING }
}

#[inline]
pub(crate) unsafe fn cfb_is_virtfb(p: *mut FbInfo) -> bool {
    unsafe { (*p).flags as u32 & FBINFO_VIRTFB != 0 }
}

/// `p->fbops->fb_sync(p)` when the driver provides one.
#[inline]
pub(crate) unsafe fn cfb_sync(p: *mut FbInfo) {
    unsafe {
        if let Some(fb_sync) = (*(*p).fbops).fb_sync {
            fb_sync(p);
        }
    }
}

/// `fb_warn_once()` for `FBINFO_VIRTFB` framebuffers.
pub(crate) fn warn_virtfb_once() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        crate::drm_print_upstream::drm_dev_printk(
            core::ptr::null(),
            tk_linux_kapi::print::DrmLogLevel::Warn,
            "Framebuffer is not in I/O address space.",
        );
    }
}
