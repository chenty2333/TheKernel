// SPDX-License-Identifier: MIT
// Copyright 2026 TheKernel contributors. See ../../LICENSE-MIT.
//! Monotonic Linux timer-list adapter. A persistent axtask worker waits until
//! the earliest jiffies deadline, then runs the timer callback asynchronously
//! with preemption disabled. This is task-context timer dispatch, not a native
//! softirq/timer-wheel implementation; callbacks are never run inline from
//! `mod_timer()`.

#![allow(unsafe_code)]
#![allow(non_snake_case)]

use alloc::string::String;
use core::{
    ffi::c_ulong,
    fmt::Write as _,
    hint::spin_loop,
    ops::{Deref, DerefMut},
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering},
    time::Duration,
};

use kernel_guard::{NoPreempt, NoPreemptIrqSave};

use crate::{
    intel_context_upstream::Hrtimer,
    intel_engine_cs_upstream::{HlistNode, TimerList},
    linux::fields::KtimeT,
};

pub const HRTIMER_NORESTART: i32 = 0;
pub const HRTIMER_MODE_REL: i32 = 1;
pub const CLOCK_MONOTONIC: i32 = 1;
pub const NSEC_PER_USEC: u64 = 1_000;
pub const NSEC_PER_MSEC: u64 = 1_000_000;


/// Linux `ns_to_ktime()` for the target's scalar `ktime_t` representation.
#[inline]
pub const fn ns_to_ktime(ns: u64) -> KtimeT {
    ns as KtimeT
}

const TIMER_WORKER_UNINITIALIZED: u8 = 0;
const TIMER_WORKER_STARTING: u8 = 1;
const TIMER_WORKER_READY: u8 = 2;

const _: [(); core::mem::size_of::<usize>()] = [(); core::mem::size_of::<c_ulong>()];

/// Pointer/reference forms used by the source-order timer APIs.
pub trait TimerListPtr {
    fn timer_list_ptr(self) -> *mut TimerList;
}
impl TimerListPtr for *mut TimerList {
    fn timer_list_ptr(self) -> *mut TimerList {
        self
    }
}
impl TimerListPtr for *const TimerList {
    fn timer_list_ptr(self) -> *mut TimerList {
        self.cast_mut()
    }
}
impl TimerListPtr for &mut TimerList {
    fn timer_list_ptr(self) -> *mut TimerList {
        self
    }
}
impl TimerListPtr for &TimerList {
    fn timer_list_ptr(self) -> *mut TimerList {
        self as *const TimerList as *mut TimerList
    }
}

struct TimerListQueue {
    head: *mut HlistNode,
}
impl TimerListQueue {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
        }
    }

    /// Insert an unqueued timer. Caller owns TIMER_QUEUE.
    unsafe fn insert(&mut self, timer: *mut TimerList) {
        let node = unsafe { ptr::addr_of_mut!((*timer).entry) };
        assert!(unsafe { (*node).pprev.is_null() });
        unsafe {
            (*node).next = self.head;
            (*node).pprev = ptr::addr_of_mut!(self.head);
            if !self.head.is_null() {
                (*self.head).pprev = ptr::addr_of_mut!((*node).next);
            }
        }
        self.head = node;
    }

    /// Remove a pending timer. Returns false when it is not queued.
    unsafe fn remove(&mut self, timer: *mut TimerList) -> bool {
        let node = unsafe { ptr::addr_of_mut!((*timer).entry) };
        let previous = unsafe { (*node).pprev };
        if previous.is_null() {
            return false;
        }
        let next = unsafe { (*node).next };
        unsafe {
            *previous = next;
            if !next.is_null() {
                (*next).pprev = previous;
            }
            (*node).next = ptr::null_mut();
            (*node).pprev = ptr::null_mut();
        }
        true
    }

    fn earliest(&self) -> *mut TimerList {
        let mut cursor = self.head;
        let mut earliest: *mut TimerList = ptr::null_mut();
        while !cursor.is_null() {
            // SAFETY: every queued node is an embedded TimerList.entry.
            let timer = unsafe {
                cursor
                    .cast::<u8>()
                    .sub(core::mem::offset_of!(TimerList, entry))
                    .cast::<TimerList>()
            };
            if earliest.is_null()
                || time_before(unsafe { (*timer).expires as u64 }, unsafe {
                    (*earliest).expires as u64
                })
            {
                earliest = timer;
            }
            cursor = unsafe { (*cursor).next };
        }
        earliest
    }
}
unsafe impl Send for TimerListQueue {}

