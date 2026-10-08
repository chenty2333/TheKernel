// SPDX-License-Identifier: MIT
// Copyright © 2022 Intel Corporation.
// Source-order bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_hwconfig.h.

use core::ffi::c_void;

use crate::intel_gt_types_upstream::IntelGt;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IntelHwconfig {
    pub size: u32,
    pub ptr: *mut c_void,
}

unsafe extern "C" {
    pub fn intel_gt_init_hwconfig(gt: *mut IntelGt) -> i32;
    pub fn intel_gt_fini_hwconfig(gt: *mut IntelGt);
}

const _: [(); 16] = [(); core::mem::size_of::<IntelHwconfig>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelHwconfig>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelHwconfig, size)];
const _: [(); 8] = [(); core::mem::offset_of!(IntelHwconfig, ptr)];
