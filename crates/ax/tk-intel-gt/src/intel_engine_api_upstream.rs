// SPDX-License-Identifier: MIT
// Copyright © 2019 Intel Corporation.
//
//! Engine API declarations and inline helpers from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_engine.h`.
//!
//! `HAS_EXECLISTS` is deliberately not included: it is defined by the separate
//! framework header `i915_drv.h`, not by this MIT-licensed engine header.
//! The `ENGINE_TRACE` wrapper depends on `GEM_TRACE` from `i915_gem.h`; this
//! target has no generic trace sink binding, so it is left at that boundary
//! rather than redirected to an error logger or replaced with a no-op.

#![allow(unsafe_code)]

use core::{
    ffi::{c_char, c_ulong, c_void},
    mem::{offset_of, size_of},
};

use crate::{
    i915_request_types_upstream::{DrmPrinter, I915Request},
    intel_context_types_upstream::{IntelContext, IntelContextOps},
    intel_context_upstream::I915AddressSpace,
    intel_engine_cs_upstream::{ListHead, RbNode, RbRoot},
    intel_engine_types_upstream::{
        BCS0, CCS0, I915_MAX_BCS, I915_MAX_CCS, I915_MAX_RCS, I915_MAX_VCS, I915_MAX_VECS,
        IntelEngineCs, IntelEngineExeclists, IntelEngineMask, IntelInstdone, RCS0, VCS0, VECS0,
        intel_engine_has_preemption, intel_engine_is_virtual,
    },
    intel_gt_types_upstream::{INTEL_SUBMISSION_GUC, IntelGt},
    intel_workarounds_types_upstream::I915RegT,
    linux::fields::KtimeT,
    linux_i915_private::DrmI915Private,
};

#[repr(C)]
pub struct LockClassKey {
    _opaque: [u8; 0],
}

pub const CACHELINE_BYTES: usize = 64;
pub const CACHELINE_DWORDS: usize = CACHELINE_BYTES / size_of::<u32>();

// Source helper macros that token-paste the corresponding uncore operation.
#[macro_export]
macro_rules! __ENGINE_REG_OP {
    (read16, $engine:expr, $reg:expr) => {{ unsafe { $crate::intel_uncore_types_upstream::intel_uncore_read16((*$engine).uncore, $reg) } }};
    (read, $engine:expr, $reg:expr) => {{ unsafe { $crate::intel_uncore_types_upstream::intel_uncore_read((*$engine).uncore, $reg) } }};
    (read_fw, $engine:expr, $reg:expr) => {{
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_read_fw((*$engine).uncore, $reg)
        }
    }};
    (posting_read_fw, $engine:expr, $reg:expr) => {{
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_posting_read_fw(
                (*$engine).uncore,
                $reg,
            )
        }
    }};
    (posting_read16, $engine:expr, $reg:expr) => {{
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_posting_read16(
                (*$engine).uncore,
                $reg,
            )
        }
    }};
    (read64_2x32, $engine:expr, $lower:expr, $upper:expr) => {{
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_read64_2x32(
                (*$engine).uncore,
                $lower,
                $upper,
            )
        }
    }};
    (write16, $engine:expr, $reg:expr, $value:expr) => {{
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_write16(
                (*$engine).uncore,
                $reg,
                $value,
            )
        }
    }};
    (write, $engine:expr, $reg:expr, $value:expr) => {{
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_write((*$engine).uncore, $reg, $value)
        }
    }};
    (write_fw, $engine:expr, $reg:expr, $value:expr) => {{
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_write_fw(
                (*$engine).uncore,
                $reg,
                $value,
            )
        }
    }};
}

#[macro_export]
macro_rules! __ENGINE_READ_OP {
    ($op:ident, $engine:expr, $reg:expr) => {{
        let __engine = $engine;
        $crate::__ENGINE_REG_OP!($op, __engine, ($reg)((*__engine).mmio_base))
    }};
}

#[macro_export]
macro_rules! __ENGINE_WRITE_OP {
    ($op:ident, $engine:expr, $reg:expr, $value:expr) => {{
        let __engine = $engine;
        $crate::__ENGINE_REG_OP!($op, __engine, ($reg)((*__engine).mmio_base), $value)
    }};
}

