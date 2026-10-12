// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Field-for-field mirror of the Linux v7.2.3 x86_64 definitions below,
// generated from the configured oracle's DWARF layout (pahole). Every
// size and field offset is pinned by a compile-time assertion.
//! i915 display type definitions. Source headers:
//! `drivers/gpu/drm/i915/display/intel_display_irq.h`
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unused_imports)]

use core::ffi::{c_char, c_ulong, c_void};
use core::mem::{align_of, size_of, ManuallyDrop};


#[repr(C)]
pub struct IntelDisplayIrqState {
    pub master_ctl: u32,
    pub iir: u32,
    pub eir: u32,
    pub hotplug_status: u32,
    pub dpinvgtt: u32,
    pub pipe_stats: [u32; 4],
}

const _: () = assert!(size_of::<IntelDisplayIrqState>() == 36);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrqState, master_ctl) == 0);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrqState, iir) == 4);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrqState, eir) == 8);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrqState, hotplug_status) == 12);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrqState, dpinvgtt) == 16);
const _: () = assert!(core::mem::offset_of!(IntelDisplayIrqState, pipe_stats) == 20);
const _: () = assert!(size_of::<[u32; 4]>() == 16);
