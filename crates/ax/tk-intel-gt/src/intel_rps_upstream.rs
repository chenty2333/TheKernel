// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation.
//
//! Source-order translations of the raw-frequency accessors from Linux
//! v7.2.3 `drivers/gpu/drm/i915/gt/intel_rps.c`.

#![allow(unsafe_code)]

use crate::{
    intel_gt_types_upstream::IntelGt,
    intel_guc_slpc_types_upstream::IntelGucSlpc,
    intel_rps_types_upstream::IntelRps,
    intel_uc_types_upstream::intel_uc_uses_guc_slpc,
    linux::i915::GRAPHICS_VER,
};

const GT_FREQUENCY_MULTIPLIER: u32 = 50;
const GEN9_FREQ_SCALER: u8 = 3;

// upstream: intel_rps.c rps_to_gt()
unsafe fn rps_to_gt(rps: *mut IntelRps) -> *mut IntelGt {
    container_of!(rps, IntelGt, rps)
}

// upstream: intel_rps.c rps_to_slpc()
unsafe fn rps_to_slpc(rps: *mut IntelRps) -> *mut IntelGucSlpc {
    let gt = unsafe { rps_to_gt(rps) };
    core::ptr::addr_of_mut!((*gt).uc.guc.slpc)
}

// upstream: intel_rps.c rps_uses_slpc()
unsafe fn rps_uses_slpc(rps: *mut IntelRps) -> bool {
    let gt = unsafe { rps_to_gt(rps) };
    intel_uc_uses_guc_slpc(core::ptr::addr_of_mut!((*gt).uc))
}

// upstream: intel_rps.c intel_rps_get_max_raw_freq()
pub unsafe fn intel_rps_get_max_raw_freq(rps: *mut IntelRps) -> u32 {
    if unsafe { rps_uses_slpc(rps) } {
        let freq = unsafe { (*rps_to_slpc(rps)).rp0_freq };
        return (freq + GT_FREQUENCY_MULTIPLIER / 2) / GT_FREQUENCY_MULTIPLIER;
    }

    let freq = unsafe { (*rps).max_freq };
    if unsafe { GRAPHICS_VER((*rps_to_gt(rps)).i915) } >= 9 {
        (freq / GEN9_FREQ_SCALER) as u32
    } else {
        freq as u32
    }
}

// upstream: intel_rps.c intel_rps_get_min_raw_freq()
pub unsafe fn intel_rps_get_min_raw_freq(rps: *mut IntelRps) -> u32 {
    if unsafe { rps_uses_slpc(rps) } {
        let freq = unsafe { (*rps_to_slpc(rps)).min_freq };
        return (freq + GT_FREQUENCY_MULTIPLIER / 2) / GT_FREQUENCY_MULTIPLIER;
    }

    let freq = unsafe { (*rps).min_freq };
    if unsafe { GRAPHICS_VER((*rps_to_gt(rps)).i915) } >= 9 {
        (freq / GEN9_FREQ_SCALER) as u32
    } else {
        freq as u32
    }
}
