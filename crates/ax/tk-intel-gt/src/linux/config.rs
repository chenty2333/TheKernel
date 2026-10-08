// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../LICENSE-MIT.
//
// Linux 7.2.3 configuration values for the x86_64 wt-dev oracle build used by
// the translated i915 GT sources. Keep these tied to that build's auto.conf.

#![allow(non_snake_case, non_upper_case_globals)]

pub const CONFIG_DRM_I915_CAPTURE_ERROR: bool = true;
pub const CONFIG_DRM_I915_DEBUG_GEM: bool = false;
pub const CONFIG_DRM_I915_GVT: bool = false;
pub const CONFIG_DRM_I915_SELFTEST: bool = false;
pub const CONFIG_DRM_I915_SW_FENCE_CHECK_DAG: bool = false;
pub const CONFIG_LOCKDEP: bool = false;
pub const CONFIG_PREEMPT_RT: bool = false;

pub const CONFIG_DRM_I915_HEARTBEAT_INTERVAL: u64 = 2500;
pub const CONFIG_DRM_I915_MAX_REQUEST_BUSYWAIT: u64 = 8000;
pub const CONFIG_DRM_I915_PREEMPT_TIMEOUT: u64 = 640;
pub const CONFIG_DRM_I915_PREEMPT_TIMEOUT_COMPUTE: u64 = 7500;
pub const CONFIG_DRM_I915_STOP_TIMEOUT: u64 = 100;
pub const CONFIG_DRM_I915_TIMESLICE_DURATION: u64 = 1;
pub const BITS_PER_LONG: u32 = 64;
pub const CONFIG_HZ: u32 = 100;
pub const HZ: u32 = CONFIG_HZ;
pub(crate) use axtask::current;
pub const GFP_KERNEL: u32 = 0x0cc0;
pub const GFP_ATOMIC: u32 = 0x0820;
pub const FW_REG_READ: u32 = 1;
pub const FW_REG_WRITE: u32 = 2;

// i915 public-header helpers are re-exported here because source-order
// translations include `i915_drv.h`, `intel_gt.h`, `i915_vma.h`, and
// `i915_reg_defs.h` through one LinuxKPI prelude.
pub(crate) use crate::{
    execlists::EL_CTRL_LOAD,
    guc_submission::{
        CONTEXT_REGISTRATION_FLAG_KMD, PARENT_SCRATCH_SIZE, WQ_GUC_ID_MASK, WQ_RING_TAIL_MASK,
        WQ_STATUS_ACTIVE, WQ_TYPE_MULTI_LRC,
    },
    intel_breadcrumbs_upstream::{
        intel_breadcrumbs_get, intel_breadcrumbs_put, intel_breadcrumbs_reset,
    },
    intel_context_upstream::{
        I915Request, intel_context_enter_engine, intel_context_exit_engine, intel_context_fini,
        intel_context_free, intel_context_get_active_request,
    },
    intel_engine_cs_upstream::{
        intel_engine_cleanup_common, intel_engine_create_virtual, intel_engine_irq_disable,
        intel_engine_irq_enable, intel_engine_set_hwsp_writemask, intel_engine_stop_cs,
        intel_engine_wait_for_pending_mi_fw, xehp_enable_ccs_engines,
    },
    intel_lrc_upstream::{
        lrc_alloc, lrc_check_regs, lrc_fini, lrc_fini_wa_ctx, lrc_init_regs, lrc_init_state,
        lrc_init_wa_ctx, lrc_pin, lrc_post_unpin, lrc_pre_pin, lrc_reset, lrc_reset_regs,
        lrc_unpin, lrc_update_offsets, lrc_update_regs, lrc_update_runtime,
    },
    intel_ring::{PIN_OFFSET_BIAS, intel_engine_create_ring},
    intel_timeline_upstream::{
        intel_timeline_enter, intel_timeline_exit, intel_timeline_reset_seqno,
    },
    intel_workarounds_upstream::{I915McrReg, I915Reg},
    linux::{
        bits::*, contexts::*, fields::*, i915::*, irq::*, primitives::*, rbtree::*, rcu::*,
        registers::*, requests::*, sw_fence::*,
    },
    linux_assert::*,
    linux_i915_private::{
        DrmI915Private, i915_increase_reset_engine_count, i915_reset_count, i915_reset_engine_count,
    },
    linux_list::*,
    linux_locks::*,
    linux_macros::{likely, read_once, unlikely},
    linux_memory::*,
    linux_mutex::*,
    linux_pm::*,
    linux_print::*,
    linux_tasklet::TASKLET_STATE_SCHED,
    linux_timer::*,
    linux_wait::*,
    linux_workqueue::*,
    linux_xarray::*,
    uc::ENABLE_GUC_SUBMISSION,
};

// Linux 7.2.3 errno values and i915 GEM flags/enums used by the imported GT
// sources. `I915_CACHE_LLC` is enum value 1; write-back map mode is 0.
pub const PAGE_SIZE: usize = 4096;
pub const MAX_SCHEDULE_TIMEOUT: u64 = i64::MAX as u64 >> 1;
pub const EAGAIN: i32 = 11;
pub const EINTR: i32 = 4;
pub const ENOMEM: i32 = 12;
pub const EFAULT: i32 = 14;
pub const EBUSY: i32 = 16;
pub const ENODEV: i32 = 19;
pub const EINVAL: i32 = 22;
pub const EPIPE: i32 = 32;
pub const EDEADLK: i32 = 35;
pub const EPROTO: i32 = 71;
pub const EOVERFLOW: i32 = 75;
pub const ETIME: i32 = 62;
pub const ETIMEDOUT: i32 = 110;
pub const EIO: i32 = 5;
pub const ENOENT: i32 = 2;
pub const ENXIO: i32 = 6;
pub const ERANGE: i32 = 34;
pub const ENOSPC: i32 = 28;
pub const I915_CACHE_LLC: u32 = 1;
pub const I915_MAP_WB: u32 = 0;
pub const I915_ENGINE_IS_VIRTUAL: u32 = 1 << 5;
pub const PIN_HIGH: u64 = 1 << 5;
pub const PIN_NOEVICT: u64 = 1 << 0;
pub const PIN_NOSEARCH: u64 = 1 << 1;
pub const PIN_NONBLOCK: u64 = 1 << 2;
pub const PIN_MAPPABLE: u64 = 1 << 3;
pub const PIN_ZONE_4G: u64 = 1 << 4;

#[allow(non_snake_case)]
pub const fn IS_ENABLED(enabled: bool) -> bool {
    enabled
}

#[allow(non_snake_case)]
pub fn IS_ERR<T>(pointer: *const T) -> bool {
    pointer as usize >= usize::MAX - 4094
}

#[allow(non_snake_case)]
pub fn PTR_ERR<T>(pointer: *const T) -> i32 {
    pointer as isize as i32
}

#[allow(non_snake_case)]
pub fn ERR_PTR<T>(error: i32) -> *mut T {
    error as isize as usize as *mut T
}
pub use crate::linux_tasklet::*;
