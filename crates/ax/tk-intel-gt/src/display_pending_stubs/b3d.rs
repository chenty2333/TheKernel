// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//! TEMPORARY definitions for callees owned by batch B3d (BW, display IRQ, global state, CMTG).
//! Replaced by intel_bw_upstream.rs, intel_display_irq_upstream.rs,
//! intel_global_state_upstream.rs, intel_dbuf_bw_upstream.rs (B3d).

#![allow(non_snake_case, dead_code, unsafe_code)]

use crate::intel_display_core_types_upstream::IntelDisplay;

// upstream: intel_global_state.c intel_atomic_global_obj_cleanup()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_global_obj_cleanup(_display: *mut IntelDisplay) {}

// upstream: intel_global_state.c intel_atomic_global_obj_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_atomic_global_obj_init(
    _display: *mut IntelDisplay,
    _obj: *mut crate::intel_global_state_types_upstream::IntelGlobalObj,
    _state: *mut crate::intel_global_state_types_upstream::IntelGlobalState,
    _funcs: *const crate::intel_global_state_types_upstream::IntelGlobalStateFuncs,
) {}

// upstream: intel_display_irq.c intel_display_irq_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_display_irq_init(_display: *mut IntelDisplay) {}

// upstream: intel_bw.c intel_dbuf_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_dbuf_bw.c intel_dbuf_bw_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_dbuf_bw_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_bw.c intel_bw_init()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_bw_init(_display: *mut IntelDisplay) -> i32 {
    0
}

// upstream: intel_display_irq.c ilk_enable_display_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_enable_display_irq(_display: *mut IntelDisplay, _mask: u32) {}

// upstream: intel_display_irq.c ilk_disable_display_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ilk_disable_display_irq(_display: *mut IntelDisplay, _mask: u32) {}

// upstream: intel_display_irq.c gen8_irq_power_well_post_enable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_irq_power_well_post_enable(
    _display: *mut IntelDisplay,
    _pipe_mask: u8,
) {}

// upstream: intel_display_irq.c gen8_irq_power_well_pre_disable()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gen8_irq_power_well_pre_disable(
    _display: *mut IntelDisplay,
    _pipe_mask: u8,
) {}
