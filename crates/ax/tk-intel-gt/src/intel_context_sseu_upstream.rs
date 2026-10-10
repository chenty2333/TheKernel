// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//! Linux 7.2.3 `drivers/gpu/drm/i915/gt/intel_context_sseu.c` translation.

#![allow(unsafe_code)]

use crate::{
    intel_context_api_upstream::{
        intel_context_lock_pinned, intel_context_pin_if_active, intel_context_unlock_pinned,
        intel_context_unpin,
    },
    intel_context_types_upstream::IntelContext,
    intel_engine_cs_upstream::IntelEngineCs,
    intel_lrc_reg_types_upstream::CTX_R_PWR_CLK_STATE,
    intel_lrc_types_upstream::LRC_STATE_OFFSET,
    intel_sseu_types_upstream::IntelSseu,
    i915_request_types_upstream::I915Request,
    linux::{i915::i915_ggtt_offset, pm::{intel_engine_pm_get, intel_engine_pm_put}},
};

// upstream: intel_engine_pm.h intel_engine_create_kernel_request()
unsafe fn intel_engine_create_kernel_request(engine: *mut IntelEngineCs) -> *mut I915Request {
    unsafe { intel_engine_pm_get(engine) };
    let request = unsafe {
        crate::i915_request_upstream::i915_request_create((*engine).kernel_context)
    };
    unsafe { intel_engine_pm_put(engine) };
    request
}

// upstream: intel_context_sseu.c gen8_emit_rpcs_config()
unsafe fn gen8_emit_rpcs_config(
    request: *mut I915Request,
    context: *const IntelContext,
    sseu: *const IntelSseu,
) -> i32 {
    let mut cs = unsafe { crate::intel_ring_upstream::intel_ring_begin(request, 4) };
    if IS_ERR!(cs) {
        return PTR_ERR!(cs);
    }

    let offset = unsafe {
        u64::from(i915_ggtt_offset((*context).state))
            + LRC_STATE_OFFSET as u64
            + (CTX_R_PWR_CLK_STATE * 4) as u64
    };
    unsafe {
        *cs = crate::linux::registers::MI_STORE_DWORD_IMM_GEN4
            | crate::linux::registers::MI_USE_GGTT;
        cs = cs.add(1);
        *cs = offset as u32;
        cs = cs.add(1);
        *cs = (offset >> 32) as u32;
        cs = cs.add(1);
        *cs = crate::intel_sseu_upstream::intel_sseu_make_rpcs(
            (*(*request).engine).gt,
            sseu,
        );
        cs = cs.add(1);
        crate::linux::requests::intel_ring_advance(request, cs);
    }
    0
}

// upstream: intel_context_sseu.c gen8_modify_rpcs()
unsafe fn gen8_modify_rpcs(context: *mut IntelContext, sseu: *const IntelSseu) -> i32 {
    lockdep_assert_held!(unsafe { &(*context).pin_mutex });
    if !unsafe { intel_context_pin_if_active(context) } {
        return 0;
    }

    let request = unsafe { intel_engine_create_kernel_request((*context).engine) };
    if IS_ERR!(request) {
        let result = PTR_ERR!(request);
        unsafe { intel_context_unpin(context) };
        return result;
    }

    let mut result = unsafe { crate::intel_context_upstream::intel_context_prepare_remote_request(context, request) };
    if result == 0 {
        result = unsafe { gen8_emit_rpcs_config(request, context, sseu) };
    }
    unsafe { crate::i915_request_upstream::i915_request_add(request) };
    unsafe { intel_context_unpin(context) };
    result
}

// upstream: intel_context_sseu.c intel_context_reconfigure_sseu()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn intel_context_reconfigure_sseu(
    context: *mut IntelContext,
    sseu: IntelSseu,
) -> i32 {
    GEM_BUG_ON!(unsafe { crate::linux::i915::GRAPHICS_VER((*(*context).engine).i915) < 8 });

    let mut result = unsafe { intel_context_lock_pinned(context) };
    if result != 0 {
        return result;
    }

    if unsafe { (*context).sseu == sseu } {
        unsafe { intel_context_unlock_pinned(context) };
        return result;
    }
    result = unsafe { gen8_modify_rpcs(context, &sseu) };
    if result == 0 {
        unsafe { (*context).sseu = sseu };
    }
    unsafe { intel_context_unlock_pinned(context) };
    result
}
