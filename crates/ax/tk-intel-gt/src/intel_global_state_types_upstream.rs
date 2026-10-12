// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_global_state.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};
use tk_linux_kapi::types::Kref;
use tk_linux_kapi::types::ListHead;
use crate::intel_display_types_upstream::{IntelAtomicState};

#[repr(C)]
pub struct IntelGlobalStateFuncs {
    pub atomic_duplicate_state: Option<unsafe extern "C" fn(*mut IntelGlobalObj) -> *mut IntelGlobalState>,
    pub atomic_destroy_state: Option<unsafe extern "C" fn(*mut IntelGlobalObj, *mut IntelGlobalState)>,
}

#[repr(C)]
pub struct IntelGlobalObj {
    pub head: ListHead,
    pub state: *mut IntelGlobalState,
    pub funcs: *mut IntelGlobalStateFuncs,
}

#[repr(C, align(8))]
pub struct IntelGlobalState {
    pub obj: *mut IntelGlobalObj,
    pub state: *mut IntelAtomicState,
    pub commit: *mut c_void,
    pub r#ref: Kref,
    pub changed: bool,
    pub serialized: bool,
    pub _pad_30: [u8; 2],
}

const _: () = assert!(size_of::<IntelGlobalStateFuncs>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelGlobalStateFuncs, atomic_duplicate_state) == 0);
const _: () = assert!(core::mem::offset_of!(IntelGlobalStateFuncs, atomic_destroy_state) == 8);
const _: () = assert!(size_of::<IntelGlobalObj>() == 32);
const _: () = assert!(core::mem::offset_of!(IntelGlobalObj, head) == 0);
const _: () = assert!(size_of::<ListHead>() == 16);
const _: () = assert!(core::mem::offset_of!(IntelGlobalObj, state) == 16);
const _: () = assert!(core::mem::offset_of!(IntelGlobalObj, funcs) == 24);
const _: () = assert!(size_of::<IntelGlobalState>() == 32);
const _: () = assert!(align_of::<IntelGlobalState>() == 8);
const _: () = assert!(core::mem::offset_of!(IntelGlobalState, obj) == 0);
const _: () = assert!(core::mem::offset_of!(IntelGlobalState, state) == 8);
const _: () = assert!(core::mem::offset_of!(IntelGlobalState, commit) == 16);
const _: () = assert!(core::mem::offset_of!(IntelGlobalState, r#ref) == 24);
const _: () = assert!(size_of::<Kref>() == 4);
const _: () = assert!(core::mem::offset_of!(IntelGlobalState, changed) == 28);
const _: () = assert!(core::mem::offset_of!(IntelGlobalState, serialized) == 29);
