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
    types::{WaitQueueEntry, WaitQueueHead},
    types::{ListHead, Spinlock},
    config::{CONFIG_HZ, MAX_SCHEDULE_TIMEOUT},
    list::{INIT_LIST_HEAD, list_add_tail, list_del_init, list_empty},
    locks::{spin_lock_irqsave, spin_unlock_irqrestore},
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
    let ptr = axhal::percpu::current_task_ptr::<()>() as usize;
    if ptr != 0 {
        ptr
    } else {
        1
    }
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
    let tick_ns = 1_000_000_000 / CONFIG_HZ as u64;
    let unbounded = timeout as u64 >= MAX_SCHEDULE_TIMEOUT;
    let nanos = if unbounded {
        u64::MAX
    } else {
        (timeout as u64).saturating_mul(tick_ns)
    };
    let wake_pending = || peek_task_state(task) == Some(TASK_RUNNING);
    let start = crate::clock::now_ns();
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
    let elapsed = crate::clock::now_ns().saturating_sub(start);
    (timeout - (elapsed / tick_ns.max(1)) as i64).max(0)
}

/// Linux `schedule_timeout_uninterruptible()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn schedule_timeout_uninterruptible(timeout: i64) -> i64 {
    set_task_state(current_task_key(), TASK_UNINTERRUPTIBLE);
    schedule_timeout(timeout)
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
    let start = crate::clock::now_ns();
    let deadline = start.saturating_add(timeout_ns);
    loop {
        if condition() {
            return false;
        }
        let now = crate::clock::now_ns();
        if now >= deadline {
            return true;
        }
        if yield_task && axtask::can_block_current() {
            axtask::yield_now();
        } else {
            crate::clock::relax();
        }
    }
}

pub fn wait_woken(entry: &mut WaitQueueEntry, _state: u32, timeout: i64) -> i64 {
    if timeout <= 0 {
        return 0;
    }
    let tick_ns = 1_000_000_000 / CONFIG_HZ as u64;
    let timeout_ns = if timeout as u64 >= MAX_SCHEDULE_TIMEOUT {
        u64::MAX
    } else {
        (timeout as u64).saturating_mul(tick_ns)
    };
    let start = crate::clock::now_ns();
    let timed_out = wait_until(timeout_ns, || entry.flags & WQ_FLAG_WOKEN != 0, true);
    if timed_out {
        return 0;
    }
    entry.flags &= !WQ_FLAG_WOKEN;
    let elapsed = crate::clock::now_ns().saturating_sub(start);
    timeout
        .saturating_sub((elapsed / tick_ns.max(1)) as i64)
        .max(1)
}

#[macro_export]
macro_rules! DEFINE_WAIT {
    () => {{
        $crate::types::WaitQueueEntry {
            flags: 0,
            private: core::ptr::null_mut(),
            func: Some($crate::wait::default_wake_function),
            entry: $crate::types::ListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
        }
    }};
}

#[macro_export]
macro_rules! DEFINE_WAIT_FUNC {
    ($function:ident) => {{
        $crate::types::WaitQueueEntry {
            flags: 0,
            private: core::ptr::null_mut(),
            func: Some($function),
            entry: $crate::types::ListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
        }
    }};
}

pub trait AsWaitQueueHeadPtr {
    fn as_wq_head_ptr(&mut self) -> *mut WaitQueueHead;
}

impl AsWaitQueueHeadPtr for WaitQueueHead {
    fn as_wq_head_ptr(&mut self) -> *mut WaitQueueHead {
        self as *mut _
    }
}

impl AsWaitQueueHeadPtr for *mut WaitQueueHead {
    fn as_wq_head_ptr(&mut self) -> *mut WaitQueueHead {
        *self
    }
}

impl AsWaitQueueHeadPtr for &mut WaitQueueHead {
    fn as_wq_head_ptr(&mut self) -> *mut WaitQueueHead {
        (*self) as *mut _
    }
}

#[macro_export]
macro_rules! wait_event_lock_irq {
    ($wait_queue:expr, $condition:expr, $lock:expr $(,)?) => {{
        while !$condition {
            unsafe { $crate::locks::spin_unlock_irq(&mut $lock) };
            axtask::yield_now();
            unsafe { $crate::locks::spin_lock_irq(&mut $lock) };
        }
    }};
}

