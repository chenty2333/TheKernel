// SPDX-License-Identifier: MIT
// Copyright © 2017-2018 Intel Corporation.
// Source-order bindings from Linux v7.2.3
// drivers/gpu/drm/i915/gt/intel_wopcm.h.

#[repr(C)]
pub struct IntelWopcmGuc {
    pub base: u32,
    pub size: u32,
}

#[repr(C)]
pub struct IntelWopcm {
    pub size: u32,
    pub guc: IntelWopcmGuc,
}

/// `intel_wopcm_guc_base()`.
pub fn intel_wopcm_guc_base(wopcm: &IntelWopcm) -> u32 {
    wopcm.guc.base
}

/// `intel_wopcm_guc_size()`.
pub fn intel_wopcm_guc_size(wopcm: &IntelWopcm) -> u32 {
    wopcm.guc.size
}

unsafe extern "C" {
    pub fn intel_wopcm_init_early(wopcm: *mut IntelWopcm);
    pub fn intel_wopcm_init(wopcm: *mut IntelWopcm);
}

const _: [(); 8] = [(); core::mem::size_of::<IntelWopcmGuc>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelWopcmGuc, base)];
const _: [(); 4] = [(); core::mem::offset_of!(IntelWopcmGuc, size)];
const _: [(); 12] = [(); core::mem::size_of::<IntelWopcm>()];
const _: [(); 4] = [(); core::mem::align_of::<IntelWopcm>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelWopcm, size)];
const _: [(); 4] = [(); core::mem::offset_of!(IntelWopcm, guc)];
