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
pub const GFP_KERNEL: u32 = 0x0cc0;
pub const GFP_ATOMIC: u32 = 0x0820;
pub const FW_REG_READ: u32 = 1;
pub const FW_REG_WRITE: u32 = 2;

// Linux 7.2.3 errno values and i915 GEM flags/enums used by the imported GT
// sources. `I915_CACHE_LLC` is enum value 1; write-back map mode is 0.
pub const PAGE_SIZE: usize = 4096;
pub const MAX_SCHEDULE_TIMEOUT: u64 = i64::MAX as u64 >> 1;
pub const EAGAIN: i32 = 11;
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
pub const I915_CACHE_LLC: u32 = 1;
pub const I915_MAP_WB: u32 = 0;
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