struct IrqSpinLock<T> {
    held: AtomicBool,
    data: core::cell::UnsafeCell<T>,
}
unsafe impl<T: Send> Send for IrqSpinLock<T> {}
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}
impl<T> IrqSpinLock<T> {
    const fn new(data: T) -> Self {
        Self {
            held: AtomicBool::new(false),
            data: core::cell::UnsafeCell::new(data),
        }
    }
    fn lock(&self) -> IrqSpinGuard<'_, T> {
        let irq_guard = NoPreemptIrqSave::new();
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.held.load(Ordering::Relaxed) {
                spin_loop();
            }
        }
        IrqSpinGuard {
            lock: self,
            _irq_guard: irq_guard,
        }
    }
}
struct IrqSpinGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    _irq_guard: NoPreemptIrqSave,
}
impl<T> Deref for IrqSpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}
impl<T> DerefMut for IrqSpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}
impl<T> Drop for IrqSpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

static TIMER_QUEUE: IrqSpinLock<TimerListQueue> = IrqSpinLock::new(TimerListQueue::new());
static TIMER_ACTIVE: AtomicPtr<TimerList> = AtomicPtr::new(ptr::null_mut());
static TIMER_WORKER_STATE: AtomicU8 = AtomicU8::new(TIMER_WORKER_UNINITIALIZED);
static TIMER_GENERATION: AtomicUsize = AtomicUsize::new(0);
static TIMER_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();
static TIMER_STATE_WAKE: axtask::WaitQueue = axtask::WaitQueue::new();

fn notify_timer_worker() {
    TIMER_GENERATION.fetch_add(1, Ordering::Release);
    TIMER_WAKE.notify_all(false);
}

fn time_before(left: u64, right: u64) -> bool {
    (left.wrapping_sub(right) as i64) < 0
}

fn timer_worker_name() -> Result<String, ()> {
    let mut name = String::new();
    name.try_reserve_exact(16).map_err(|_| ())?;
    name.push_str("i915-timer");
    name.write_fmt(format_args!("-{}", 0)).map_err(|_| ())?;
    Ok(name)
}

/// Start the monotonic timer-list worker in task context. Timer setup calls
/// this before a timer can become visible to interrupt context.
pub fn init_timer_executor() -> Result<(), ()> {
    if !axtask::can_block_current() {
        return Err(());
    }
    loop {
        match TIMER_WORKER_STATE.load(Ordering::Acquire) {
            TIMER_WORKER_READY => return Ok(()),
            TIMER_WORKER_STARTING => {
                TIMER_WAKE
                    .wait_until(|| {
                        TIMER_WORKER_STATE.load(Ordering::Acquire) != TIMER_WORKER_STARTING
                    })
                    .map_err(|_| ())?;
            }
            TIMER_WORKER_UNINITIALIZED => {
                if TIMER_WORKER_STATE
                    .compare_exchange(
                        TIMER_WORKER_UNINITIALIZED,
                        TIMER_WORKER_STARTING,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }
                let name = match timer_worker_name() {
                    Ok(name) => name,
                    Err(()) => {
                        TIMER_WORKER_STATE.store(TIMER_WORKER_UNINITIALIZED, Ordering::Release);
                        TIMER_WAKE.notify_all(false);
                        return Err(());
                    }
                };
                let worker = match axtask::try_spawn_with_name(timer_worker, name) {
                    Ok(worker) => worker,
                    Err(_) => {
                        TIMER_WORKER_STATE.store(TIMER_WORKER_UNINITIALIZED, Ordering::Release);
                        TIMER_WAKE.notify_all(false);
                        return Err(());
                    }
                };
                core::mem::forget(worker);
                TIMER_WORKER_STATE.store(TIMER_WORKER_READY, Ordering::Release);
                TIMER_WAKE.notify_all(false);
                return Ok(());
            }
            _ => unreachable!("invalid timer worker state"),
        }
    }
}

