// SPDX-License-Identifier: MIT
// Copyright © 2021 Intel Corporation.
// Source-order translation of Linux 7.2.3 drivers/gpu/drm/i915/intel_clock_gating.c
// for the dispatch that TheKernel's Gen12 (ADL-N) target reaches.
//
// Linux `intel_clock_gating_hooks_init()` selects one `clock_gating_funcs` table
// with an ordered predicate chain. Gen12 platforms other than DG2 match none of
// the explicit entries and select `nop_clock_gating_funcs`; ADL-N is one of them.
// The per-platform bodies for DG2 (discrete GPU) and pre-Gen12 platforms are not
// translated because TheKernel only admits ADL-N; reaching them fails closed.
#![allow(non_snake_case, unsafe_op_in_unsafe_fn)]

use core::ffi::c_void;

use crate::linux::i915::{GRAPHICS_VER, IS_DG2, to_i915};

// upstream: intel_clock_gating.c nop_init_clock_gating()
//
// Linux emits only `drm_dbg_kms("No clock gating settings or workarounds
// applied.")`, which is a debug-only message and changes no hardware state.
unsafe fn nop_init_clock_gating() {}

// upstream: intel_clock_gating.c intel_clock_gating_hooks_init() (Gen12 subset)
unsafe fn clock_gating_funcs_is_nop(i915: *mut crate::linux_i915_private::DrmI915Private) -> bool {
    if unsafe { IS_DG2(i915) } {
        panic!("intel_clock_gating: DG2 (discrete GPU) clock gating is not supported by TheKernel");
    }
    if unsafe { GRAPHICS_VER(i915) } >= 12 {
        return true;
    }
    panic!("intel_clock_gating: pre-Gen12 clock gating is not supported by TheKernel");
}

// upstream: intel_clock_gating.c intel_clock_gating_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_clock_gating_init(dev: *mut c_void) {
    let i915 = unsafe { to_i915(dev) };
    if unsafe { clock_gating_funcs_is_nop(i915) } {
        unsafe { nop_init_clock_gating() };
    }
}