// Register-operation macros from intel_engine.h. The register designator is
// passed as a Rust callable taking the engine MMIO base (and, for indexed
// registers, an index) and returning the source i915_reg_t value.
#[macro_export]
macro_rules! ENGINE_READ16 {
    ($engine:expr, $reg:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_read16(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base),
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_READ {
    ($engine:expr, $reg:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_read(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base),
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_READ_FW {
    ($engine:expr, $reg:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_read_fw(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base),
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_POSTING_READ {
    ($engine:expr, $reg:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_posting_read_fw(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base),
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_POSTING_READ16 {
    ($engine:expr, $reg:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_posting_read16(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base),
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_READ64 {
    ($engine:expr, $lower_reg:expr, $upper_reg:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_read64_2x32(
                (*__engine).uncore,
                ($lower_reg)((*__engine).mmio_base),
                ($upper_reg)((*__engine).mmio_base),
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_READ_IDX {
    ($engine:expr, $reg:expr, $index:expr) => {{
        let __engine = $engine;
        let __index = $index;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_read(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base, __index),
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_WRITE16 {
    ($engine:expr, $reg:expr, $value:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_write16(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base),
                $value,
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_WRITE {
    ($engine:expr, $reg:expr, $value:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_write(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base),
                $value,
            )
        }
    }};
}
#[macro_export]
macro_rules! ENGINE_WRITE_FW {
    ($engine:expr, $reg:expr, $value:expr) => {{
        let __engine = $engine;
        unsafe {
            $crate::intel_uncore_types_upstream::intel_uncore_write_fw(
                (*__engine).uncore,
                ($reg)((*__engine).mmio_base),
                $value,
            )
        }
    }};
}

#[allow(non_snake_case)]
pub const fn __HAS_ENGINE(engine_mask: IntelEngineMask, id: u32) -> IntelEngineMask {
    engine_mask & (1u32 << id)
}

/// Return the selected contiguous instance bits in `engine_mask`.
#[inline]
pub const fn engine_instances_mask(engine_mask: u32, first: u32, count: u32) -> u32 {
    if count == 0 || first >= u32::BITS || count > u32::BITS - first {
        return 0;
    }
    let mask = if count == u32::BITS {
        u32::MAX
    } else {
        ((1u32 << count) - 1) << first
    };
    (engine_mask & mask) >> first
}

#[macro_export]
macro_rules! __HAS_ENGINE {
    ($mask:expr, $id:expr) => {{ ($mask) & (1u32 << ($id as u32)) }};
}
#[macro_export]
macro_rules! HAS_ENGINE {
    ($gt:expr, $id:expr) => {{ unsafe { $crate::intel_engine_api_upstream::HAS_ENGINE($gt, $id as u32) } }};
}
#[macro_export]
macro_rules! __ENGINE_INSTANCES_MASK {
    ($mask:expr, $first:expr, $count:expr) => {{
        $crate::intel_engine_api_upstream::engine_instances_mask(
            $mask,
            $first as u32,
            $count as u32,
        )
    }};
}
#[macro_export]
macro_rules! ENGINE_INSTANCES_MASK {
    ($gt:expr, $first:expr, $count:expr) => {{
        unsafe {
            $crate::intel_engine_api_upstream::ENGINE_INSTANCES_MASK(
                $gt,
                $first as u32,
                $count as u32,
            )
        }
    }};
}
#[macro_export]
macro_rules! RCS_MASK {
    ($gt:expr) => {{ unsafe { $crate::intel_engine_api_upstream::RCS_MASK($gt) } }};
}
#[macro_export]
macro_rules! BCS_MASK {
    ($gt:expr) => {{ unsafe { $crate::intel_engine_api_upstream::BCS_MASK($gt) } }};
}
#[macro_export]
macro_rules! VDBOX_MASK {
    ($gt:expr) => {{ unsafe { $crate::intel_engine_api_upstream::VDBOX_MASK($gt) } }};
}
#[macro_export]
macro_rules! VEBOX_MASK {
    ($gt:expr) => {{ unsafe { $crate::intel_engine_api_upstream::VEBOX_MASK($gt) } }};
}
#[macro_export]
macro_rules! CCS_MASK {
    ($gt:expr) => {{ unsafe { $crate::intel_engine_api_upstream::CCS_MASK($gt) } }};
}

#[allow(non_snake_case)]
pub unsafe fn HAS_ENGINE(gt: *const IntelGt, id: u32) -> IntelEngineMask {
    assert!(!gt.is_null());
    unsafe { __HAS_ENGINE((*gt).info.engine_mask, id) }
}

#[allow(non_snake_case)]
pub unsafe fn ENGINE_INSTANCES_MASK(gt: *const IntelGt, first: u32, count: u32) -> u32 {
    assert!(!gt.is_null());
    unsafe { engine_instances_mask((*gt).info.engine_mask, first, count) }
}

#[allow(non_snake_case)]
pub unsafe fn RCS_MASK(gt: *const IntelGt) -> u32 {
    unsafe { ENGINE_INSTANCES_MASK(gt, RCS0 as u32, I915_MAX_RCS as u32) }
}
#[allow(non_snake_case)]
pub unsafe fn BCS_MASK(gt: *const IntelGt) -> u32 {
    unsafe { ENGINE_INSTANCES_MASK(gt, BCS0 as u32, I915_MAX_BCS as u32) }
}
#[allow(non_snake_case)]
pub unsafe fn VDBOX_MASK(gt: *const IntelGt) -> u32 {
    unsafe { ENGINE_INSTANCES_MASK(gt, VCS0 as u32, I915_MAX_VCS as u32) }
}
#[allow(non_snake_case)]
pub unsafe fn VEBOX_MASK(gt: *const IntelGt) -> u32 {
    unsafe { ENGINE_INSTANCES_MASK(gt, VECS0 as u32, I915_MAX_VECS as u32) }
}
#[allow(non_snake_case)]
pub unsafe fn CCS_MASK(gt: *const IntelGt) -> u32 {
    unsafe { ENGINE_INSTANCES_MASK(gt, CCS0 as u32, I915_MAX_CCS as u32) }
}

/// `RING_FAULT_REG(engine)` from the included `intel_gt_regs.h`.
#[inline]
pub unsafe fn ring_fault_reg(engine: *const IntelEngineCs) -> I915RegT {
    assert!(!engine.is_null());
    I915RegT {
        reg: 0x4094 + ((*engine).class as u32) * 0x100,
    }
}

#[inline]
pub unsafe fn gen6_ring_fault_reg_read(engine: *const IntelEngineCs) -> u32 {
    assert!(!engine.is_null());
    unsafe {
        crate::intel_uncore_types_upstream::intel_uncore_read(
            (*engine).uncore,
            ring_fault_reg(engine),
        )
    }
}

#[inline]
pub unsafe fn gen6_ring_fault_reg_posting_read(engine: *const IntelEngineCs) {
    assert!(!engine.is_null());
    unsafe {
        crate::intel_uncore_types_upstream::intel_uncore_posting_read(
            (*engine).uncore,
            ring_fault_reg(engine),
        )
    }
}

#[inline]
pub unsafe fn gen6_ring_fault_reg_rmw(engine: *const IntelEngineCs, clear: u32, set: u32) {
    assert!(!engine.is_null());
    unsafe {
        let uncore = (*engine).uncore;
        let reg = ring_fault_reg(engine);
        let value =
            (crate::intel_uncore_types_upstream::intel_uncore_read(uncore, reg) & !clear) | set;
        crate::intel_uncore_types_upstream::intel_uncore_write(uncore, reg, value);
    }
}

#[macro_export]
macro_rules! GEN6_RING_FAULT_REG_READ {
    ($engine:expr) => {{ unsafe { $crate::intel_engine_api_upstream::gen6_ring_fault_reg_read($engine) } }};
}
#[macro_export]
macro_rules! GEN6_RING_FAULT_REG_POSTING_READ {
    ($engine:expr) => {{ unsafe { $crate::intel_engine_api_upstream::gen6_ring_fault_reg_posting_read($engine) } }};
}
#[macro_export]
macro_rules! GEN6_RING_FAULT_REG_RMW {
    ($engine:expr, $clear:expr, $set:expr) => {{ unsafe { $crate::intel_engine_api_upstream::gen6_ring_fault_reg_rmw($engine, $clear, $set) } }};
}

// Status-page slots from intel_engine.h.
pub const I915_GEM_HWS_PREEMPT: u32 = 0x32;
pub const I915_GEM_HWS_PREEMPT_ADDR: usize = I915_GEM_HWS_PREEMPT as usize * size_of::<u32>();
pub const I915_GEM_HWS_SEQNO: u32 = 0x40;
pub const I915_GEM_HWS_SEQNO_ADDR: usize = I915_GEM_HWS_SEQNO as usize * size_of::<u32>();
pub const I915_GEM_HWS_MIGRATE: usize = 0x42 * size_of::<u32>();
pub const I915_GEM_HWS_GGTT_BIND: u32 = 0x46;
pub const I915_GEM_HWS_GGTT_BIND_ADDR: usize = I915_GEM_HWS_GGTT_BIND as usize * size_of::<u32>();
pub const I915_GEM_HWS_PXP: u32 = 0x60;
pub const I915_GEM_HWS_PXP_ADDR: usize = I915_GEM_HWS_PXP as usize * size_of::<u32>();
pub const I915_GEM_HWS_GSC: u32 = 0x62;
pub const I915_GEM_HWS_GSC_ADDR: usize = I915_GEM_HWS_GSC as usize * size_of::<u32>();
pub const I915_GEM_HWS_SCRATCH: u32 = 0x80;

pub const I915_HWS_CSB_BUF0_INDEX: u32 = 0x10;
pub const I915_HWS_CSB_WRITE_INDEX: u32 = 0x1f;
pub const ICL_HWS_CSB_WRITE_INDEX: u32 = 0x2f;
pub unsafe fn intel_hws_csb_write_index(i915: *const DrmI915Private) -> u32 {
    if unsafe { crate::linux::i915::graphics_ver(i915) } >= 11 {
        ICL_HWS_CSB_WRITE_INDEX
    } else {
        I915_HWS_CSB_WRITE_INDEX
    }
}
#[macro_export]
macro_rules! INTEL_HWS_CSB_WRITE_INDEX {
    ($i915:expr) => {{ unsafe { $crate::intel_engine_api_upstream::intel_hws_csb_write_index($i915) as usize } }};
}

// C entry points declared by intel_engine.h.
unsafe extern "C" {
    pub fn drm_clflush_virt_range(addr: *mut c_void, length: c_ulong);
    pub fn intel_engine_stop(engine: *mut IntelEngineCs);
    pub fn intel_engine_cleanup(engine: *mut IntelEngineCs);
    pub fn intel_engines_init_mmio(gt: *mut IntelGt) -> i32;
    pub fn intel_engines_init(gt: *mut IntelGt) -> i32;
    pub fn intel_engine_free_request_pool(engine: *mut IntelEngineCs);
    pub fn intel_engines_release(gt: *mut IntelGt);
    pub fn intel_engines_free(gt: *mut IntelGt);
    pub fn intel_engine_init_common(engine: *mut IntelEngineCs) -> i32;
    pub fn intel_engine_cleanup_common(engine: *mut IntelEngineCs);
    pub fn intel_engine_resume(engine: *mut IntelEngineCs) -> i32;
    pub fn intel_ring_submission_setup(engine: *mut IntelEngineCs) -> i32;
    pub fn intel_engine_stop_cs(engine: *mut IntelEngineCs) -> i32;
    pub fn intel_engine_cancel_stop_cs(engine: *mut IntelEngineCs);
    pub fn intel_engine_wait_for_pending_mi_fw(engine: *mut IntelEngineCs);
    pub fn intel_engine_set_hwsp_writemask(engine: *mut IntelEngineCs, mask: u32);
    pub fn intel_engine_get_active_head(engine: *const IntelEngineCs) -> u64;
    pub fn intel_engine_get_last_batch_head(engine: *const IntelEngineCs) -> u64;
    pub fn intel_engine_get_instdone(engine: *const IntelEngineCs, instdone: *mut IntelInstdone);
    pub fn intel_engine_init_execlists(engine: *mut IntelEngineCs);
    pub fn intel_engine_irq_enable(engine: *mut IntelEngineCs) -> bool;
    pub fn intel_engine_irq_disable(engine: *mut IntelEngineCs);
    pub fn intel_engines_are_idle(gt: *mut IntelGt) -> bool;
    pub fn intel_engine_is_idle(engine: *mut IntelEngineCs) -> bool;
    pub fn __intel_engine_flush_submission(engine: *mut IntelEngineCs, sync: bool);
    pub fn intel_engines_reset_default_submission(gt: *mut IntelGt);
    pub fn intel_engine_can_store_dword(engine: *mut IntelEngineCs) -> bool;
    pub fn intel_engine_dump(
        engine: *mut IntelEngineCs,
        printer: *mut DrmPrinter,
        header: *const c_char,
        ...
    );
    pub fn intel_engine_dump_active_requests(
        requests: *mut ListHead,
        hung_rq: *mut I915Request,
        printer: *mut DrmPrinter,
    );
    pub fn intel_engine_get_busy_time(engine: *mut IntelEngineCs, now: *mut KtimeT) -> KtimeT;
    pub fn intel_engine_get_hung_entity(
        engine: *mut IntelEngineCs,
        ce: *mut *mut IntelContext,
        rq: *mut *mut I915Request,
    );
    pub fn intel_engine_context_size(gt: *mut IntelGt, class: u8) -> u32;
    pub fn intel_engine_create_pinned_context(
        engine: *mut IntelEngineCs,
        vm: *mut I915AddressSpace,
        ring_size: u32,
        hwsp: u32,
        key: *mut LockClassKey,
        name: *const c_char,
    ) -> *mut IntelContext;
    pub fn intel_engine_destroy_pinned_context(ce: *mut IntelContext);
    pub fn xehp_enable_ccs_engines(engine: *mut IntelEngineCs);
    pub fn intel_engine_create_virtual(
        siblings: *mut *mut IntelEngineCs,
        count: u32,
        flags: c_ulong,
    ) -> *mut IntelContext;
    pub fn intel_clamp_heartbeat_interval_ms(engine: *mut IntelEngineCs, value: u64) -> u64;
    pub fn intel_clamp_max_busywait_duration_ns(engine: *mut IntelEngineCs, value: u64) -> u64;
    pub fn intel_clamp_preempt_timeout_ms(engine: *mut IntelEngineCs, value: u64) -> u64;
    pub fn intel_clamp_stop_timeout_ms(engine: *mut IntelEngineCs, value: u64) -> u64;
    pub fn intel_clamp_timeslice_duration_ms(engine: *mut IntelEngineCs, value: u64) -> u64;
}

#[inline]
pub unsafe fn execlists_num_ports(execlists: *const IntelEngineExeclists) -> u32 {
    assert!(!execlists.is_null());
    unsafe { (*execlists).port_mask + 1 }
}

#[inline]
pub unsafe fn execlists_active(execlists: *const IntelEngineExeclists) -> *mut I915Request {
    assert!(!execlists.is_null());
    let mut cur = unsafe { READ_ONCE!((*execlists).active) };
    unsafe { crate::linux::primitives::rmb() };
    loop {
        let old = cur;
        let active = unsafe { READ_ONCE!(*old) };
        cur = unsafe { READ_ONCE!((*execlists).active) };
        unsafe { crate::linux::primitives::rmb() };
        if cur == old {
            return active;
        }
    }
}

#[inline]
pub unsafe fn intel_read_status_page(engine: *const IntelEngineCs, reg: i32) -> u32 {
    assert!(!engine.is_null());
    let addr = unsafe { (*engine).status_page.addr.add(reg as usize) };
    unsafe { READ_ONCE!(*addr) }
}

#[inline]
pub unsafe fn intel_write_status_page(engine: *mut IntelEngineCs, reg: i32, value: u32) {
    assert!(!engine.is_null());
    unsafe {
        let addr = (*engine).status_page.addr.add(reg as usize);
        drm_clflush_virt_range(addr.cast(), size_of::<u32>() as c_ulong);
        WRITE_ONCE!(*addr, value);
        drm_clflush_virt_range(addr.cast(), size_of::<u32>() as c_ulong);
    }
}

#[inline]
pub unsafe fn __intel_engine_reset(engine: *mut IntelEngineCs, stalled: bool) {
    assert!(!engine.is_null());
    unsafe {
        if let Some(rewind) = (*engine).reset.rewind {
            rewind(engine, stalled);
        }
        (*engine).serial = (*engine).serial.wrapping_add(1);
    }
}

#[inline]
pub unsafe fn intel_engine_flush_submission(engine: *mut IntelEngineCs) {
    unsafe { __intel_engine_flush_submission(engine, true) }
}

/// `intel_engine_uses_guc()` from this header, using canonical GT state.
pub unsafe fn intel_engine_uses_guc(engine: *const IntelEngineCs) -> bool {
    assert!(!engine.is_null());
    let gt = unsafe { (*engine).gt };
    assert!(!gt.is_null());
    unsafe { (*gt).submission_method >= INTEL_SUBMISSION_GUC }
}

#[inline]
pub unsafe fn intel_engine_has_preempt_reset(engine: *const IntelEngineCs) -> bool {
    if crate::linux_config::CONFIG_DRM_I915_PREEMPT_TIMEOUT == 0 {
        return false;
    }
    unsafe { intel_engine_has_preemption(engine) }
}

pub const ENGINE_PHYSICAL: i32 = 0;
pub const ENGINE_MOCK: i32 = 1;
pub const ENGINE_VIRTUAL: i32 = 2;

pub const FORCE_VIRTUAL: c_ulong = 1;

#[inline]
pub unsafe fn intel_engine_create_parallel(
    engines: *mut *mut IntelEngineCs,
    num_engines: u32,
    width: u32,
) -> *mut IntelContext {
    unsafe {
        let first = *engines;
        assert!(!first.is_null());
        let ops = (*first).cops;
        assert!(!ops.is_null());
        GEM_BUG_ON!((*ops).create_parallel.is_none());
        (*ops).create_parallel.unwrap()(engines, num_engines, width)
    }
}

#[inline]
pub unsafe fn intel_virtual_engine_has_heartbeat(engine: *const IntelEngineCs) -> bool {
    unsafe {
        GEM_BUG_ON!(!intel_engine_uses_guc(engine));
        crate::intel_guc_submission_types_upstream::intel_guc_virtual_engine_has_heartbeat(engine)
    }
}

#[inline]
pub unsafe fn intel_engine_has_heartbeat(engine: *const IntelEngineCs) -> bool {
    if crate::linux_config::CONFIG_DRM_I915_HEARTBEAT_INTERVAL == 0 {
        return false;
    }
    if unsafe { intel_engine_is_virtual(engine) } {
        unsafe { intel_virtual_engine_has_heartbeat(engine) }
    } else {
        unsafe { READ_ONCE!((*engine).props.heartbeat_interval_ms) != 0 }
    }
}

#[inline]
pub unsafe fn intel_engine_get_sibling(
    engine: *mut IntelEngineCs,
    sibling: u32,
) -> *mut IntelEngineCs {
    assert!(!engine.is_null());
    unsafe {
        GEM_BUG_ON!(!intel_engine_is_virtual(engine));
        let ops = (*engine).cops;
        let get_sibling = (*ops).get_sibling.unwrap_unchecked();
        get_sibling(engine, sibling)
    }
}

#[inline]
pub unsafe fn intel_engine_set_hung_context(engine: *mut IntelEngineCs, ce: *mut IntelContext) {
    assert!(!engine.is_null());
    unsafe { (*engine).hung_ce = ce };
}

#[inline]
pub unsafe fn intel_engine_clear_hung_context(engine: *mut IntelEngineCs) {
    unsafe { intel_engine_set_hung_context(engine, core::ptr::null_mut()) }
}

#[inline]
pub unsafe fn intel_engine_get_hung_context(engine: *const IntelEngineCs) -> *mut IntelContext {
    assert!(!engine.is_null());
    unsafe { (*engine).hung_ce }
}

/// `rb_to_uabi_engine()`; `uabi_node` is the first member of the source union.
#[inline]
pub unsafe fn rb_to_uabi_engine(node: *mut RbNode) -> *mut IntelEngineCs {
    if node.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        node.cast::<u8>()
            .sub(offset_of!(IntelEngineCs, uabi))
            .cast::<IntelEngineCs>()
    }
}

/// Iterate engines in an already-resolved `uabi_engines` root. The source
/// macro extracts this root from `drm_i915_private`; that field is outside the
/// current canonical private-structure binding, so Rust callers pass the root.
#[macro_export]
macro_rules! rb_to_uabi_engine {
    ($node:expr) => {{ unsafe { $crate::intel_engine_api_upstream::rb_to_uabi_engine($node) } }};
}

pub unsafe fn rb_first_uabi_engine(root: *const RbRoot) -> *mut RbNode {
    assert!(!root.is_null());
    let mut node = unsafe { (*root).node };
    while !node.is_null() {
        let left = unsafe { (*node).left };
        if left.is_null() {
            break;
        }
        node = left;
    }
    node
}

#[macro_export]
macro_rules! for_each_uabi_engine {
    ($engine:ident, $root:expr, $body:block) => {{
        let mut __node = unsafe { $crate::intel_engine_api_upstream::rb_first_uabi_engine($root) };
        while !__node.is_null() {
            let $engine = unsafe { $crate::intel_engine_api_upstream::rb_to_uabi_engine(__node) };
            $body
            __node = unsafe { $crate::linux::rbtree::rb_next(__node) };
        }
    }};
}
