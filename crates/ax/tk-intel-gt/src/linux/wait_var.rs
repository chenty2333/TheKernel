// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//
// i915 active-reference wait: the Linux `wait_var_event_interruptible()` shape
// whose predicate is the i915 active-tracker idle check. The generic wait
// queue, signal and scheduler primitives come from tk-linux-kapi.

use core::ffi::c_void;

use tk_linux_kapi::{
    config::{EINTR, MAX_SCHEDULE_TIMEOUT},
    signal,
    types::{ListHead, WaitQueueEntry},
    wait::{
        TASK_INTERRUPTIBLE, __var_waitqueue, default_wake_function, finish_wait, prepare_to_wait,
        schedule_timeout, wake_up_all,
    },
};

/// Linux `wait_var_event_interruptible()` for the i915 active-reference wait.
/// The predicate is the active-reference idle check on `addr`; the sleep is
/// woken by `wake_up_var()` and re-checks the predicate after every wake.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn linux_wait_var_event_interruptible(addr: *mut c_void, state: i32) -> i32 {
    let wq = __var_waitqueue(addr);
    let mut entry = WaitQueueEntry {
        flags: 0,
        private: core::ptr::null_mut(),
        func: Some(default_wake_function),
        entry: ListHead {
            next: core::ptr::null_mut(),
            prev: core::ptr::null_mut(),
        },
    };
    loop {
        unsafe { prepare_to_wait(wq, &mut entry, state) };
        if state as u32 == TASK_INTERRUPTIBLE && tk_linux_kapi::signal::signal_pending_current() {
            unsafe { finish_wait(wq, &mut entry) };
            return -EINTR;
        }
        if crate::linux::requests::i915_active_is_idle(
            addr.cast_const().cast::<crate::intel_context_types_upstream::I915Active>(),
        ) {
            unsafe { finish_wait(wq, &mut entry) };
            return 0;
        }
        let _ = schedule_timeout(MAX_SCHEDULE_TIMEOUT as i64);
        unsafe { finish_wait(wq, &mut entry) };
    }
}