/// Linux `wait_event_timeout()` macro: sleeps until condition evaluates to true
/// or the timeout in jiffies expires.
///
/// Returns:
/// - 0 if condition evaluated to false after timeout elapsed;
/// - 1 if condition evaluated to true after timeout elapsed;
/// - remaining jiffies (at least 1) if condition evaluated to true before timeout elapsed.
#[macro_export]
macro_rules! wait_event_timeout {
    ($wq:expr, $condition:expr, $timeout:expr $(,)?) => {{
        let mut __ret: i64 = $timeout as i64;
        $crate::wait::might_sleep();
        if !{
            let __cond = $condition;
            if __cond && __ret == 0 {
                __ret = 1;
            }
            __cond || __ret == 0
        } {
            let mut __entry = $crate::DEFINE_WAIT!();
            unsafe {
                $crate::wait::init_wait_entry(core::ptr::addr_of_mut!(__entry), 0);
            }
            let __wq_ptr = $crate::wait::AsWaitQueueHeadPtr::as_wq_head_ptr(&mut ($wq));
            loop {
                let _ = unsafe {
                    $crate::wait::prepare_to_wait_event(
                        __wq_ptr,
                        core::ptr::addr_of_mut!(__entry),
                        $crate::wait::TASK_UNINTERRUPTIBLE as i32,
                    )
                };
                let __cond = $condition;
                if __cond && __ret == 0 {
                    __ret = 1;
                }
                if __cond || __ret == 0 {
                    break;
                }
                __ret = $crate::wait::schedule_timeout(__ret);
                let __cond = $condition;
                if __cond && __ret == 0 {
                    __ret = 1;
                }
                if __cond || __ret == 0 {
                    break;
                }
            }
            unsafe {
                $crate::wait::finish_wait(
                    __wq_ptr,
                    core::ptr::addr_of_mut!(__entry),
                );
            }
        }
        __ret
    }};
}

/// Linux `wait_event_interruptible_timeout()` macro.
#[macro_export]
macro_rules! wait_event_interruptible_timeout {
    ($wq:expr, $condition:expr, $timeout:expr $(,)?) => {{
        let mut __ret: i64 = $timeout as i64;
        $crate::wait::might_sleep();
        if !{
            let __cond = $condition;
            if __cond && __ret == 0 {
                __ret = 1;
            }
            __cond || __ret == 0
        } {
            let mut __entry = $crate::DEFINE_WAIT!();
            unsafe {
                $crate::wait::init_wait_entry(core::ptr::addr_of_mut!(__entry), 0);
            }
            let __wq_ptr = $crate::wait::AsWaitQueueHeadPtr::as_wq_head_ptr(&mut ($wq));
            loop {
                let __int = unsafe {
                    $crate::wait::prepare_to_wait_event(
                        __wq_ptr,
                        core::ptr::addr_of_mut!(__entry),
                        $crate::wait::TASK_INTERRUPTIBLE as i32,
                    )
                };
                let __cond = $condition;
                if __cond && __ret == 0 {
                    __ret = 1;
                }
                if __cond || __ret == 0 {
                    break;
                }
                if __int != 0 {
                    __ret = __int;
                    break;
                }
                __ret = $crate::wait::schedule_timeout(__ret);
                let __cond = $condition;
                if __cond && __ret == 0 {
                    __ret = 1;
                }
                if __cond || __ret == 0 {
                    break;
                }
            }
            unsafe {
                $crate::wait::finish_wait(
                    __wq_ptr,
                    core::ptr::addr_of_mut!(__entry),
                );
            }
        }
        __ret
    }};
}

/// Linux `wait_event_interruptible()` macro.
#[macro_export]
macro_rules! wait_event_interruptible {
    ($wq:expr, $condition:expr $(,)?) => {{
        let __res = $crate::wait_event_interruptible_timeout!(
            $wq,
            $condition,
            $crate::config::MAX_SCHEDULE_TIMEOUT as i64,
        );
        if __res < 0 {
            __res as i32
        } else {
            0i32
        }
    }};
}

pub type PollQueueProc = Option<unsafe extern "C" fn(*mut crate::types::File, *mut WaitQueueHead, *mut PollTable)>;

#[repr(C)]
pub struct PollTable {
    pub qproc: PollQueueProc,
    pub key: u32,
}

