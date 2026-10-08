// SPDX-License-Identifier: MIT
// Copyright © 2014 Intel Corporation.
//
//! ABI constants and inline helpers transcribed from Linux v7.2.3
//! `drivers/gpu/drm/i915/gt/intel_lrc.h`.
//!
//! That header declares no i915-owned struct, union, or typedef. Register
//! state definitions from `intel_lrc_reg.h` are intentionally excluded.
//! Non-inline C function declarations map to the implementations in
//! `intel_lrc_upstream.rs` rather than duplicate entry points here.

use crate::{
    intel_context_types_upstream::{CONTEXT_BARRIER_BIT, IntelContext},
    intel_ring::PAGE_SIZE,
};

// At the start of the context image is its per-process HWS page.
pub const LRC_PPHWSP_PN: usize = 0;
pub const LRC_PPHWSP_SZ: usize = 1;
// After the PPHWSP we have the logical state for the context.
pub const LRC_STATE_PN: usize = LRC_PPHWSP_PN + LRC_PPHWSP_SZ;
pub const LRC_STATE_OFFSET: usize = LRC_STATE_PN * PAGE_SIZE;

// Space within PPHWSP reserved to be used as scratch.
pub const LRC_PPHWSP_SCRATCH: usize = 0x34;
pub const LRC_PPHWSP_SCRATCH_ADDR: usize = LRC_PPHWSP_SCRATCH * core::mem::size_of::<u32>();

// The two anonymous C enums have int representation. Keep the source enum
// value sets while exposing i32 constants so all C enum bit patterns remain
// representable at the FFI boundary.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LrcContextTypeValue {
    Advanced   = 0,
    Legacy32B  = 1,
    AdvancedAd = 2,
    Legacy64B  = 3,
}
pub type LrcContextType = i32;
pub const INTEL_ADVANCED_CONTEXT: LrcContextType = LrcContextTypeValue::Advanced as i32;
pub const INTEL_LEGACY_32B_CONTEXT: LrcContextType = LrcContextTypeValue::Legacy32B as i32;
pub const INTEL_ADVANCED_AD_CONTEXT: LrcContextType = LrcContextTypeValue::AdvancedAd as i32;
pub const INTEL_LEGACY_64B_CONTEXT: LrcContextType = LrcContextTypeValue::Legacy64B as i32;

#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LrcFaultModeValue {
    Hang     = 0,
    Halt     = 1,
    Stream   = 2,
    Continue = 3,
}
pub type LrcFaultMode = i32;
pub const FAULT_AND_HANG: LrcFaultMode = LrcFaultModeValue::Hang as i32;
pub const FAULT_AND_HALT: LrcFaultMode = LrcFaultModeValue::Halt as i32;
pub const FAULT_AND_STREAM: LrcFaultMode = LrcFaultModeValue::Stream as i32;
pub const FAULT_AND_CONTINUE: LrcFaultMode = LrcFaultModeValue::Continue as i32;

// GEN8+ context descriptor and register state bits from intel_lrc.h.
pub const CTX_GTT_ADDRESS_MASK: u32 = 0xffff_f000;
pub const GEN8_CTX_VALID: u32 = 1 << 0;
pub const GEN8_CTX_FORCE_PD_RESTORE: u32 = 1 << 1;
pub const GEN8_CTX_FORCE_RESTORE: u32 = 1 << 2;
pub const GEN8_CTX_L3LLC_COHERENT: u32 = 1 << 5;
pub const GEN8_CTX_PRIVILEGE: u32 = 1 << 8;
pub const GEN8_CTX_ADDRESSING_MODE_SHIFT: u32 = 3;

pub const GEN12_CTX_PRIORITY_MASK: u32 = 0x600;
pub const GEN12_CTX_PRIORITY_HIGH: u32 = (2 << 9) & GEN12_CTX_PRIORITY_MASK;
pub const GEN12_CTX_PRIORITY_NORMAL: u32 = (1 << 9) & GEN12_CTX_PRIORITY_MASK;
pub const GEN12_CTX_PRIORITY_LOW: u32 = (0 << 9) & GEN12_CTX_PRIORITY_MASK;

pub const GEN8_CTX_ID_SHIFT: u32 = 32;
pub const GEN8_CTX_ID_WIDTH: u32 = 21;
pub const GEN11_SW_CTX_ID_SHIFT: u32 = 37;
pub const GEN11_SW_CTX_ID_WIDTH: u32 = 11;
pub const GEN11_ENGINE_CLASS_SHIFT: u32 = 61;
pub const GEN11_ENGINE_CLASS_WIDTH: u32 = 3;
pub const GEN11_ENGINE_INSTANCE_SHIFT: u32 = 48;
pub const GEN11_ENGINE_INSTANCE_WIDTH: u32 = 6;
pub const XEHP_SW_CTX_ID_SHIFT: u32 = 39;
pub const XEHP_SW_CTX_ID_WIDTH: u32 = 16;
pub const XEHP_SW_COUNTER_SHIFT: u32 = 58;
pub const XEHP_SW_COUNTER_WIDTH: u32 = 6;
pub const GEN12_GUC_SW_CTX_ID_SHIFT: u32 = 39;
pub const GEN12_GUC_SW_CTX_ID_WIDTH: u32 = 16;

// Debug predicate-buffer placement from intel_lrc.h, not intel_lrc_reg.h.
pub const DG2_PREDICATE_RESULT_WA: usize = PAGE_SIZE - core::mem::size_of::<u64>();
pub const DG2_PREDICATE_RESULT_BB: usize = 2048;

#[inline]
pub unsafe fn lrc_runtime_start(ce: *mut IntelContext) {
    if (*ce).flags & (1 << CONTEXT_BARRIER_BIT) != 0 {
        return;
    }

    if (*ce).stats.active != 0 {
        return;
    }

    // WRITE_ONCE(stats->active, intel_context_clock()), where the source
    // inline uses ktime_get_raw_fast_ns(). Keep this on the raw-fast binding,
    // not the adjusted realtime clock.
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*ce).stats.active),
        crate::linux::primitives::ktime_get_raw_fast_ns(),
    );
}

#[inline]
pub unsafe fn lrc_runtime_stop(ce: *mut IntelContext) {
    if (*ce).stats.active == 0 {
        return;
    }

    crate::intel_lrc_upstream::lrc_update_runtime(ce);
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*ce).stats.active), 0);
}

const _: [(); 4] = [(); core::mem::size_of::<LrcContextTypeValue>()];
const _: [(); 4] = [(); core::mem::size_of::<LrcFaultModeValue>()];
