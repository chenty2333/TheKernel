// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! GT declarations and inline helpers from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_gt.h`.
//!
//! The binding uses canonical IntelGt/uC/engine owner modules. `GT_TRACE` is
//! not redirected: this header delegates it to `GEM_TRACE`, whose trace sink
//! is outside the current Rust binding.

#![allow(unsafe_code)]

use core::ffi::{c_long, c_void};

use crate::{
    i915_request_types_upstream::DrmPrinter,
    intel_context_types_upstream::IntelContext,
    intel_context_upstream::{DrmI915GemObject, I915Vma},
    intel_engine_cs_upstream::WorkStruct,
    intel_engine_types_upstream::{COPY_ENGINE_CLASS, IntelEngineCs, IntelEngineMask},
    intel_gsc_types_upstream::IntelGsc,
    intel_gsc_uc_types_upstream::IntelGscUc,
    intel_gt_buffer_pool_types_upstream::I915MapType,
    intel_gt_defines_types_upstream::I915_MAX_GT,
    intel_gt_types_upstream::{IntelGt, IntelGtInfo, IntelGtScratchField},
    intel_guc_types_upstream::IntelGuc,
    intel_huc_types_upstream::IntelHuc,
    intel_reset_types_upstream::{I915_WEDGED, I915_WEDGED_ON_FINI, I915_WEDGED_ON_INIT},
    intel_uc_types_upstream::IntelUc,
    intel_workarounds_types_upstream::I915RegT,
    linux_i915_private::DrmI915Private,
};

// Source macros at intel_gt.h:20-78. IS_GFX_GT_IP_RANGE / IS_MEDIA_GT_IP_RANGE preserve the source
// BUILD_BUG_ON_ZERO range checks at Rust macro expansion sites.
#[macro_export]
macro_rules! IS_GFX_GT_IP_RANGE {
    ($gt:expr, $from:expr, $until:expr) => {{
        const _: () = assert!($from >= $crate::linux::i915::IP_VER(2, 0));
        const _: () = assert!($until >= $from);
        unsafe { $crate::linux::i915::IS_GFX_GT_IP_RANGE($gt, $from, $until) }
    }};
}
#[macro_export]
macro_rules! IS_MEDIA_GT_IP_RANGE {
    ($gt:expr, $from:expr, $until:expr) => {{
        const _: () = assert!($from >= $crate::linux::i915::IP_VER(13, 0));
        const _: () = assert!($until >= $from);
        unsafe { $crate::linux::i915::IS_MEDIA_GT_IP_RANGE($gt, $from, $until) }
    }};
}
#[macro_export]
macro_rules! IS_GFX_GT_IP_STEP {
    ($gt:expr, $ipver:expr, $from:expr, $until:expr) => {{
        const _: () = assert!($until > $from);
        unsafe { $crate::linux::i915::IS_GFX_GT_IP_STEP($gt, $ipver, $from, $until) }
    }};
}
#[macro_export]
macro_rules! IS_MEDIA_GT_IP_STEP {
    ($gt:expr, $ipver:expr, $from:expr, $until:expr) => {{
        const _: () = assert!($until > $from);
        unsafe { $crate::intel_gt_api_upstream::is_media_gt_ip_step($gt, $ipver, $from, $until) }
    }};
}

// `IS_MEDIA_GT_IP_STEP` inline-equivalent used by the macro above.
#[inline]
pub unsafe fn is_media_gt_ip_step(gt: *mut IntelGt, ip: u16, from: u8, until: u8) -> bool {
    unsafe {
        crate::linux::i915::IS_MEDIA_GT_IP_RANGE(gt, ip, ip)
            && crate::linux::i915::IS_MEDIA_STEP((*gt).i915, from, until)
    }
}

// The source GT_TRACE macro forwards to GEM_TRACE from i915_gem.h. No Rust
// trace-level sink is currently bound, so this module intentionally has no
// fallback/no-op macro of its own.

/// `gt_is_root()` at intel_gt.h:85.
#[inline]
pub unsafe fn gt_is_root(gt: *const IntelGt) -> bool {
    assert!(!gt.is_null());
    unsafe { (*gt).info.id == 0 }
}

// Out-of-line workaround predicates declared at intel_gt.h:90-91.
unsafe extern "C" {
    pub fn intel_gt_needs_wa_16018031267(gt: *mut IntelGt) -> bool;
    pub fn intel_gt_needs_wa_22016122933(gt: *mut IntelGt) -> bool;
}

