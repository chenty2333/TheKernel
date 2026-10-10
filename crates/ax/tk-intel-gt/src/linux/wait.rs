// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Wait-queue and polling support used by the imported i915 GT code.

#![allow(unsafe_code)]

use alloc::vec::Vec;
use core::{
    cell::UnsafeCell,
    ffi::c_void,
    mem::{MaybeUninit, offset_of},
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};

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

/// Linux task state consumed by [`schedule_timeout`]. `TASK_RUNNING` is the
/// absence of a sleep request; a wake that arrives after `set_current_state()`
/// turns the recorded state into `TASK_RUNNING`, so the following schedule
/// returns at once and no wakeup is lost.
static TASK_STATES: spin::Mutex<Vec<(usize, u32)>> = spin::Mutex::new(Vec::new());
static TASK_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();

#[inline]
fn current_task_key() -> usize {
    axhal::percpu::current_task_ptr::<()>() as usize
}

fn set_task_state(task: usize, state: u32) {
    let mut states = TASK_STATES.lock();
    let index = states.iter().position(|(t, _)| *t == task);
    match (index, state) {
        (Some(index), TASK_RUNNING) => {
            states.swap_remove(index);
        }
        (Some(index), _) => states[index].1 = state,
        (None, TASK_RUNNING) => {}
        (None, _) => states.push((task, state)),
    }
}

fn peek_task_state(task: usize) -> Option<u32> {
    TASK_STATES
        .lock()
        .iter()
        .find(|(t, _)| *t == task)
        .map(|(_, state)| *state)
}

/// Consume the recorded state of `task`, if any.
fn take_task_state(task: usize) -> Option<u32> {
    let mut states = TASK_STATES.lock();
    let index = states.iter().position(|(t, _)| *t == task)?;
    Some(states.swap_remove(index).1)
}

/// Linux `wake_up_process()`: makes a sleeping task runnable. Returns 1 when
/// the task was sleeping and is now woken, 0 when it was already running.
pub fn wake_up_task(task: usize) -> i32 {
    let mut states = TASK_STATES.lock();
    let woken = match states.iter_mut().find(|(t, _)| *t == task) {
        Some(slot) if slot.1 != TASK_RUNNING => {
            // Keep a RUNNING record so the pending schedule observes the wake.
            slot.1 = TASK_RUNNING;
            true
        }
        _ => false,
    };
    drop(states);
    if woken {
        TASK_WAKE.notify_all(false);
        1
    } else {
        0
    }
}

/// Linux `schedule_timeout()`. A task that recorded a sleeping state sleeps
/// until `wake_up_task()` or the timeout; a task without a recorded state keeps
/// the timed-sleep behaviour of the non-interruptible i915 wait paths. Returns
/// the remaining jiffies, or 0 when the full timeout elapsed.
pub fn schedule_timeout(timeout: i64) -> i64 {
    if timeout < 0 {
        return 0;
    }
    let task = current_task_key();
    let tick_ns = axhal::time::NANOS_PER_SEC as u64 / CONFIG_HZ as u64;
    let unbounded = timeout as u64 >= MAX_SCHEDULE_TIMEOUT;
    let nanos = if unbounded {
        u64::MAX
    } else {
        (timeout as u64).saturating_mul(tick_ns)
    };
    let wake_pending = || peek_task_state(task) == Some(TASK_RUNNING);
    let start = axhal::time::monotonic_time_nanos();
    match peek_task_state(task) {
        Some(TASK_RUNNING) => {
            take_task_state(task);
            return timeout;
        }
        Some(_) => {
            // The entry stays in place while sleeping so wake_up_task() can
            // find it; a wake issued before this point left RUNNING behind.
            if axtask::can_block_current() {
                let _ = TASK_WAKE.wait_timeout_until(
                    Duration::from_nanos(nanos.min(i64::MAX as u64)),
                    wake_pending,
                );
            } else {
                wait_until(nanos, wake_pending, true);
            }
            take_task_state(task);
        }
        None => {
            if axtask::can_block_current() {
                let _ = axtask::sleep(Duration::from_nanos(nanos.min(i64::MAX as u64)));
            } else {
                wait_until(nanos, || false, true);
            }
        }
    }
    if unbounded {
        return 0;
    }
    let elapsed = axhal::time::monotonic_time_nanos().saturating_sub(start);
    (timeout - (elapsed / tick_ns.max(1)) as i64).max(0)
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
        // Linux default_wake_function(): wake the task that owns the entry.
        if !(*entry).private.is_null() {
            wake_up_task((*entry).private as usize);
        }
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

// ---------------------------------------------------------------------------
// C ABI entry points for the scheduler and wait-queue primitives.
// ---------------------------------------------------------------------------

/// Linux `set_current_state()`.
#[unsafe(no_mangle)]
pub extern "C" fn set_current_state(state: i32) {
    set_task_state(current_task_key(), state as u32);
}

/// Linux `__set_current_state()`: the same store without the barrier.
#[unsafe(no_mangle)]
pub extern "C" fn __set_current_state(state: i32) {
    set_task_state(current_task_key(), state as u32);
}

/// Linux `need_resched()`.
///
/// The scheduler's pending-reschedule flag is not exported to LinuxKPI. Callers
/// use this only as a preemption hint in polling loops that are bounded by
/// their own timeouts, so reporting no pending reschedule is conservative.
#[unsafe(no_mangle)]
pub extern "C" fn need_resched() -> bool {
    false
}

/// Linux `might_sleep()`: compiled out with CONFIG_DEBUG_ATOMIC_SLEEP off.
#[unsafe(export_name = "might_sleep")]
pub extern "C" fn c_might_sleep() {}

/// Linux `wake_up_process()`.
#[unsafe(no_mangle)]
pub extern "C" fn wake_up_process(task: *mut c_void) -> i32 {
    wake_up_task(task as usize)
}

/// Linux `schedule_timeout()` C entry point (the Rust helper keeps its name).
#[unsafe(no_mangle)]
pub extern "C" fn io_schedule_timeout(timeout: i64) -> i64 {
    schedule_timeout(timeout)
}

/// Linux `prepare_to_wait()`: queue `entry` on `wq` if it is not queued yet and
/// record the sleeping state of the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn prepare_to_wait(
    wq: *const WaitQueueHead,
    entry: *mut WaitQueueEntry,
    state: i32,
) {
    assert!(!wq.is_null() && !entry.is_null());
    unsafe {
        (*entry).flags &= !WQ_FLAG_EXCLUSIVE;
        if (*entry).private.is_null() {
            (*entry).private = current_task_key() as *mut c_void;
        }
        if list_empty(&(*entry).entry) {
            queue_entry_locked(wq.cast_mut(), entry, false);
        }
    }
    set_task_state(current_task_key(), state as u32);
}