/// Linux `timer_setup()` for the source `timer_list` layout.
pub unsafe fn timer_setup<T: TimerListPtr>(
    timer: T,
    callback: unsafe extern "C" fn(*mut TimerList),
    flags: u32,
) {
    init_timer_executor().expect("failed to bind Linux timer-list executor");
    let timer = timer.timer_list_ptr();
    assert!(!timer.is_null());
    let mut queue = TIMER_QUEUE.lock();
    assert!(unsafe { (*timer).entry.pprev.is_null() });
    let _ = unsafe { queue.remove(timer) };
    unsafe {
        (*timer).entry.next = ptr::null_mut();
        (*timer).entry.pprev = ptr::null_mut();
        (*timer).expires = 0;
        (*timer).function = Some(callback);
        (*timer).flags = flags;
    }
}

/// Insert or update a timer at the absolute Linux jiffies deadline.
pub unsafe fn mod_timer<T: TimerListPtr>(timer: T, expires: c_ulong) -> bool {
    let timer = timer.timer_list_ptr();
    assert!(!timer.is_null());
    assert_eq!(
        TIMER_WORKER_STATE.load(Ordering::Acquire),
        TIMER_WORKER_READY,
        "timer scheduled before executor setup"
    );
    assert!(
        unsafe { (*timer).function }.is_some(),
        "uninitialized Linux timer"
    );
    let mut queue = TIMER_QUEUE.lock();
    let was_pending = unsafe { queue.remove(timer) };
    unsafe {
        (*timer).expires = expires;
    }
    unsafe {
        queue.insert(timer);
    }
    drop(queue);
    notify_timer_worker();
    was_pending
}

/// Schedule `timer` after a relative millisecond delay.
pub unsafe fn set_timer_ms<T: TimerListPtr>(timer: T, delay_ms: u64) -> bool {
    let ticks = delay_ms
        .saturating_mul(crate::linux_config::CONFIG_HZ as u64)
        .saturating_add(999)
        / 1000;
    unsafe {
        mod_timer(
            timer,
            crate::linux::primitives::jiffies().wrapping_add(ticks) as c_ulong,
        )
    }
}

/// Remove a pending timer without waiting for a callback already running.
pub unsafe fn del_timer<T: TimerListPtr>(timer: T) -> bool {
    let timer = timer.timer_list_ptr();
    assert!(!timer.is_null());
    let removed = unsafe { TIMER_QUEUE.lock().remove(timer) };
    if removed {
        notify_timer_worker();
    }
    removed
}

/// Synchronously delete a timer, including a callback currently in progress.
pub unsafe fn del_timer_sync<T: TimerListPtr>(timer: T) -> bool {
    let timer = timer.timer_list_ptr();
    assert!(!timer.is_null());
    if !axtask::can_block_current() {
        panic!("del_timer_sync requires task context");
    }
    let mut was_pending = false;
    loop {
        was_pending |= unsafe { TIMER_QUEUE.lock().remove(timer) };
        notify_timer_worker();
        if TIMER_ACTIVE.load(Ordering::Acquire) == timer {
            TIMER_STATE_WAKE
                .wait_until(|| TIMER_ACTIVE.load(Ordering::Acquire) != timer)
                .expect("timer cancellation lost its task wait context");
            continue;
        }
        break;
    }
    was_pending
}

/// Source-name used by the imported Execlists code for synchronous deletion.
pub unsafe fn cancel_timer<T: TimerListPtr>(timer: T) {
    let _ = unsafe { del_timer_sync(timer) };
}

pub unsafe fn timer_pending<T: TimerListPtr>(timer: T) -> bool {
    let timer = timer.timer_list_ptr();
    assert!(!timer.is_null());
    let _queue = TIMER_QUEUE.lock();
    unsafe { !(*timer).entry.pprev.is_null() }
}

/// Whether the timer is queued or its callback is currently executing.
pub unsafe fn timer_active<T: TimerListPtr>(timer: T) -> bool {
    let timer = timer.timer_list_ptr();
    assert!(!timer.is_null());
    (unsafe { timer_pending(timer) }) || TIMER_ACTIVE.load(Ordering::Acquire) == timer
}