/// `NEEDS_FASTCOLOR_BLT_WABB()` at intel_gt.h:93.
pub unsafe fn needs_fastcolor_blt_wabb(engine: *const IntelEngineCs) -> bool {
    assert!(!engine.is_null());
    unsafe {
        intel_gt_needs_wa_16018031267((*engine).gt)
            && (*engine).class as i32 == COPY_ENGINE_CLASS
            && (*engine).instance == 0
    }
}
#[macro_export]
macro_rules! NEEDS_FASTCOLOR_BLT_WABB {
    ($engine:expr) => {{ unsafe { $crate::intel_gt_api_upstream::needs_fastcolor_blt_wabb($engine) } }};
}

/// Container conversion helpers at intel_gt.h:97-127.
#[inline]
pub unsafe fn uc_to_gt(uc: *mut IntelUc) -> *mut IntelGt {
    assert!(!uc.is_null());
    unsafe {
        uc.cast::<u8>()
            .sub(core::mem::offset_of!(IntelGt, uc))
            .cast::<IntelGt>()
    }
}
#[inline]
pub unsafe fn guc_to_gt(guc: *mut IntelGuc) -> *mut IntelGt {
    assert!(!guc.is_null());
    unsafe {
        guc.cast::<u8>()
            .sub(core::mem::offset_of!(IntelGt, uc.guc))
            .cast::<IntelGt>()
    }
}
#[inline]
pub unsafe fn huc_to_gt(huc: *mut IntelHuc) -> *mut IntelGt {
    assert!(!huc.is_null());
    unsafe {
        huc.cast::<u8>()
            .sub(core::mem::offset_of!(IntelGt, uc.huc))
            .cast::<IntelGt>()
    }
}
#[inline]
pub unsafe fn gsc_uc_to_gt(gsc_uc: *mut IntelGscUc) -> *mut IntelGt {
    assert!(!gsc_uc.is_null());
    unsafe {
        gsc_uc
            .cast::<u8>()
            .sub(core::mem::offset_of!(IntelGt, uc.gsc))
            .cast::<IntelGt>()
    }
}
#[inline]
pub unsafe fn gsc_to_gt(gsc: *mut IntelGsc) -> *mut IntelGt {
    assert!(!gsc.is_null());
    unsafe {
        gsc.cast::<u8>()
            .sub(core::mem::offset_of!(IntelGt, gsc))
            .cast::<IntelGt>()
    }
}
#[inline]
pub unsafe fn guc_to_i915(guc: *mut IntelGuc) -> *mut DrmI915Private {
    unsafe { (*guc_to_gt(guc)).i915 }
}
#[inline]
pub unsafe fn gt_to_guc(gt: *mut IntelGt) -> *mut IntelGuc {
    assert!(!gt.is_null());
    unsafe { core::ptr::addr_of_mut!((*gt).uc.guc) }
}

// C declarations in source order at intel_gt.h:132-153.
unsafe extern "C" {
    pub fn intel_gt_common_init_early(gt: *mut IntelGt);
    pub fn intel_root_gt_init_early(i915: *mut DrmI915Private) -> i32;
    pub fn intel_gt_assign_ggtt(gt: *mut IntelGt) -> i32;
    pub fn intel_gt_init_mmio(gt: *mut IntelGt) -> i32;
    #[must_use]
    pub fn intel_gt_init_hw(gt: *mut IntelGt) -> i32;
    pub fn intel_gt_init(gt: *mut IntelGt) -> i32;
    pub fn intel_gt_driver_register(gt: *mut IntelGt);
    pub fn intel_gt_driver_unregister(gt: *mut IntelGt);
    pub fn intel_gt_driver_remove(gt: *mut IntelGt);
    pub fn intel_gt_driver_release(gt: *mut IntelGt);
    pub fn intel_gt_driver_late_release_all(i915: *mut DrmI915Private);
    pub fn intel_gt_wait_for_idle(gt: *mut IntelGt, timeout: c_long) -> i32;
    pub fn intel_gt_check_and_clear_faults(gt: *mut IntelGt);
    pub fn intel_gt_perf_limit_reasons_reg(gt: *mut IntelGt) -> I915RegT;
    pub fn intel_gt_clear_error_registers(gt: *mut IntelGt, engine_mask: IntelEngineMask);
    pub fn intel_gt_flush_ggtt_writes(gt: *mut IntelGt);
    pub fn intel_gt_chipset_flush(gt: *mut IntelGt);
}

