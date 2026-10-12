// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_frontbuffer.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::AtomicT;
use tk_linux_kapi::types::WorkStruct;
use crate::intel_display_core_types_upstream::{IntelDisplay};

#[repr(C, align(8))]
pub struct IntelFrontbuffer {
    pub display: *mut IntelDisplay,
    pub bits: AtomicT,
    pub _pad_12: [u8; 4],
    pub flush_work: WorkStruct,
}

const _: () = assert!(size_of::<IntelFrontbuffer>() == 48);
const _: () = assert!(align_of::<IntelFrontbuffer>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelFrontbuffer, display) == 0);
const _: () = assert!(core::mem::offset_of!(IntelFrontbuffer, bits) == 8);
const _: () = assert!(size_of::<AtomicT>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelFrontbuffer, flush_work) == 16);
const _: () = assert!(size_of::<WorkStruct>() == 32);
