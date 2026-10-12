// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Host-test recording state for the TEMPORARY event and kthread stubs.
//! The stubs record their calls here so the host tests can check delivery
//! order and work queueing; they do not execute any work. Not a C-ABI owner.

use core::sync::atomic::{AtomicU64, AtomicUsize};
#[cfg(test)]
use core::sync::atomic::Ordering;

/// Maximum number of recorded calls kept by the recording stubs.
pub(crate) const RECORD_SLOTS: usize = 64;

pub(crate) static DELIVERED_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(crate) static DELIVERED: [AtomicU64; RECORD_SLOTS] = [const { AtomicU64::new(0) }; RECORD_SLOTS];
pub(crate) static QUEUED_COUNT: AtomicUsize = AtomicUsize::new(0);
pub(crate) static QUEUED: [AtomicU64; RECORD_SLOTS] = [const { AtomicU64::new(0) }; RECORD_SLOTS];

/// Clears the recording buffers; used by host tests before each scenario.
#[cfg(test)]
pub fn pending_stubs_reset_records() {
    DELIVERED_COUNT.store(0, Ordering::SeqCst);
    QUEUED_COUNT.store(0, Ordering::SeqCst);
}

/// Pointers of the events delivered through `drm_send_event_timestamp_locked`, in order.
#[cfg(test)]
pub fn pending_stubs_delivered_events() -> std::vec::Vec<usize> {
    let n = DELIVERED_COUNT.load(Ordering::SeqCst).min(RECORD_SLOTS);
    (0..n).map(|i| DELIVERED[i].load(Ordering::SeqCst) as usize).collect()
}

/// Pointers of the works queued through `kthread_queue_work`, in order.
#[cfg(test)]
pub fn pending_stubs_queued_works() -> std::vec::Vec<usize> {
    let n = QUEUED_COUNT.load(Ordering::SeqCst).min(RECORD_SLOTS);
    (0..n).map(|i| QUEUED[i].load(Ordering::SeqCst) as usize).collect()
}