/// Whether an active timer has reached its stored jiffies deadline.
pub unsafe fn timer_expired<T: TimerListPtr>(timer: T) -> bool {
    let timer = timer.timer_list_ptr();
    assert!(!timer.is_null());
    let _queue = TIMER_QUEUE.lock();
    let pending = unsafe { !(*timer).entry.pprev.is_null() };
    let active = TIMER_ACTIVE.load(Ordering::Acquire) == timer;
    (pending || active)
        && !time_before(crate::linux::primitives::jiffies(), unsafe {
            (*timer).expires as u64
        })
}

/// Linux 7.2.3 source spelling for non-synchronous timer deletion.
pub unsafe fn timer_delete<T: TimerListPtr>(timer: T) -> bool {
    unsafe { del_timer(timer) }
}

/// Linux 7.2.3 source spelling for synchronous timer deletion.
pub unsafe fn timer_delete_sync<T: TimerListPtr>(timer: T) -> bool {
    unsafe { del_timer_sync(timer) }
}

fn timer_worker() {
    TIMER_WAKE
        .wait_until(|| TIMER_WORKER_STATE.load(Ordering::Acquire) == TIMER_WORKER_READY)
        .expect("timer worker could not wait for executor publication");
    loop {
        let generation = TIMER_GENERATION.load(Ordering::Acquire);
        let (timer, expires) = {
            let mut queue = TIMER_QUEUE.lock();
            let timer = queue.earliest();
            if timer.is_null() {
                (timer, 0)
            } else {
                let expires = unsafe { (*timer).expires as u64 };
                if !time_before(crate::linux::primitives::jiffies(), expires) {
                    let _ = unsafe { queue.remove(timer) };
                    assert!(
                        TIMER_ACTIVE
                            .compare_exchange(
                                ptr::null_mut(),
                                timer,
                                Ordering::AcqRel,
                                Ordering::Acquire
                            )
                            .is_ok()
                    );
                }
                (timer, expires)
            }
        };
        if timer.is_null() {
            if TIMER_GENERATION.load(Ordering::Acquire) == generation {
                TIMER_WAKE
                    .wait_until(|| TIMER_GENERATION.load(Ordering::Acquire) != generation)
                    .expect("timer worker lost its task wait context");
            }
            continue;
        }
        if TIMER_ACTIVE.load(Ordering::Acquire) == timer {
            let callback = unsafe { (*timer).function }.expect("timer has no callback");
            {
                let _atomic_timer_context = NoPreempt::new();
                unsafe { callback(timer) };
            }
            TIMER_ACTIVE.store(ptr::null_mut(), Ordering::Release);
            TIMER_STATE_WAKE.notify_all(false);
            continue;
        }

        let now = crate::linux::primitives::jiffies();
        if time_before(now, expires) {
            let remaining_ticks = expires.wrapping_sub(now);
            let hz = crate::linux_config::CONFIG_HZ as u64;
            let nanos = remaining_ticks.saturating_mul(axhal::time::NANOS_PER_SEC as u64) / hz;
            TIMER_WAKE
                .wait_timeout_until(Duration::from_nanos(nanos.max(1)), || {
                    TIMER_GENERATION.load(Ordering::Acquire) != generation
                })
                .expect("timer worker could not register its deadline");
        }
    }
}

// ---------------------------------------------------------------------------
// High-resolution timers and jiffies rounding.
//
// An hrtimer is an embedded timer-list entry plus its callback and absolute
// monotonic deadline. The timer-list worker dispatches it at the first jiffy
// whose tick covers the deadline; the trampoline re-arms when the deadline has
// not been reached yet, so the callback never runs early. Resolution is one
// tick (CONFIG_HZ), which is coarser than Linux's hrtimer clock.
// ---------------------------------------------------------------------------

const HRTIMER_MODE_ABS_VALUE: i32 = 0;
const NSEC_PER_JIFFY: u64 = 1_000_000_000 / crate::linux_config::CONFIG_HZ as u64;

#[repr(C)]
struct HrtimerImpl {
    timer: TimerList,
    function: Option<unsafe extern "C" fn(*mut Hrtimer) -> i32>,
    expires_ns: i64,
    _reserved: u64,
}
const _: () = assert!(core::mem::size_of::<HrtimerImpl>() <= core::mem::size_of::<Hrtimer>());
const _: () = assert!(core::mem::align_of::<HrtimerImpl>() <= core::mem::align_of::<Hrtimer>());

