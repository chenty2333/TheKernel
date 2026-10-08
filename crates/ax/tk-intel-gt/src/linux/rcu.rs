// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Preempt-disabled read-side sections and deferred RCU callbacks for the
//! Linux 7.2.3 i915 `call_rcu()` sites. Reclamation callbacks run on a task
//! after the current read-side population quiesces.

#![allow(unsafe_code)]

use core::sync::atomic::{AtomicUsize, Ordering};

use kernel_guard::BaseGuard;

use crate::intel_context_upstream::RcuHead;

static RCU_READERS: AtomicUsize = AtomicUsize::new(0);
static RCU_PENDING: AtomicUsize = AtomicUsize::new(0);

#[inline]
pub fn rcu_read_lock() {
    kernel_guard::NoPreempt::acquire();
    RCU_READERS.fetch_add(1, Ordering::Acquire);
}

#[inline]
pub fn rcu_read_unlock() {
    let old = RCU_READERS.fetch_sub(1, Ordering::Release);
    assert!(old != 0, "unbalanced rcu_read_unlock");
    kernel_guard::NoPreempt::release(());
}

/// Queue a Linux RCU reclamation callback. If task allocation is temporarily
/// unavailable, retain the object rather than invoke a destructor before a
/// grace period; the caller's reference remains leaked but safe.
pub fn call_rcu(head: *mut RcuHead, callback: unsafe fn(*mut RcuHead)) {
    if head.is_null() {
        return;
    }
    RCU_PENDING.fetch_add(1, Ordering::AcqRel);
    let head_address = head as usize;
    let result = axtask::spawn(move || {
        while RCU_READERS.load(Ordering::Acquire) != 0 {
            axtask::yield_now();
        }
        unsafe { callback(head_address as *mut RcuHead) };
        RCU_PENDING.fetch_sub(1, Ordering::Release);
    });
    if result.is_err() {
        // Fail closed: never reclaim without a proven grace period.
        RCU_PENDING.fetch_sub(1, Ordering::Release);
    }
}

pub fn rcu_barrier() {
    while RCU_PENDING.load(Ordering::Acquire) != 0 {
        axtask::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_count_tracks_nested_sections() {
        let before = RCU_READERS.load(Ordering::Acquire);
        rcu_read_lock();
        rcu_read_lock();
        assert_eq!(RCU_READERS.load(Ordering::Acquire), before + 2);
        rcu_read_unlock();
        rcu_read_unlock();
        assert_eq!(RCU_READERS.load(Ordering::Acquire), before);
    }
}
