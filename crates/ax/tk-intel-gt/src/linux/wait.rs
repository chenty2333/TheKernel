// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Wait-queue and polling support used by the imported i915 GT code.

#![allow(unsafe_code)]

use core::{ffi::c_void, mem::offset_of};

use crate::{
    intel_context_upstream::{WaitQueueEntry, WaitQueueHead},
    intel_engine_cs_upstream::{ListHead, Spinlock},
    linux_config::{CONFIG_HZ, MAX_SCHEDULE_TIMEOUT},
    linux_list::{INIT_LIST_HEAD, list_add_tail, list_del_init, list_empty},
    linux_locks::{spin_lock_irqsave, spin_unlock_irqrestore},
};

/// `might_sleep()` is compiled out for this Linux 7.2.3 target because
/// CONFIG_DEBUG_ATOMIC_SLEEP is disabled.
#[inline]
pub fn might_sleep() {}

pub const TASK_INTERRUPTIBLE: u32 = 1;
pub const TASK_UNINTERRUPTIBLE: u32 = 2;
pub const TASK_RUNNING: u32 = 0;
pub const WQ_FLAG_EXCLUSIVE: u32 = 1;
pub const WQ_FLAG_WOKEN: u32 = 2;
pub const WQ_FLAG_CUSTOM: u32 = 4;
pub const WQ_FLAG_DONE: u32 = 8;
pub const WQ_FLAG_PRIORITY: u32 = 16;

/// Linux `cond_resched()` voluntary scheduling point.
#[inline]
pub fn cond_resched() {
    axtask::resched_if_needed();
}

/// Linux `schedule_timeout()` for the non-interruptible i915 wait paths.
/// This task backend exposes timed sleep rather than Linux task-state
/// scheduling; a completed timed wait therefore returns zero remaining ticks.
pub fn schedule_timeout(timeout: i64) -> i64 {
    if timeout <= 0 {
        return 0;
    }
    let ticks = timeout as u64;
    let nanos = ticks
        .saturating_mul(axhal::time::NANOS_PER_SEC as u64 / CONFIG_HZ as u64);
    if axtask::sleep(core::time::Duration::from_nanos(nanos)).is_err() {
        wait_until(nanos, || false, true);
    }
    0
}

/// Linux `msleep()` backed by the task timer; if called outside a sleepable
/// task context, retain the requested minimum delay via cooperative polling.
pub fn msleep(milliseconds: u32) {
    if milliseconds == 0 {
        return;
    }
    let nanos = u64::from(milliseconds).saturating_mul(1_000_000);
    if axtask::sleep(core::time::Duration::from_nanos(nanos)).is_err() {
        wait_until(nanos, || false, true);
    }
}

pub type WaitQueueFunc = unsafe extern "C" fn(*mut WaitQueueEntry, u32, i32, *mut c_void) -> i32;

pub fn init_waitqueue_head(head: &mut WaitQueueHead) {
    unsafe { INIT_LIST_HEAD(core::ptr::addr_of_mut!(head.head)) };
}

pub fn add_wait_queue(head: &mut WaitQueueHead, entry: &mut WaitQueueEntry) {
    let mut flags = 0u64;
    unsafe { spin_lock_irqsave(&mut head.lock, &mut flags) };
    unsafe {
        list_add_tail(
            core::ptr::addr_of_mut!(entry.entry),
            core::ptr::addr_of_mut!(head.head),
        )
    };
    unsafe { spin_unlock_irqrestore(&mut head.lock, flags) };
}

pub fn remove_wait_queue(head: &mut WaitQueueHead, entry: &mut WaitQueueEntry) {
    let mut flags = 0u64;
    unsafe { spin_lock_irqsave(&mut head.lock, &mut flags) };
    unsafe { list_del_init(core::ptr::addr_of_mut!(entry.entry)) };
    unsafe { spin_unlock_irqrestore(&mut head.lock, flags) };
}

pub fn wake_up(head: &mut WaitQueueHead) {
    wake_up_all(head);
}

