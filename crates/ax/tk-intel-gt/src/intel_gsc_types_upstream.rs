// SPDX-License-Identifier: MIT
// Copyright(c) 2019-2022, Intel Corporation. All rights reserved.
// Source-order ABI bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_gsc.h.

#![allow(non_snake_case)]

use core::ffi::c_ulong;

use crate::{
    i915_gem_object_types_upstream::DrmI915GemObject, intel_gt_types_upstream::IntelGt,
    linux_i915_private::DrmI915Private,
};

pub const INTEL_GSC_NUM_INTERFACES: i32 = 2;

/// Forward-declared by `intel_gsc.h`; referenced only by pointer.
#[repr(C)]
pub struct MeiAuxDevice {
    _opaque: [u8; 0],
}

/// `GSC_IRQ_INTF(_x)`, where HECI1 and HECI2 map to bits 15 and 14.
pub const fn GSC_IRQ_INTF(x: u32) -> c_ulong {
    1 << (15 - x)
}

/// Nested `struct intel_gsc_intf` record from `struct intel_gsc`.
#[repr(C)]
pub struct IntelGscIntf {
    pub adev: *mut MeiAuxDevice,
    pub gem_obj: *mut DrmI915GemObject,
    pub irq: i32,
    pub id: u32,
}

/// `struct intel_gsc`.
#[repr(C)]
pub struct IntelGsc {
    pub intf: [IntelGscIntf; INTEL_GSC_NUM_INTERFACES as usize],
}

// Out-of-line functions declared by intel_gsc.h.
unsafe extern "C" {
    pub fn intel_gsc_init(gsc: *mut IntelGsc, i915: *mut DrmI915Private);
    pub fn intel_gsc_fini(gsc: *mut IntelGsc);
    pub fn intel_gsc_irq_handler(gt: *mut IntelGt, iir: u32);
}

// x86_64 source ABI: each interface is two pointers followed by int/u32.
const _: [(); 24] = [(); core::mem::size_of::<IntelGscIntf>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelGscIntf>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGscIntf, adev)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelGscIntf, gem_obj)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelGscIntf, irq)];
const _: [(); 20] = [(); core::mem::offset_of!(IntelGscIntf, id)];
const _: [(); 48] = [(); core::mem::size_of::<IntelGsc>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelGsc, intf)];