/// Linux `finish_wait()`: restore TASK_RUNNING and dequeue `entry` if queued.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn finish_wait(wq: *const WaitQueueHead, entry: *mut WaitQueueEntry) {
    assert!(!wq.is_null() && !entry.is_null());
    set_task_state(current_task_key(), TASK_RUNNING);
    if !list_empty(unsafe { &(*entry).entry }) {
        let mut flags = 0u64;
        let head = wq.cast_mut();
        unsafe { spin_lock_irqsave(&mut (*head).lock, &mut flags) };
        unsafe { list_del_init(core::ptr::addr_of_mut!((*entry).entry)) };
        unsafe { spin_unlock_irqrestore(&mut (*head).lock, flags) };
    }
}

/// Linux `add_wait_queue()`: clears the exclusive flag and inserts at the head
/// of the list, as `__add_wait_queue()` does.
#[unsafe(export_name = "add_wait_queue")]
pub unsafe extern "C" fn c_add_wait_queue(wq: *mut WaitQueueHead, entry: *mut WaitQueueEntry) {
    unsafe {
        (*entry).flags &= !WQ_FLAG_EXCLUSIVE;
        queue_entry_locked(wq, entry, true);
    }
}

/// Insert `entry` under the wait-queue spinlock, at the head or the tail.
unsafe fn queue_entry_locked(wq: *mut WaitQueueHead, entry: *mut WaitQueueEntry, head: bool) {
    let mut flags = 0u64;
    unsafe { spin_lock_irqsave(&mut (*wq).lock, &mut flags) };
    unsafe {
        let list = core::ptr::addr_of_mut!((*wq).head);
        let node = core::ptr::addr_of_mut!((*entry).entry);
        if head {
            crate::linux_list::list_add(node, list);
        } else {
            list_add_tail(node, list);
        }
    }
    unsafe { spin_unlock_irqrestore(&mut (*wq).lock, flags) };
}

/// Wait-table size for `__var_waitqueue()` (Linux uses 256 buckets).
const VAR_WAIT_BUCKETS: usize = 64;

struct VarWaitSlot {
    /// 0 = uninitialised, 1 = being initialised, 2 = ready.
    state: AtomicU8,
    head: UnsafeCell<MaybeUninit<WaitQueueHead>>,
}

// SAFETY: the head is written once under the `state` handshake and afterwards
// accessed only through its own spinlock.
unsafe impl Sync for VarWaitSlot {}

static VAR_WAIT_TABLE: [VarWaitSlot; VAR_WAIT_BUCKETS] = [const {
    VarWaitSlot {
        state: AtomicU8::new(0),
        head: UnsafeCell::new(MaybeUninit::zeroed()),
    }
}; VAR_WAIT_BUCKETS];

/// Linux `__var_waitqueue()`: the wait queue hashed from an address.
#[unsafe(no_mangle)]
pub extern "C" fn __var_waitqueue(addr: *mut c_void) -> *mut WaitQueueHead {
    let hashed = (addr as usize >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58;
    let slot = &VAR_WAIT_TABLE[hashed % VAR_WAIT_BUCKETS];
    loop {
        match slot.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                let head = unsafe { (*slot.head.get()).as_mut_ptr() };
                init_waitqueue_head(unsafe { &mut *head });
                slot.state.store(2, Ordering::Release);
                break;
            }
            Err(2) => break,
            Err(_) => core::hint::spin_loop(),
        }
    }
    unsafe { (*slot.head.get()).as_mut_ptr() }
}

/// Linux `wake_up_var()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wake_up_var(addr: *mut c_void) {
    unsafe { wake_up_all(&mut *__var_waitqueue(addr)) };
}

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
        if crate::linux::requests::i915_active_is_idle(addr.cast_const().cast::<crate::intel_context_types_upstream::I915Active>()) {
            unsafe { finish_wait(wq, &mut entry) };
            return 0;
        }
        let _ = schedule_timeout(MAX_SCHEDULE_TIMEOUT as i64);
        unsafe { finish_wait(wq, &mut entry) };
    }
}
