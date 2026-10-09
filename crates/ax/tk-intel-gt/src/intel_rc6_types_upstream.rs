// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! RC6 records and residency constants from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_rc6_types.h`.

use crate::{intel_context_upstream::DrmI915GemObject, intel_workarounds_types_upstream::I915RegT};

/// `enum intel_rc6_res_type` (C `int` representation).
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntelRc6ResTypeValue {
    Rc6Locked = 0,
    Rc6       = 1,
    Rc6p      = 2,
    Rc6pp     = 3,
    Max       = 4,
}

pub type IntelRc6ResType = i32;
pub const INTEL_RC6_RES_RC6_LOCKED: IntelRc6ResType = 0;
pub const INTEL_RC6_RES_RC6: IntelRc6ResType = 1;
pub const INTEL_RC6_RES_RC6p: IntelRc6ResType = 2;
pub const INTEL_RC6_RES_RC6pp: IntelRc6ResType = 3;
pub const INTEL_RC6_RES_MAX: IntelRc6ResType = 4;
pub const INTEL_RC6_RES_VLV_MEDIA: IntelRc6ResType = INTEL_RC6_RES_RC6p;

/// `struct intel_rc6`.
#[repr(C)]
pub struct IntelRc6 {
    pub res_reg: [I915RegT; INTEL_RC6_RES_MAX as usize],
    pub prev_hw_residency: [u64; INTEL_RC6_RES_MAX as usize],
    pub cur_residency: [u64; INTEL_RC6_RES_MAX as usize],
    pub ctl_enable: u32,
    pub bios_rc_state: u32,
    pub pctx: *mut DrmI915GemObject,
    /// Underlying byte for C `bool:1` flags: supported, enabled, manual,
    /// wakeref, and bios_state_captured occupy bits 0 through 4 respectively.
    pub state_bits: u8,
}

// x86_64 Linux 7.2.3. The previous IntelRc6 opaque binding in
// intel_engine_cs_upstream.rs records size 104/alignment 8.
const _: [(); 4] = [(); core::mem::size_of::<IntelRc6ResTypeValue>()];
const _: [(); 4] = [(); core::mem::size_of::<I915RegT>()];
const _: [(); 104] = [(); core::mem::size_of::<IntelRc6>()];
const _: [(); 8] = [(); core::mem::align_of::<IntelRc6>()];
const _: [(); 0] = [(); core::mem::offset_of!(IntelRc6, res_reg)];
const _: [(); 16] = [(); core::mem::offset_of!(IntelRc6, prev_hw_residency)];
const _: [(); 48] = [(); core::mem::offset_of!(IntelRc6, cur_residency)];
const _: [(); 80] = [(); core::mem::offset_of!(IntelRc6, ctl_enable)];
const _: [(); 84] = [(); core::mem::offset_of!(IntelRc6, bios_rc_state)];
const _: [(); 88] = [(); core::mem::offset_of!(IntelRc6, pctx)];
const _: [(); 96] = [(); core::mem::offset_of!(IntelRc6, state_bits)];