/// Arm the timer-list entry of `h` for its absolute deadline.
unsafe fn arm_hrtimer(h: *mut HrtimerImpl) -> bool {
    let expires_ns = unsafe { (*h).expires_ns };
    let now_ns = axhal::time::monotonic_time_nanos() as i64;
    let now_jiffies = crate::linux::primitives::jiffies();
    let target = if expires_ns <= now_ns {
        now_jiffies
    } else {
        let delta = (expires_ns - now_ns) as u64;
        now_jiffies.wrapping_add(delta.div_ceil(NSEC_PER_JIFFY))
    };
    unsafe { mod_timer(h.cast::<TimerList>(), target as c_ulong) }
}

unsafe extern "C" fn hrtimer_timer_fn(timer: *mut TimerList) {
    let h = timer.cast::<HrtimerImpl>();
    let now_ns = axhal::time::monotonic_time_nanos() as i64;
    if now_ns < unsafe { (*h).expires_ns } {
        let _ = unsafe { arm_hrtimer(h) };
        return;
    }
    if let Some(function) = unsafe { (*h).function } {
        let _ = unsafe { function(h.cast::<Hrtimer>()) };
    }
}

/// Linux `hrtimer_setup()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_setup(
    timer: *mut Hrtimer,
    function: Option<unsafe extern "C" fn(*mut Hrtimer) -> i32>,
    _clock_id: i32,
    _mode: i32,
) {
    let h = timer.cast::<HrtimerImpl>();
    unsafe {
        (*h).function = function;
        (*h).expires_ns = 0;
        timer_setup(h.cast::<TimerList>(), hrtimer_timer_fn, 0);
    }
}

/// Linux `hrtimer_start_range_ns()`. `range_ns` is accepted for the slack
/// window; the timer fires at the earliest deadline, which is within it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_start_range_ns(
    timer: *mut Hrtimer,
    time: KtimeT,
    _range_ns: u64,
    mode: i32,
) {
    let h = timer.cast::<HrtimerImpl>();
    let now_ns = axhal::time::monotonic_time_nanos() as i64;
    unsafe {
        (*h).expires_ns = if mode == HRTIMER_MODE_ABS_VALUE {
            time
        } else {
            now_ns.saturating_add(time)
        };
        let _ = arm_hrtimer(h);
    }
}

/// Linux `hrtimer_try_to_cancel()`: 1 when a pending timer was cancelled, 0
/// when it was not armed, -1 when its callback is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_try_to_cancel(timer: *mut Hrtimer) -> i32 {
    let h = timer.cast::<TimerList>();
    if unsafe { del_timer(h) } {
        1
    } else if TIMER_ACTIVE.load(Ordering::Acquire) == h {
        -1
    } else {
        0
    }
}

/// Linux `hrtimer_cancel()`: cancel and wait for a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_cancel(timer: *mut Hrtimer) -> i32 {
    unsafe { del_timer_sync(timer.cast::<TimerList>()) as i32 }
}

/// Linux `timer_shutdown_sync()`: synchronous deletion. Returns whether the
/// timer was pending. Re-arming after shutdown is not rejected here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn timer_shutdown_sync(timer: *mut TimerList) -> i32 {
    unsafe { del_timer_sync(timer) as i32 }
}

/// Linux `round_jiffies_common()` for CPU 0 with `force_up` semantics: round
/// `j` up to the next whole second when it is still in the future.
fn round_jiffies_up_abs(j: u64) -> u64 {
    let hz = crate::linux_config::CONFIG_HZ as u64;
    let now = crate::linux::primitives::jiffies();
    let rem = j % hz;
    let rounded = j - rem + hz;
    if (now.wrapping_sub(rounded) as i64) < 0 {
        rounded
    } else {
        j
    }
}

/// Linux `round_jiffies_up()`.
#[unsafe(no_mangle)]
pub extern "C" fn round_jiffies_up(j: c_ulong) -> c_ulong {
    round_jiffies_up_abs(j as u64) as c_ulong
}

/// Linux `round_jiffies_up_relative()`.
#[unsafe(no_mangle)]
pub extern "C" fn round_jiffies_up_relative(j: c_ulong) -> c_ulong {
    let now = crate::linux::primitives::jiffies();
    round_jiffies_up_abs(now.wrapping_add(j as u64)).wrapping_sub(now) as c_ulong
}