/// `intel_gt_scratch_offset()` at intel_gt.h:155.
#[inline]
pub unsafe fn intel_gt_scratch_offset(gt: *const IntelGt, field: IntelGtScratchField) -> u32 {
    assert!(!gt.is_null());
    unsafe { crate::linux::i915::i915_ggtt_offset((*gt).scratch).wrapping_add(field as u32) }
}

/// `intel_gt_has_unrecoverable_error()` at intel_gt.h:161.
#[inline]
pub unsafe fn intel_gt_has_unrecoverable_error(gt: *const IntelGt) -> bool {
    assert!(!gt.is_null());
    unsafe {
        crate::linux::bits::test_bit(I915_WEDGED_ON_INIT, &(*gt).reset.flags)
            || crate::linux::bits::test_bit(I915_WEDGED_ON_FINI, &(*gt).reset.flags)
    }
}

/// `intel_gt_is_wedged()` at intel_gt.h:167.
#[inline]
pub unsafe fn intel_gt_is_wedged(gt: *const IntelGt) -> bool {
    assert!(!gt.is_null());
    unsafe {
        GEM_BUG_ON!(
            intel_gt_has_unrecoverable_error(gt)
                && !crate::linux::bits::test_bit(I915_WEDGED, &(*gt).reset.flags)
        );
        crate::linux::bits::test_bit(I915_WEDGED, &(*gt).reset.flags)
    }
}

// Probe declarations at intel_gt.h:175-176.
unsafe extern "C" {
    pub fn intel_gt_probe_all(i915: *mut DrmI915Private) -> i32;
    pub fn intel_gt_tiles_init(i915: *mut DrmI915Private) -> i32;
}

// Iteration macros at intel_gt.h:178-196. Rust forms take an explicit body
// block and preserve the source's populated-slot/selected-mask iteration.
#[macro_export]
macro_rules! for_each_gt {
    ($gt:ident, $i915:expr, $id:ident, $body:block) => {{
        let __i915 = $i915;
        let mut $id: usize = 0;
        while $id < $crate::intel_gt_types_upstream::I915_MAX_GT {
            let $gt = unsafe { (*__i915).gt[$id] };
            if !$gt.is_null() {
                $body
            }
            $id += 1;
        }
    }};
}
#[macro_export]
macro_rules! for_each_engine {
    ($engine:ident, $gt:expr, $id:ident, $body:block) => {{
        let __gt = $gt;
        let mut $id: usize = 0;
        while $id < $crate::intel_engine_types_upstream::I915_NUM_ENGINES as usize {
            let $engine = unsafe { (*__gt).engine[$id] };
            if !$engine.is_null() {
                $body
            }
            $id += 1;
        }
    }};
}
#[macro_export]
macro_rules! for_each_engine_masked {
    ($engine:ident, $gt:expr, $mask:expr, $tmp:ident, $body:block) => {{
        let __gt = $gt;
        let mut $tmp = $mask & unsafe { (*__gt).info.engine_mask };
        while $tmp != 0 {
            let __index = $tmp.trailing_zeros() as usize;
            $tmp &= !(1u32 << __index);
            let $engine = unsafe { (*__gt).engine[__index] };
            if !$engine.is_null() {
                $body
            }
        }
    }};
}

// Information, watchdog, map-type, and bind-context declarations at
// intel_gt.h:198-209.
unsafe extern "C" {
    pub fn intel_gt_info_print(info: *const IntelGtInfo, printer: *mut DrmPrinter);
    pub fn intel_gt_watchdog_work(work: *mut WorkStruct);
    pub fn intel_gt_coherent_map_type(
        gt: *mut IntelGt,
        obj: *mut DrmI915GemObject,
        always_coherent: bool,
    ) -> I915MapType;
    pub fn intel_gt_bind_context_set_ready(gt: *mut IntelGt);
    pub fn intel_gt_bind_context_set_unready(gt: *mut IntelGt);
    pub fn intel_gt_is_bind_context_ready(gt: *mut IntelGt) -> bool;
}

#[inline]
pub unsafe fn intel_gt_set_wedged_async(gt: *mut IntelGt) {
    assert!(!gt.is_null());
    unsafe {
        let _ = crate::linux::workqueue::queue_work(
            crate::linux::workqueue::system_highpri_wq,
            core::ptr::addr_of_mut!((*gt).wedge),
        );
    }
}
