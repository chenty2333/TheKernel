// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_dmc_wl.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::DelayedWork;
use tk_linux_kapi::types::RefcountT;
use tk_linux_kapi::types::Spinlock;

#[repr(C, align(8))]
pub struct IntelDmcWl {
    pub lock: Spinlock,
    pub enabled: bool,
    pub taken: bool,
    pub _pad_6: [u8; 2],
    pub refcount: RefcountT,
    pub dc_state: u32,
    pub work: DelayedWork,
}

const _: () = assert!(size_of::<IntelDmcWl>() == 104);
const _: () = assert!(align_of::<IntelDmcWl>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelDmcWl, lock) == 0);
const _: () = assert!(size_of::<Spinlock>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDmcWl, enabled) == 4);
const _: () = assert!(core::mem::offset_of!(IntelDmcWl, taken) == 5);
const _: () = assert!(core::mem::offset_of!(IntelDmcWl, refcount) == 8);
const _: () = assert!(size_of::<RefcountT>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelDmcWl, dc_state) == 12);
const _: () = assert!(core::mem::offset_of!(IntelDmcWl, work) == 16);
const _: () = assert!(size_of::<DelayedWork>() == 88);
