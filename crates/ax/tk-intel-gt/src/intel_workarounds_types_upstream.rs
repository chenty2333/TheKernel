// SPDX-License-Identifier: MIT
// Copyright © 2014-2018 Intel Corporation.
// Source-order type transcription of Linux 7.2.3
// drivers/gpu/drm/i915/gt/intel_workarounds_types.h.
//
// The register bitfields are represented by their containing C u32 storage
// word. This separate header binding does not alias the prior workarounds.c
// translation's records; consumers must switch the two together.

use core::ffi::c_char;

use crate::intel_gt_types_upstream::IntelGt;

/// `i915_reg_t`, the typedef'd register-offset wrapper from i915_reg_defs.h.
#[repr(C)]
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub struct I915RegT {
    pub reg: u32,
}

#[allow(non_camel_case_types)]
pub type i915_reg_t = I915RegT;

/// `i915_mcr_reg_t`, the multicast-register-offset wrapper.
#[repr(C)]
#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub struct I915McrRegT {
    pub reg: u32,
}

#[allow(non_camel_case_types)]
pub type i915_mcr_reg_t = I915McrRegT;

#[repr(C)]
#[derive(Clone, Copy)]
pub union I915WaReg {
    pub reg: I915RegT,
    pub mcr_reg: I915McrRegT,
}

/// `struct i915_wa`. The final two adjacent `u32:1` bitfields share one
/// 32-bit storage word at offset 16. The unassigned bits are preserved by
/// read/modify/write operations through `flags`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct I915Wa {
    pub reg: I915WaReg,
    pub clr: u32,
    pub set: u32,
    pub read: u32,
    pub flags: u32,
}

impl I915Wa {
    pub const fn masked_reg(&self) -> bool {
        self.flags & 1 != 0
    }

    pub const fn is_mcr(&self) -> bool {
        self.flags & 2 != 0
    }

    pub fn set_masked_reg(&mut self, value: bool) {
        self.flags = (self.flags & !1) | (if value { 1 } else { 0 });
    }

    pub fn set_is_mcr(&mut self, value: bool) {
        self.flags = (self.flags & !2) | (if value { 2 } else { 0 });
    }
}

/// `struct i915_wa_list`.
#[repr(C)]
pub struct I915WaList {
    pub gt: *mut IntelGt,
    pub name: *const c_char,
    pub engine_name: *const c_char,
    pub list: *mut I915Wa,
    pub count: u32,
    pub wa_count: u32,
}

const _: [(); 4] = [(); core::mem::size_of::<I915RegT>()];
const _: [(); 4] = [(); core::mem::size_of::<I915McrRegT>()];
const _: [(); 4] = [(); core::mem::size_of::<I915WaReg>()];
const _: [(); 20] = [(); core::mem::size_of::<I915Wa>()];
const _: [(); 4] = [(); core::mem::offset_of!(I915Wa, clr)];
const _: [(); 8] = [(); core::mem::offset_of!(I915Wa, set)];
const _: [(); 12] = [(); core::mem::offset_of!(I915Wa, read)];
const _: [(); 16] = [(); core::mem::offset_of!(I915Wa, flags)];
const _: [(); 40] = [(); core::mem::size_of::<I915WaList>()];
const _: [(); 8] = [(); core::mem::offset_of!(I915WaList, name)];
const _: [(); 16] = [(); core::mem::offset_of!(I915WaList, engine_name)];
const _: [(); 24] = [(); core::mem::offset_of!(I915WaList, list)];
const _: [(); 32] = [(); core::mem::offset_of!(I915WaList, count)];
const _: [(); 36] = [(); core::mem::offset_of!(I915WaList, wa_count)];