pub fn wake_up_all(head: &mut WaitQueueHead) {
    let mut flags = 0u64;
    unsafe { spin_lock_irqsave(&mut head.lock, &mut flags) };
    let mut node = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(head.head.next)) };
    let sentinel = core::ptr::addr_of_mut!(head.head);
    while !core::ptr::eq(node, sentinel) {
        let entry = node
            .cast::<u8>()
            .wrapping_sub(offset_of!(WaitQueueEntry, entry))
            .cast::<WaitQueueEntry>();
        let next = unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*node).next)) };
        if let Some(wake) = unsafe { (*entry).func } {
            unsafe { wake(entry, u32::MAX, 0, core::ptr::null_mut()) };
        }
        node = next;
    }
    unsafe { spin_unlock_irqrestore(&mut head.lock, flags) };
}

pub unsafe extern "C" fn default_wake_function(
    entry: *mut WaitQueueEntry,
    _mode: u32,
    _sync: i32,
    _key: *mut c_void,
) -> i32 {
    unsafe { woken_wake_function(entry, _mode, _sync, _key) }
}

pub unsafe extern "C" fn woken_wake_function(
    entry: *mut WaitQueueEntry,
    _mode: u32,
    _sync: i32,
    _key: *mut c_void,
) -> i32 {
    if entry.is_null() {
        return 0;
    }
    unsafe {
        (*entry).flags |= WQ_FLAG_WOKEN;
    }
    1
}

/// Wait until a condition becomes true or its monotonic deadline expires.
/// Return true on timeout, matching Linux `wait_for()` call sites.
pub fn wait_until(timeout_ns: u64, mut condition: impl FnMut() -> bool, yield_task: bool) -> bool {
    let start = axhal::time::monotonic_time_nanos();
    let deadline = start.saturating_add(timeout_ns);
    loop {
        if condition() {
            return false;
        }
        if axhal::time::monotonic_time_nanos() >= deadline {
            return true;
        }
        if yield_task {
            axtask::yield_now();
        } else {
            core::hint::spin_loop();
        }
    }
}

pub fn wait_woken(entry: &mut WaitQueueEntry, _state: u32, timeout: i64) -> i64 {
    if timeout <= 0 {
        return 0;
    }
    let timeout_ns = if timeout as u64 >= MAX_SCHEDULE_TIMEOUT {
        u64::MAX
    } else {
        (timeout as u64).saturating_mul(axhal::time::NANOS_PER_SEC as u64 / CONFIG_HZ as u64)
    };
    let start = axhal::time::monotonic_time_nanos();
    let timed_out = wait_until(timeout_ns, || entry.flags & WQ_FLAG_WOKEN != 0, true);
    if timed_out {
        return 0;
    }
    entry.flags &= !WQ_FLAG_WOKEN;
    let elapsed = axhal::time::monotonic_time_nanos().saturating_sub(start);
    let tick_ns = axhal::time::NANOS_PER_SEC as u64 / CONFIG_HZ as u64;
    timeout
        .saturating_sub((elapsed / tick_ns.max(1)) as i64)
        .max(1)
}

macro_rules! DEFINE_WAIT {
    () => {{
        $crate::intel_context_upstream::WaitQueueEntry {
            flags: 0,
            private: core::ptr::null_mut(),
            func: Some($crate::linux_wait::default_wake_function),
            entry: $crate::intel_engine_cs_upstream::ListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
        }
    }};
}

macro_rules! DEFINE_WAIT_FUNC {
    ($function:ident) => {{
        $crate::intel_context_upstream::WaitQueueEntry {
            flags: 0,
            private: core::ptr::null_mut(),
            func: Some($function),
            entry: $crate::intel_engine_cs_upstream::ListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
        }
    }};
}

macro_rules! wait_for {
    ($condition:expr, $timeout_ms:expr $(,)?) => {{
        $crate::linux_wait::wait_until(
            ($timeout_ms as u64).saturating_mul(1_000_000),
            || $condition,
            true,
        )
    }};
}

macro_rules! wait_for_atomic_us {
    ($condition:expr, $timeout_us:expr $(,)?) => {{
        $crate::linux_wait::wait_until(
            ($timeout_us as u64).saturating_mul(1_000),
            || $condition,
            false,
        )
    }};
}

macro_rules! wait_event_lock_irq {
    ($wait_queue:expr, $condition:expr, $lock:expr $(,)?) => {{
        while !$condition {
            unsafe { $crate::linux_locks::spin_unlock_irq(&mut $lock) };
            axtask::yield_now();
            unsafe { $crate::linux_locks::spin_lock_irq(&mut $lock) };
        }
    }};
}
