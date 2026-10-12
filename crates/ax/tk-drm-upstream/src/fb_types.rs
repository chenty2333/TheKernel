// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! Linux DRM type definitions. Source headers:
//! `include/linux/fb.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::ListHead;
use tk_linux_kapi::types::Page;

#[repr(C)]
pub struct FbDeferredIo {
    pub delay: u64,
    pub sort_pagereflist: bool,
    pub get_page: Option<unsafe extern "C" fn(*mut FbInfo, u64) -> *mut Page>,
    pub deferred_io: Option<unsafe extern "C" fn(*mut FbInfo, *mut ListHead)>,
}

#[repr(C)]
pub struct FbInfo {
    _opaque: [u8; 0],
}

const _: () = assert!(size_of::<FbDeferredIo>() == 32);
const _: () = assert!(core::mem::offset_of!(FbDeferredIo, delay) == 0);
const _: () = assert!(core::mem::offset_of!(FbDeferredIo, sort_pagereflist) == 8);
const _: () = assert!(core::mem::offset_of!(FbDeferredIo, get_page) == 16);
const _: () = assert!(core::mem::offset_of!(FbDeferredIo, deferred_io) == 24);