/// Linux `poll_wait()`: registers a wait queue with a poll table.
#[inline]
pub unsafe fn poll_wait(
    filp: *mut crate::types::File,
    wait_address: *mut WaitQueueHead,
    p: *mut PollTable,
) {
    if !p.is_null() && !wait_address.is_null() {
        if let Some(qproc) = unsafe { (*p).qproc } {
            unsafe { qproc(filp, wait_address, p) };
        }
    }
}

/// Linux `wake_up_interruptible_poll()`.
#[inline]
pub fn wake_up_interruptible_poll(head: &mut WaitQueueHead, _key: u32) {
    wake_up_all(head);
}

/// Linux `wait_event_interruptible_locked()` macro.
#[macro_export]
macro_rules! wait_event_interruptible_locked {
    ($wq:expr, $condition:expr $(,)?) => {{
        if $condition {
            0i32
        } else {
            let mut __wait = $crate::DEFINE_WAIT!();
            unsafe {
                $crate::wait::init_wait_entry(core::ptr::addr_of_mut!(__wait), 0);
            }
            let __wq_ptr = $crate::wait::AsWaitQueueHeadPtr::as_wq_head_ptr(&mut ($wq));
            let mut __ret = 0i32;
            loop {
                __ret = unsafe {
                    $crate::wait::do_wait_intr(
                        __wq_ptr,
                        core::ptr::addr_of_mut!(__wait),
                    )
                };
                if __ret != 0 {
                    break;
                }
                if $condition {
                    break;
                }
            }
            unsafe {
                $crate::wait::__remove_wait_queue(
                    __wq_ptr,
                    core::ptr::addr_of_mut!(__wait),
                );
                $crate::wait::__set_current_state($crate::wait::TASK_RUNNING as i32);
            }
            __ret
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

/// Linux `need_resched()`: whether the current task has a pending reschedule.
/// The request is only read here; it is consumed by the scheduler at the next
/// preemption point, as in Linux.
#[unsafe(no_mangle)]
pub extern "C" fn need_resched() -> bool {
    axtask::current_need_resched()
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
    if !(*entry).entry.next.is_null() && !list_empty(unsafe { &(*entry).entry }) {
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
            crate::list::list_add(node, list);
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

// upstream: kernel/sched/wait.c init_wait_entry()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn init_wait_entry(
    wq_entry: *mut WaitQueueEntry,
    flags: i32,
) {
    if !wq_entry.is_null() {
        unsafe {
            (*wq_entry).flags = flags as u32;
            (*wq_entry).private = current_task_key() as *mut c_void;
            (*wq_entry).func = Some(default_wake_function);
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*wq_entry).entry));
        }
    }
}

// upstream: kernel/sched/wait.c prepare_to_wait_event()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn prepare_to_wait_event(
    wq_head: *mut WaitQueueHead,
    wq_entry: *mut WaitQueueEntry,
    state: i32,
) -> i64 {
    assert!(!wq_head.is_null() && !wq_entry.is_null());
    let mut flags = 0u64;
    unsafe { spin_lock_irqsave(&mut (*wq_head).lock, &mut flags) };
    let mut ret = 0i64;
    unsafe {
        if (*wq_entry).private.is_null() {
            (*wq_entry).private = current_task_key() as *mut c_void;
        }
        if (*wq_entry).func.is_none() {
            (*wq_entry).func = Some(default_wake_function);
        }
    }
    let pending = if (state as u32 & (TASK_INTERRUPTIBLE | TASK_UNINTERRUPTIBLE)) == TASK_INTERRUPTIBLE {
        crate::signal::signal_pending_current()
    } else {
        false
    };
    if pending {
        unsafe { list_del_init(core::ptr::addr_of_mut!((*wq_entry).entry)) };
        ret = -crate::config::ERESTARTSYS as i64;
    } else {
        unsafe {
            if (*wq_entry).entry.next.is_null() {
                INIT_LIST_HEAD(core::ptr::addr_of_mut!((*wq_entry).entry));
            }
            if list_empty(&(*wq_entry).entry) {
                if (*wq_entry).flags & WQ_FLAG_EXCLUSIVE != 0 {
                    list_add_tail(
                        core::ptr::addr_of_mut!((*wq_entry).entry),
                        core::ptr::addr_of_mut!((*wq_head).head),
                    );
                } else {
                    crate::list::list_add(
                        core::ptr::addr_of_mut!((*wq_entry).entry),
                        core::ptr::addr_of_mut!((*wq_head).head),
                    );
                }
            }
        }
        set_task_state(current_task_key(), state as u32);
    }
    unsafe { spin_unlock_irqrestore(&mut (*wq_head).lock, flags) };
    ret
}

// upstream: kernel/sched/wait.c do_wait_intr()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn do_wait_intr(
    wq: *mut WaitQueueHead,
    wait: *mut WaitQueueEntry,
) -> core::ffi::c_int {
    assert!(!wq.is_null() && !wait.is_null());
    unsafe {
        if (*wait).entry.next.is_null() {
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*wait).entry));
        }
        if list_empty(&(*wait).entry) {
            list_add_tail(
                core::ptr::addr_of_mut!((*wait).entry),
                core::ptr::addr_of_mut!((*wq).head),
            );
        }
    }
    set_task_state(current_task_key(), TASK_INTERRUPTIBLE);
    if crate::signal::signal_pending_current() {
        return -crate::config::ERESTARTSYS;
    }
    unsafe {
        crate::locks::spin_unlock(&mut (*wq).lock);
        if axtask::can_block_current() {
            axtask::yield_now();
        } else {
            core::hint::spin_loop();
        }
        crate::locks::spin_lock(&mut (*wq).lock);
    }
    0
}

