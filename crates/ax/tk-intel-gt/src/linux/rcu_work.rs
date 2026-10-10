// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Layout-only LinuxKPI binding for the kernel's deferred RCU-work handle.
//! This does not provide a workqueue/RCU implementation; execution is owned by
//! the runtime's kernel-work integration.

use core::ffi::c_void;

use crate::{intel_context_upstream::RcuHead, intel_engine_cs_upstream::WorkStruct};

#[repr(C)]
pub struct RcuWork {
    pub work: WorkStruct,
    pub rcu: RcuHead,
    pub wq: *mut c_void,
}

const _: [(); 56] = [(); core::mem::size_of::<RcuWork>()];
const _: [(); 8] = [(); core::mem::align_of::<RcuWork>()];

/// Linux `rcu_work_rcufn()`: runs after the grace period and queues the work.
unsafe extern "C" fn rcu_work_rcufn(rcu: *mut RcuHead) {
    let rwork = unsafe { rcu.cast::<u8>().sub(core::mem::offset_of!(RcuWork, rcu)) }
        .cast::<RcuWork>();
    unsafe { crate::linux_workqueue::enqueue_marked_work(core::ptr::addr_of_mut!((*rwork).work)) };
}

/// Linux `queue_rcu_work()`: queue `rwork` on `wq` after an RCU grace period.
/// Returns false when the work is already pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn queue_rcu_work(wq: *mut c_void, rwork: *mut RcuWork) -> bool {
    assert!(!rwork.is_null());
    if !unsafe { crate::linux_workqueue::mark_work_pending(core::ptr::addr_of_mut!((*rwork).work)) } {
        return false;
    }
    unsafe { (*rwork).wq = wq };
    crate::linux::rcu::call_rcu(core::ptr::addr_of_mut!((*rwork).rcu), rcu_work_rcufn);
    true
}
