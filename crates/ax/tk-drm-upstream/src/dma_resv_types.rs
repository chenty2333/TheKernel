// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/linux/dma-resv.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::ww_mutex::WwMutex;

#[repr(C, align(8))]
pub struct DmaResv {
    pub lock: WwMutex,
    pub fences: *mut c_void,
}

const _: () = assert!(size_of::<DmaResv>() == 40);
const _: () = assert!(align_of::<DmaResv>() == 8);
const _: () = assert!(core::mem::offset_of!(DmaResv, lock) == 0);
const _: () = assert!(size_of::<WwMutex>() == 32);
const _: () = assert!(core::mem::offset_of!(DmaResv, fences) == 32);