// upstream: kernel/sched/wait.c do_wait_intr_irq()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn do_wait_intr_irq(
    wq: *mut WaitQueueHead,
    wait: *mut WaitQueueEntry,
) -> core::ffi::c_int {
    assert!(!wq.is_null() && !wait.is_null());
    unsafe {
        if (*wait).entry.next.is_null() {
            INIT_LIST_HEAD(core::ptr::addr_of_mut!((*wait).entry));
        }
        if list_empty(&(*wait).entry) {
            list_add_tail(
                core::ptr::addr_of_mut!((*wait).entry),
                core::ptr::addr_of_mut!((*wq).head),
            );
        }
    }
    set_task_state(current_task_key(), TASK_INTERRUPTIBLE);
    if crate::signal::signal_pending_current() {
        return -crate::config::ERESTARTSYS;
    }
    unsafe {
        crate::locks::spin_unlock_irq(&mut (*wq).lock);
        if axtask::can_block_current() {
            axtask::yield_now();
        } else {
            core::hint::spin_loop();
        }
        crate::locks::spin_lock_irq(&mut (*wq).lock);
    }
    0
}

// upstream: include/linux/wait.h __remove_wait_queue()
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __remove_wait_queue(
    _wq: *mut WaitQueueHead,
    wait: *mut WaitQueueEntry,
) {
    if !wait.is_null() {
        unsafe { list_del_init(core::ptr::addr_of_mut!((*wait).entry)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wait_event_timeout_contract() {
        let mut wq = WaitQueueHead {
            lock: crate::types::Spinlock::new(),
            head: crate::types::ListHead {
                next: core::ptr::null_mut(),
                prev: core::ptr::null_mut(),
            },
        };
        init_waitqueue_head(&mut wq);

        // 1. Condition already true: returns remaining jiffies (timeout >= 1)
        let ret = wait_event_timeout!(wq, true, 100);
        assert_eq!(ret, 100);

        // 2. Condition remains false across timeout: returns 0
        let ret = wait_event_timeout!(wq, false, 1);
        assert_eq!(ret, 0);

        // 3. Condition becomes true at timeout: returns 1
        let mut checked = false;
        let ret = wait_event_timeout!(wq, {
            if checked {
                true
            } else {
                checked = true;
                false
            }
        }, 1);
        assert_eq!(ret, 1);

        // 4. Interruptible timeout variant
        let ret_intr = wait_event_interruptible_timeout!(wq, true, 50);
        assert_eq!(ret_intr, 50);
        let ret_intr_to = wait_event_interruptible_timeout!(wq, false, 1);
        assert_eq!(ret_intr_to, 0);

        // 5. Interruptible locked variant
        let ret_locked = wait_event_interruptible_locked!(wq, true);
        assert_eq!(ret_locked, 0);
        let mut locked_checked = false;
        let ret_locked2 = wait_event_interruptible_locked!(wq, {
            if locked_checked {
                true
            } else {
                locked_checked = true;
                false
            }
        });
        assert_eq!(ret_locked2, 0);
    }
}

