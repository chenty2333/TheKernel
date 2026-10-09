// SPDX-License-Identifier: MIT
// Copyright © 2026 TheKernel contributors.
//
// Linux v7.2.3 drivers/gpu/drm/i915/gt/intel_reset.c reset-backoff SRCU
// entry points needed by GuC workers. This is a partial source owner: the
// reset execution paths still need a complete translation before this module
// can replace Linux's reset implementation.

use core::ffi::c_int;

use crate::{
    intel_gt_types_upstream::IntelGt,
    intel_reset_types_upstream::I915_RESET_BACKOFF,
    linux::i915::INTEL_INFO,
    linux::{bits::test_bit, rcu::{rcu_read_lock, rcu_read_unlock}, srcu::SrcuStruct},
};

/// Source `intel_gt_gpu_reset_clobbers_display()` device-info accessor.
pub unsafe fn intel_gt_gpu_reset_clobbers_display(gt: *mut IntelGt) -> bool {
    let info = unsafe { INTEL_INFO((*gt).i915) };
    unsafe { (*info).flags[0] & (1 << 5) != 0 }
}

unsafe extern "C" {
    fn srcu_read_lock(srcu: *mut SrcuStruct) -> c_int;
    fn srcu_read_unlock(srcu: *mut SrcuStruct, idx: c_int);
}

/// Try to enter the GT reset-backoff SRCU domain without waiting.
pub unsafe fn intel_gt_reset_trylock(gt: *mut IntelGt, srcu_idx: *mut c_int) -> c_int {
    let reset = unsafe { &mut (*gt).reset };
    rcu_read_lock();
    if test_bit(I915_RESET_BACKOFF, &reset.flags) {
        rcu_read_unlock();
        return -crate::linux_config::EBUSY;
    }
    unsafe { *srcu_idx = srcu_read_lock(&mut reset.backoff_srcu) };
    rcu_read_unlock();
    0
}

/// Leave the reset-backoff SRCU read-side critical section.
pub unsafe fn intel_gt_reset_unlock(gt: *mut IntelGt, srcu_idx: c_int) {
    unsafe { srcu_read_unlock(&mut (*gt).reset.backoff_srcu, srcu_idx) };
}
