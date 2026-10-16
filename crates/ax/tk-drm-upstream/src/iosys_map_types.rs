// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/linux/iosys-map.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub union IosysMapAnon0 {
    pub vaddr_iomem: *mut c_void,
    pub vaddr: *mut c_void,
}

#[repr(C)]
pub struct IosysMap {
    pub _anon_0: IosysMapAnon0,
    pub is_iomem: bool,
}

const _: () = assert!(size_of::<IosysMapAnon0>() == 8);
const _: () = assert!(size_of::<IosysMap>() == 16);
const _: () = assert!(core::mem::offset_of!(IosysMap, _anon_0) == 0);
const _: () = assert!(core::mem::offset_of!(IosysMap, is_iomem) == 8);

/// Linux `iosys_map_set_vaddr()` (`include/linux/iosys-map.h`): record a
/// kernel-virtual address and clear `is_iomem`.
///
/// # Safety
/// `map` must be a valid, writable `struct iosys_map`.
#[inline]
pub unsafe fn iosys_map_set_vaddr(map: *mut IosysMap, vaddr: *mut c_void) {
    unsafe {
        (*map)._anon_0.vaddr = vaddr;
        (*map).is_iomem = false;
    }
}
