// SPDX-License-Identifier: MIT
// Copyright © 2025 Intel Corporation
// Source: Linux v7.2.3 drivers/gpu/drm/i915/i915_edram.c (MIT).
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use crate::{
    linux_print::{CFormatArg, DrmLogLevel, drm_log_at, format_message},
    intel_uncore_types_upstream::{IntelUncore, intel_uncore_read_fw},
    intel_workarounds_types_upstream::I915RegT,
    linux::i915::{GRAPHICS_VER, IS_BROADWELL, IS_HASWELL},
    linux_i915_private::DrmI915Private,
};

// i915_reg.h: HSW_EDRAM_CAP and the EDRAM_* field macros.
const HSW_EDRAM_CAP: I915RegT = I915RegT { reg: 0x120010 };
const EDRAM_ENABLED: u32 = 0x1;

const fn EDRAM_NUM_BANKS(cap: u32) -> u32 {
    (cap >> 1) & 0xf
}

const fn EDRAM_WAYS_IDX(cap: u32) -> usize {
    ((cap >> 5) & 0x7) as usize
}

const fn EDRAM_SETS_IDX(cap: u32) -> usize {
    ((cap >> 8) & 0x3) as usize
}

/// `drm_info()` for this file, as in i915_driver_upstream.rs.
macro_rules! drm_info {
    ($device:expr, $format:expr $(, $argument:expr)* $(,)?) => {{
        let _ = $device;
        let __args: &[&dyn CFormatArg] = &[$(&($argument) as &dyn CFormatArg),*];
        let __message = format_message($format, __args);
        drm_log_at(DrmLogLevel::Info, "i915 DRM info", file!(), line!(), &__message);
    }};
}

// upstream: i915_edram.c gen9_edram_size_mb()
fn gen9_edram_size_mb(cap: u32) -> u32 {
    static WAYS: [u8; 8] = [4, 8, 12, 16, 16, 16, 16, 16];
    static SETS: [u8; 4] = [1, 1, 2, 2];
    EDRAM_NUM_BANKS(cap) * WAYS[EDRAM_WAYS_IDX(cap)] as u32 * SETS[EDRAM_SETS_IDX(cap)] as u32
}

// upstream: i915_edram.c i915_edram_detect()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn i915_edram_detect(i915: *mut DrmI915Private) {
    unsafe {
        if !(IS_HASWELL(i915) || IS_BROADWELL(i915) || GRAPHICS_VER(i915) >= 9) {
            return;
        }

        let uncore = core::ptr::addr_of!((*i915).uncore).cast::<IntelUncore>();
        let edram_cap = intel_uncore_read_fw(uncore, HSW_EDRAM_CAP);

        // NB: We can't write IDICR yet because we don't have gt funcs set up.
        if edram_cap & EDRAM_ENABLED == 0 {
            return;
        }

        // The capability bits needed for the size are absent before gen9, so
        // pre-gen9 always reports 128MB.
        (*i915).edram_size_mb = if GRAPHICS_VER(i915) < 9 {
            128
        } else {
            gen9_edram_size_mb(edram_cap)
        };

        let size = (*i915).edram_size_mb;
        drm_info!(&(*i915).drm, "Found %uMB of eDRAM\n", size);
    }
}
