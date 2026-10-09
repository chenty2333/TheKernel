// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Linux bitmap allocation and aligned-region helpers used by GuC contexts.

use alloc::alloc::{Layout, alloc_zeroed, dealloc};
use core::{ffi::{c_int, c_long, c_ulong}, ptr};

#[repr(C)]
struct BitmapAllocation {
    words: usize,
}

/// Linux `bitmap_zalloc()` with allocation metadata for the matching free.
/// The returned pointer addresses the first zeroed `unsigned long` word.
pub unsafe fn bitmap_zalloc(nbits: u32, _flags: u32) -> *mut c_ulong {
    let words = (nbits as usize).div_ceil(usize::BITS as usize);
    let size = core::mem::size_of::<BitmapAllocation>()
        .checked_add(words.checked_mul(core::mem::size_of::<usize>()).unwrap_or(usize::MAX));
    let Some(size) = size else { return ptr::null_mut() };
    let Ok(layout) = Layout::from_size_align(size, core::mem::align_of::<usize>()) else {
        return ptr::null_mut();
    };
    let allocation = unsafe { alloc_zeroed(layout) };
    if allocation.is_null() {
        return ptr::null_mut();
    }
    unsafe { allocation.cast::<BitmapAllocation>().write(BitmapAllocation { words }) };
    unsafe { allocation.add(core::mem::size_of::<BitmapAllocation>()).cast() }
}

/// Linux `bitmap_free()` paired with `bitmap_zalloc()`.
pub unsafe fn bitmap_free(bitmap: *mut c_ulong) {
    if bitmap.is_null() {
        return;
    }
    let allocation = unsafe { bitmap.cast::<u8>().sub(core::mem::size_of::<BitmapAllocation>()) };
    let words = unsafe { (*allocation.cast::<BitmapAllocation>()).words };
    let size = core::mem::size_of::<BitmapAllocation>() + words * core::mem::size_of::<usize>();
    let layout = Layout::from_size_align(size, core::mem::align_of::<usize>())
        .expect("bitmap allocation layout is recorded by bitmap_zalloc");
    unsafe { dealloc(allocation, layout) };
}

#[inline]
unsafe fn test_bit(bitmap: *const c_ulong, bit: usize) -> bool {
    unsafe { *bitmap.add(bit / usize::BITS as usize) & ((1 as c_ulong) << (bit % c_ulong::BITS as usize)) != 0 }
}

#[inline]
unsafe fn set_bit(bitmap: *mut c_ulong, bit: usize) {
    unsafe { *bitmap.add(bit / usize::BITS as usize) |= (1 as c_ulong) << (bit % c_ulong::BITS as usize) };
}

#[inline]
unsafe fn clear_bit(bitmap: *mut c_ulong, bit: usize) {
    unsafe { *bitmap.add(bit / usize::BITS as usize) &= !((1 as c_ulong) << (bit % c_ulong::BITS as usize)) };
}

/// Linux `bitmap_find_free_region()` for a power-of-two aligned region.
pub unsafe fn bitmap_find_free_region(bitmap: *mut c_ulong, nbits: u32, order: c_long) -> c_int {
    if bitmap.is_null() || order < 0 || order as u32 >= usize::BITS {
        return -1;
    }
    let Some(count) = 1usize.checked_shl(order as u32) else { return -1 };
    let nbits = nbits as usize;
    let mut start = 0usize;
    while start.checked_add(count).is_some_and(|end| end <= nbits) {
        if (0..count).all(|bit| !unsafe { test_bit(bitmap, start + bit) }) {
            for bit in 0..count {
                unsafe { set_bit(bitmap, start + bit) };
            }
            return start as c_int;
        }
        start += count;
    }
    -1
}

/// Linux `bitmap_release_region()` for a region reserved by the helper above.
pub unsafe fn bitmap_release_region(bitmap: *mut c_ulong, pos: c_ulong, order: c_long) {
    if bitmap.is_null() || order < 0 || order as u32 >= usize::BITS {
        return;
    }
    let Some(count) = 1usize.checked_shl(order as u32) else { return };
    for bit in 0..count {
        unsafe { clear_bit(bitmap, pos as usize + bit) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_regions_are_reserved_and_released() {
        let bitmap = unsafe { bitmap_zalloc(32, 0) };
        assert!(!bitmap.is_null());
        unsafe {
            assert_eq!(bitmap_find_free_region(bitmap, 32, 2), 0);
            assert_eq!(bitmap_find_free_region(bitmap, 32, 2), 4);
            bitmap_release_region(bitmap, 0, 2);
            assert_eq!(bitmap_find_free_region(bitmap, 32, 2), 0);
            bitmap_free(bitmap);
        }
    }
}
