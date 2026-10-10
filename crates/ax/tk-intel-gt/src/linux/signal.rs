// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../../LICENSE-MIT.
//! Linux task-signal predicate adapter.
//!
//! The process signal owner lives above this crate. It installs the callback
//! that maps Linux task state and pending-signal policy to the native task;
//! calling the LinuxKPI entry point before that owner is installed fails
//! closed rather than treating a pending signal as absent.

#![allow(unsafe_code)]

use core::{
    ffi::c_void,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};
use alloc::sync::Arc;

pub type SignalPendingState = unsafe extern "C" fn(state: i32, task: *mut c_void) -> bool;

static SIGNAL_PENDING_STATE: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Install the process owner's exact Linux `signal_pending_state()` policy.
/// Reinstalling the same provider is harmless; replacing it is rejected.
pub fn install_signal_pending_state(provider: SignalPendingState) -> Result<(), &'static str> {
    let candidate = provider as *mut c_void;
    match SIGNAL_PENDING_STATE.compare_exchange(
        ptr::null_mut(),
        candidate,
        Ordering::Release,
        Ordering::Acquire,
    ) {
        Ok(_) => Ok(()),
        Err(existing) if existing == candidate => Ok(()),
        Err(_) => Err("Linux signal predicate provider already installed"),
    }
}

/// Linux C ABI used by source-order i915 callers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal_pending_state(state: i32, task: *mut c_void) -> bool {
    let provider = SIGNAL_PENDING_STATE.load(Ordering::Acquire);
    assert!(
        !provider.is_null(),
        "Linux signal policy must be installed before upstream GT waits"
    );
    assert!(!task.is_null(), "signal_pending_state received a null task");
    let provider: SignalPendingState = unsafe { core::mem::transmute(provider) };
    unsafe { provider(state, task) }
}

/// Linux `signal_pending()` for translated interruptible execbuffer waits.
pub unsafe fn signal_pending(task: *mut c_void) -> bool {
    unsafe { signal_pending_state(crate::linux::wait::TASK_INTERRUPTIBLE as i32, task) }
}

/// Check the current task using its stable kernel task pointer for the duration
/// of the callback. The local Arc clone pins that task while the provider reads
/// its signal state.
pub fn signal_pending_current() -> bool {
    let task = axtask::current().clone();
    let task_ptr = Arc::as_ptr(&task).cast_mut().cast::<c_void>();
    unsafe { signal_pending(task_ptr) }
}
