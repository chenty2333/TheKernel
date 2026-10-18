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
    sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU8, AtomicUsize, Ordering},
    time::Duration,
};

use kernel_guard::{NoPreempt, NoPreemptIrqSave};

use crate::{
    fields::KtimeT,
    types::{HlistNode, Hrtimer, TimerList},
};

pub const HRTIMER_NORESTART: i32 = 0;
pub const HRTIMER_RESTART: i32 = 1;
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
/// this before a timer can become visible to interrupt context; once started,
/// it succeeds in any context.
pub fn init_timer_executor() -> Result<(), ()> {
    loop {
        match TIMER_WORKER_STATE.load(Ordering::Acquire) {
            // An established executor serves every later caller, including
            // timer arming under an IRQ-saved lock where blocking is illegal.
            TIMER_WORKER_READY => return Ok(()),
            TIMER_WORKER_STARTING => {
                TIMER_WAKE
                    .wait_until(|| {
                        TIMER_WORKER_STATE.load(Ordering::Acquire) != TIMER_WORKER_STARTING
                    })
                    .map_err(|_| ())?;
            }
            TIMER_WORKER_UNINITIALIZED => {
                // Only the first start needs a blockable task context.
                if !axtask::can_block_current() {
                    return Err(());
                }
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
        .saturating_mul(crate::config::CONFIG_HZ as u64)
        .saturating_add(999)
        / 1000;
    unsafe {
        mod_timer(
            timer,
            crate::primitives::jiffies().wrapping_add(ticks) as c_ulong,
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
        && !time_before(crate::primitives::jiffies(), unsafe {
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
                if !time_before(crate::primitives::jiffies(), expires) {
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

        let now = crate::primitives::jiffies();
        if time_before(now, expires) {
            let remaining_ticks = expires.wrapping_sub(now);
            let hz = crate::config::CONFIG_HZ as u64;
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
const NSEC_PER_JIFFY: u64 = 1_000_000_000 / crate::config::CONFIG_HZ as u64;

#[repr(C)]
struct HrtimerImpl {
    timer: TimerList,
    function: Option<unsafe extern "C" fn(*mut Hrtimer) -> i32>,
    expires_ns: AtomicI64,
    state_lock: AtomicBool,
}
const _: () = assert!(core::mem::size_of::<HrtimerImpl>() <= core::mem::size_of::<Hrtimer>());
const _: () = assert!(core::mem::align_of::<HrtimerImpl>() <= core::mem::align_of::<Hrtimer>());

struct HrtimerStateGuard<'a> {
    state_lock: &'a AtomicBool,
    _irq_guard: NoPreemptIrqSave,
}

impl Drop for HrtimerStateGuard<'_> {
    fn drop(&mut self) {
        self.state_lock.store(false, Ordering::Release);
    }
}

fn lock_hrtimer(state_lock: &AtomicBool) -> HrtimerStateGuard<'_> {
    let irq_guard = NoPreemptIrqSave::new();
    while state_lock
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        while state_lock.load(Ordering::Relaxed) {
            spin_loop();
        }
    }
    HrtimerStateGuard {
        state_lock,
        _irq_guard: irq_guard,
    }
}

fn monotonic_now_ns() -> i64 {
    i64::try_from(axhal::time::monotonic_time_nanos()).unwrap_or(i64::MAX)
}

/// Pure Linux `hrtimer_forward()` arithmetic. Returns the number of intervals
/// advanced (one when the prior deadline has arrived) and a deadline strictly
/// beyond `now_ns`; a future deadline and nonpositive intervals are unchanged.
fn forward_hrtimer_expiry(expires_ns: i64, now_ns: i64, interval_ns: i64) -> (u64, i64) {
    if interval_ns <= 0 || now_ns < expires_ns {
        return (0, expires_ns);
    }
    let delta = i128::from(now_ns) - i128::from(expires_ns);
    let interval = i128::from(interval_ns);
    let intervals = delta / interval + 1;
    let next_expiry = i128::from(expires_ns) + intervals * interval;
    let overruns = u64::try_from(intervals).unwrap_or(u64::MAX);
    let next_expiry = i64::try_from(next_expiry).unwrap_or(i64::MAX);
    (overruns, next_expiry)
}

/// Arm the timer-list entry of `h` for its absolute deadline. The caller holds
/// the hrtimer state lock so rearm and cancellation cannot publish stale expiry.
unsafe fn arm_hrtimer_locked(h: *mut HrtimerImpl) -> bool {
    // SAFETY: caller holds the hrtimer's initialized state lock.
    let expires_ns = unsafe { (*h).expires_ns.load(Ordering::Acquire) };
    let now_ns = monotonic_now_ns();
    let now_jiffies = crate::primitives::jiffies();
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
    let function = {
        // Timer-list deadlines are rounded up to a jiffy. If a concurrent
        // restart moved the deadline out, rearm without running the callback.
        let _state = lock_hrtimer(unsafe { &(*h).state_lock });
        if monotonic_now_ns() < unsafe { (*h).expires_ns.load(Ordering::Acquire) } {
            let _ = unsafe { arm_hrtimer_locked(h) };
            return;
        }
        unsafe { (*h).function }
    };
    if let Some(function) = function {
        // Linux invokes the callback outside the base lock. It may call
        // hrtimer_forward_now() or another timer API itself.
        let result = unsafe { function(h.cast::<Hrtimer>()) };
        if result == HRTIMER_RESTART {
            // Serialize callback-requested rearming with explicit starts and
            // cancellation. A concurrent try_to_cancel observes CALLBACK and
            // may legitimately return -1 while this timer is still running.
            let _state = lock_hrtimer(unsafe { &(*h).state_lock });
            let _ = unsafe { arm_hrtimer_locked(h) };
        }
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
    assert!(!h.is_null());
    unsafe {
        // `hrtimer_setup()` is the first access to this opaque storage; write
        // each atomic before publishing the timer to the shared timer worker.
        ptr::addr_of_mut!((*h).expires_ns).write(AtomicI64::new(0));
        ptr::addr_of_mut!((*h).state_lock).write(AtomicBool::new(false));
        (*h).function = function;
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
    assert!(!h.is_null());
    let _state = lock_hrtimer(unsafe { &(*h).state_lock });
    let now_ns = monotonic_now_ns();
    unsafe {
        (*h).expires_ns.store(
            if mode == HRTIMER_MODE_ABS_VALUE {
                time
            } else {
                now_ns.saturating_add(time)
            },
            Ordering::Release,
        );
        let _ = arm_hrtimer_locked(h);
    }
}

/// Linux `hrtimer_active()`: true while an hrtimer is queued or its callback
/// is running.
///
/// # Safety
/// `timer` must point to a live, initialized `Hrtimer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_active(timer: *const Hrtimer) -> bool {
    assert!(!timer.is_null());
    unsafe { timer_active(timer.cast::<TimerList>()) }
}

/// Linux `hrtimer_forward_now()`: advance a periodic deadline past the current
/// monotonic time and return the number of intervals advanced.
///
/// # Safety
/// `timer` must point to a live, initialized `Hrtimer` and be used according
/// to the hrtimer callback/locking contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_forward_now(timer: *mut Hrtimer, interval: KtimeT) -> u64 {
    assert!(!timer.is_null());
    let h = timer.cast::<HrtimerImpl>();
    let _state = lock_hrtimer(unsafe { &(*h).state_lock });
    // SAFETY: the lock serializes deadline updates with start and rearm.
    let expires_ns = unsafe { (*h).expires_ns.load(Ordering::Acquire) };
    let (overruns, next_expiry) = forward_hrtimer_expiry(expires_ns, monotonic_now_ns(), interval);
    if next_expiry != expires_ns {
        // SAFETY: as above; `h` remains live for the caller's callback.
        unsafe { (*h).expires_ns.store(next_expiry, Ordering::Release) };
    }
    overruns
}

/// Linux `hrtimer_get_expires()`: read the timer's absolute monotonic deadline.
///
/// # Safety
/// `timer` must point to a live, initialized `Hrtimer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_get_expires(timer: *const Hrtimer) -> KtimeT {
    assert!(!timer.is_null());
    let h = timer.cast::<HrtimerImpl>();
    let _state = lock_hrtimer(unsafe { &(*h).state_lock });
    // SAFETY: the lock serializes this read with all expiry writers.
    unsafe { (*h).expires_ns.load(Ordering::Acquire) }
}

/// Linux `hrtimer_try_to_cancel()`: 1 when a pending timer was cancelled, 0
/// when it was not armed, -1 when its callback is running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_try_to_cancel(timer: *mut Hrtimer) -> i32 {
    assert!(!timer.is_null());
    let hrtimer = timer.cast::<HrtimerImpl>();
    let _state = lock_hrtimer(unsafe { &(*hrtimer).state_lock });
    let timer_list = timer.cast::<TimerList>();
    // An in-flight callback remains active even if it has already queued its
    // restart deadline; Linux reports -1 until that callback finishes.
    if TIMER_ACTIVE.load(Ordering::Acquire) == timer_list {
        -1
    } else if unsafe { del_timer(timer_list) } {
        1
    } else if TIMER_ACTIVE.load(Ordering::Acquire) == timer_list {
        -1
    } else {
        0
    }
}

/// Linux `hrtimer_cancel()`: cancel and wait for a running callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn hrtimer_cancel(timer: *mut Hrtimer) -> i32 {
    assert!(!timer.is_null());
    if !axtask::can_block_current() {
        panic!("hrtimer_cancel requires task context");
    }
    loop {
        let result = unsafe { hrtimer_try_to_cancel(timer) };
        if result != -1 {
            return result;
        }
        let timer_list = timer.cast::<TimerList>();
        TIMER_STATE_WAKE
            .wait_until(|| TIMER_ACTIVE.load(Ordering::Acquire) != timer_list)
            .expect("hrtimer cancellation lost its task wait context");
    }
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
    let hz = crate::config::CONFIG_HZ as u64;
    let now = crate::primitives::jiffies();
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
    let now = crate::primitives::jiffies();
    round_jiffies_up_abs(now.wrapping_add(j as u64)).wrapping_sub(now) as c_ulong
}

#[cfg(test)]
mod hrtimer_tests {
    use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

    use super::*;

    const TEST_INTERVAL_NS: i64 = 60 * axhal::time::NANOS_PER_SEC as i64;

    static CALLBACKS: AtomicUsize = AtomicUsize::new(0);
    static ACTIVE_IN_CALLBACK: AtomicBool = AtomicBool::new(false);
    static CANCEL_IN_CALLBACK: AtomicI32 = AtomicI32::new(0);
    static LAST_FORWARD: AtomicU64 = AtomicU64::new(0);
    static FORWARD_LEFT_FUTURE: AtomicBool = AtomicBool::new(false);

    struct TestHrtimer(alloc::boxed::Box<Hrtimer>);

    impl TestHrtimer {
        fn new(function: unsafe extern "C" fn(*mut Hrtimer) -> i32) -> Self {
            let mut timer = alloc::boxed::Box::new(unsafe { core::mem::zeroed::<Hrtimer>() });
            // SAFETY: the boxed record has a stable address and setup initializes it before use.
            unsafe {
                hrtimer_setup(
                    &mut *timer,
                    Some(function),
                    CLOCK_MONOTONIC,
                    HRTIMER_MODE_REL,
                )
            };
            Self(timer)
        }

        fn as_ptr(&self) -> *const Hrtimer {
            &*self.0
        }

        fn as_mut_ptr(&mut self) -> *mut Hrtimer {
            &mut *self.0
        }

        fn start_relative(&mut self, time: KtimeT) {
            // SAFETY: this guard owns a live timer set up by `new`.
            unsafe { hrtimer_start_range_ns(self.as_mut_ptr(), time, 0, HRTIMER_MODE_REL) };
        }

        fn try_to_cancel(&mut self) -> i32 {
            // SAFETY: this guard owns a live timer set up by `new`.
            unsafe { hrtimer_try_to_cancel(self.as_mut_ptr()) }
        }

        /// Model the timer worker's dispatch boundary without relying on a
        /// host timer tick: the worker has removed this deadline and published
        /// TIMER_ACTIVE, then invokes the production hrtimer trampoline.
        fn dispatch_expired(&mut self, was_pending: bool) {
            let timer = self.as_mut_ptr().cast::<TimerList>();
            if was_pending {
                assert!(unsafe { TIMER_QUEUE.lock().remove(timer) });
            }

            // Force the opaque deadline to the current instant. The callback
            // may forward it, and the production trampoline must enqueue the
            // restarted deadline through mod_timer().
            let h = timer.cast::<HrtimerImpl>();
            unsafe { (*h).expires_ns.store(monotonic_now_ns(), Ordering::Release) };
            assert!(
                TIMER_ACTIVE
                    .compare_exchange(ptr::null_mut(), timer, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            );
            // SAFETY: `timer` is a live, initialized hrtimer and the fixture
            // models the worker-owned active callback interval.
            unsafe { hrtimer_timer_fn(timer) };
            assert_eq!(TIMER_ACTIVE.swap(ptr::null_mut(), Ordering::AcqRel), timer);
            TIMER_STATE_WAKE.notify_all(false);
        }
    }

    impl Drop for TestHrtimer {
        fn drop(&mut self) {
            // Cancel and wait before the boxed storage is released. The test's
            // scheduler guard is declared first and therefore remains live.
            unsafe { hrtimer_cancel(self.as_mut_ptr()) };
        }
    }

    unsafe extern "C" fn periodic_callback(timer: *mut Hrtimer) -> i32 {
        ACTIVE_IN_CALLBACK.store(unsafe { hrtimer_active(timer) }, Ordering::Release);
        CANCEL_IN_CALLBACK.store(unsafe { hrtimer_try_to_cancel(timer) }, Ordering::Release);
        let before_forward = monotonic_now_ns();
        let overruns = unsafe { hrtimer_forward_now(timer, TEST_INTERVAL_NS) };
        LAST_FORWARD.store(overruns, Ordering::Release);
        FORWARD_LEFT_FUTURE.store(
            unsafe { hrtimer_get_expires(timer) } > before_forward,
            Ordering::Release,
        );
        if CALLBACKS.fetch_add(1, Ordering::AcqRel) < 2 {
            HRTIMER_RESTART
        } else {
            HRTIMER_NORESTART
        }
    }

    #[test]
    fn forward_expiry_handles_future_deadline_and_overruns() {
        assert_eq!(forward_hrtimer_expiry(100, 99, 10), (0, 100));
        assert_eq!(forward_hrtimer_expiry(100, 100, 10), (1, 110));
        assert_eq!(forward_hrtimer_expiry(100, 135, 10), (4, 140));
        assert_eq!(forward_hrtimer_expiry(100, 135, 0), (0, 100));
    }

    #[test]
    fn restart_rearms_periodically_and_norestart_stops() {
        let _scheduler = crate::test_support::scheduler_context();
        CALLBACKS.store(0, Ordering::Release);
        ACTIVE_IN_CALLBACK.store(false, Ordering::Release);
        CANCEL_IN_CALLBACK.store(0, Ordering::Release);
        LAST_FORWARD.store(0, Ordering::Release);
        FORWARD_LEFT_FUTURE.store(false, Ordering::Release);

        // A far-future timer is active while queued and can be canceled before
        // the callback becomes running.
        let mut queued = TestHrtimer::new(periodic_callback);
        let before = monotonic_now_ns();
        let delay = 60 * axhal::time::NANOS_PER_SEC as i64;
        queued.start_relative(delay);
        let expiry = unsafe { hrtimer_get_expires(queued.as_ptr()) };
        let after = monotonic_now_ns();
        assert!(expiry >= before + delay && expiry <= after + delay);
        assert!(unsafe { hrtimer_active(queued.as_ptr()) });
        assert_eq!(queued.try_to_cancel(), 1);
        assert!(!unsafe { hrtimer_active(queued.as_ptr()) });

        let mut periodic = TestHrtimer::new(periodic_callback);
        // Dispatch three synthetic expirations. The first two callbacks
        // return RESTART and must produce a real queued timer-list deadline;
        // the third returns NORESTART and must leave the timer inactive.
        periodic.dispatch_expired(false);
        assert!(unsafe { timer_pending(periodic.as_mut_ptr().cast::<TimerList>()) });
        assert!(unsafe { hrtimer_active(periodic.as_ptr()) });
        periodic.dispatch_expired(true);
        assert!(unsafe { timer_pending(periodic.as_mut_ptr().cast::<TimerList>()) });
        periodic.dispatch_expired(true);
        assert!(!unsafe { timer_pending(periodic.as_mut_ptr().cast::<TimerList>()) });
        assert!(!unsafe { hrtimer_active(periodic.as_ptr()) });

        assert_eq!(CALLBACKS.load(Ordering::Acquire), 3);
        assert!(ACTIVE_IN_CALLBACK.load(Ordering::Acquire));
        assert_eq!(CANCEL_IN_CALLBACK.load(Ordering::Acquire), -1);
        assert!(LAST_FORWARD.load(Ordering::Acquire) >= 1);
        assert!(FORWARD_LEFT_FUTURE.load(Ordering::Acquire));
    }
}
